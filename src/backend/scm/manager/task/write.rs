//! The write half of the task side: installing a task, changing a daemon between a service and a task,
//! and `enable` and `disable`.
//!
//! Everything a write needs is acquired before the first request to Task Scheduler: the account, the
//! password and the logon right of a real user, the event source, and the record directory with its
//! permissions. A failure of any is a failure with nothing sent.
//!
//! A task is armed when it has triggers; an install makes it disarmed and keeps what is installed. `disable`
//! registers the stored XML with no `Triggers`, and `enable` the stored XML as it is when it has triggers,
//! and with the ones the blob's schedule compiles to when it has none; everything outside `<Triggers>` is
//! the stored XML's, so a hand edit of it survives, and a hand-added trigger goes at `disable`, as it
//! does at the next install. Re-registering a task that logs on with a
//! password takes the password (M3): `enable` and `disable` of a real user's task read
//! `GOETIA_SERVICE_PASSWORD` as an install does.

use std::sync::Arc;

use super::api::{Registered, Registration};
use super::live::Liveness;
use super::verbs::{Processes, System, gone};
use super::{TaskPath, TaskSnapshot};
use crate::backend::scm::default_log::{self, Naming, Step};
use crate::backend::scm::folder::program_data;
use crate::backend::scm::generate::{self, ScmRegistration, WindowsDefinition};
use crate::backend::scm::instance::TaskAccount;
use crate::backend::scm::manager::identity::{self, Right, Written};
use crate::backend::scm::manager::{
    Scm, ScmSnapshot, accounts_kept_before_change, apply_gated, make_default_log, registered_account,
    registered_account_sid, registry, service_gate, written_user,
};
use crate::backend::scm::runs::Runs;
use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::sid::{Sid, sid_from_parts};
use crate::backend::scm::task_xml::{TaskXml, builtin_sid};
use crate::backend::{Identity, InstalledService, WriteOutcome};
use crate::error::{Error, Result};
use crate::manager::State;
use crate::spec::{Builtin, DaemonKind, DaemonSpec, Id};

/// What the machine holds for a task's account once an install has seen to it.
pub(super) struct Prepared {
    /// The account the task, and so its shim, runs as: the record directory names it.
    pub principal: Sid,
    /// What a registration logs on with: a real user's password.
    pub password: Option<String>,
}

/// The SID of one of the three well-known accounts a task can run as without a password
/// (`TaskAccount::Service`).
pub(super) fn well_known_sid(sid: &str) -> Result<Sid> {
    let subauthority = sid
        .strip_prefix("S-1-5-")
        .and_then(|rest| rest.parse::<u32>().ok())
        .filter(|subauthority| matches!(subauthority, 18..=20));
    match subauthority {
        Some(subauthority) => Ok(sid_from_parts(5, &[subauthority])),
        None => Err(Error::AccountLookup {
            detail: format!("`{sid}` is not SYSTEM, LocalService or NetworkService"),
            source: None,
        }),
    }
}

/// The password a registration of a task that runs as `account` logs on with: a real user's, read by
/// `read` (`GOETIA_SERVICE_PASSWORD`); never any for a service account, so that a value left over from
/// installing another daemon cannot reach a built-in account, which Windows refuses one for.
pub(super) fn password_of(
    account: &TaskAccount,
    read: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<String>> {
    match account {
        TaskAccount::Service(_) => Ok(None),
        TaskAccount::User(name) => match read()? {
            Some(password) => Ok(Some(password)),
            None => Err(Error::PasswordRequired {
                account: name.clone(),
                kind: DaemonKind::Task,
            }),
        },
    }
}

/// The password a registration of a task that runs as `account` needs, read as `enable` and `disable` read it.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn registration_password(account: &TaskAccount) -> Result<Option<String>> {
    password_of(account, identity::service_password)
}

/// The record directory of task `id` under the machine's `%ProgramData%`, with the folders above it
/// seen to ([`Runs::ensure_roots`]). The folders are always judged: whatever the manager's check of the
/// shim's folder is, a standard user can make them first, and no caller has said another way keeps
/// them safe.
fn runs_for_write(id: &Id) -> Result<Runs> {
    let runs = Runs::of(&program_data()?, id);
    runs.ensure_roots(&FolderCheck::new(CheckPermissions::Enable))?;
    Ok(runs)
}

/// What an install of a task needs of the machine before it writes anything. For a real user: the
/// account is refused unless it is a user (the right would reach every account in a group), the
/// password is read, and `SeBatchLogonRight` is granted to the SID that check found (without it a
/// first start starts nothing and raises nothing). For every account, the `Goetia` event source, which
/// the shim logs to and which only an elevated process can register.
pub(super) fn prepare(account: &TaskAccount, written: Option<Written<'_>>) -> Result<Prepared> {
    let (principal, password) = match account {
        TaskAccount::Service(sid) => (well_known_sid(sid)?, None),
        TaskAccount::User(name) => {
            let user = identity::require_user_account(name, written)?;
            let password = password_of(account, identity::service_password)?;
            identity::grant_logon_right(&user, Right::Batch)?;
            (user.sid().clone(), password)
        }
    };
    registry::register_event_source()?;
    Ok(Prepared { principal, password })
}

/// The account a stored task is registered as: the blob's when it reads, which is what the install
/// registered, and otherwise its principal's, so that a task whose blob will not decode can still be
/// enabled and disabled.
pub(super) fn stored_account(registered: &Registered) -> Result<TaskAccount> {
    match TaskXml::parse(&registered.xml, &registered.data) {
        Ok(definition) => Ok(definition.account()),
        Err(_) => TaskXml::principal(&registered.xml),
    }
}

/// Refuses a retype of a daemon that runs: `old` is what is installed, `state` its state.
pub(super) fn refuse_running(id: &Id, old: DaemonKind, state: State) -> Result<()> {
    match state {
        State::Running => Err(Error::RetypeRunning {
            id: id.to_string(),
            old,
        }),
        State::Idle | State::Stopped | State::Failed | State::Unknown => Ok(()),
    }
}

// Install =============================================================================================================

impl Scm {
    /// `spec` as an install writes it: the host the provider would place is previewed, and a path
    /// Task Scheduler cannot hold is refused.
    pub(in crate::backend::scm) fn task_render_for_install(&self, spec: &DaemonSpec) -> Result<WindowsDefinition> {
        generate::install_registration(spec, &Identity { user: String::new() }, &|| self.provider()?.preview())
    }

    /// Writes the task `spec` is: a create when nothing is installed, otherwise an update of
    /// `snapshot`'s task that keeps whether it is armed (a create is disarmed). A task installed over
    /// one of another account is registered again instead ([`Self::task_write_with`]). Write
    /// [`Self::task_write_with`] over the machine's own record directory, processes and account.
    pub(in crate::backend::scm) fn task_write(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        snapshot: Option<&TaskSnapshot>,
    ) -> Result<WriteOutcome> {
        let runs = runs_for_write(&spec.common().id)?;
        self.task_write_with(spec, definition, snapshot, &runs, &mut System, |account| {
            prepare(account, written_user(&spec.common().user))
        })
    }

    /// [`Self::task_write`] with the record directory `runs`, `processes` (which judge the old task's
    /// run) and `prepare`, which sees to what the machine must hold for the account. `prepare` comes
    /// before the first request to Task Scheduler. [`WriteOutcome::Raced`] when the task existed for a
    /// create or was gone for an update.
    ///
    /// An account is the same when the stored principal's SID is the new account's, and then the task
    /// is updated in place. Otherwise (a stored principal that cannot be told is a change) Task
    /// Scheduler would keep the security descriptor the task was registered with, which names the old
    /// account only and so leaves the task unreadable by the new account's shim. So the task is
    /// registered again ([`Self::task_register_again`]): disabled, refused while the old one runs (its
    /// shim still writes its record, and the permissions of the record directory are for the new
    /// account only), then, with the record directory made, the registration alone is deleted (its
    /// records stay) and created with the armed state it had. A failure between the delete and the
    /// create leaves nothing at the id, which an install that runs again creates (disarmed: `enable`
    /// arms it).
    pub(super) fn task_write_with<P: Processes>(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        snapshot: Option<&TaskSnapshot>,
        runs: &Runs,
        processes: &mut P,
        prepare: impl FnOnce(&TaskAccount) -> Result<Prepared>,
    ) -> Result<WriteOutcome> {
        let id = &spec.common().id;
        let prepared = prepare(&definition.account())?;
        let changed = snapshot.filter(|old| stored_principal(old.registered()).as_ref() != Some(&prepared.principal));
        match changed {
            None => {
                task_records(id, runs, &prepared, &[])?;
                self.task_register(
                    id,
                    definition,
                    snapshot.is_some_and(TaskSnapshot::armed),
                    snapshot.is_some(),
                    &prepared,
                )
            }
            Some(old) => self.task_register_again(old, definition, runs, processes, &prepared),
        }
    }

    /// The account change of [`Self::task_write_with`]. The task is disabled first (its own flag, as an
    /// uninstall does it, which needs no password): a scheduled fire between the check of its runs and the
    /// delete would start a run as the old account that nothing waits for. Then, in order, nothing runs
    /// ([`Error::AccountChangeRunning`]), the record directory is made, the task is deleted, and created
    /// with the armed state it had. A failure after the disable puts the flag back as it was found (a task
    /// found disabled, or whose flag was not read, is not touched), and a flag that cannot be put back is
    /// reported with the failure.
    fn task_register_again<P: Processes>(
        &self,
        old: &TaskSnapshot,
        definition: &TaskXml,
        runs: &Runs,
        processes: &mut P,
        prepared: &Prepared,
    ) -> Result<WriteOutcome> {
        let id = old.id();
        let path = TaskPath::of(id);
        // Task Scheduler's own flag, as it said it when the task was read: a flag it did not say, or that
        // is off, is left alone and so is never put back.
        let disabled = old.registered().enabled == Some(true)
            && match self.tasks.set_enabled(&path, false) {
                Ok(()) => true,
                // The task is gone: there is nothing to put back.
                Err(e) if gone(&e) => false,
                Err(e) => return Err(e),
            };
        match self.replace_task(old, definition, runs, processes, prepared) {
            Err(failed) if disabled => Err(match self.tasks.set_enabled(&path, true) {
                Ok(()) => failed,
                Err(e) if gone(&e) => failed,
                Err(e) => Error::both_all([
                    failed,
                    Error::Step {
                        what: format!("enable `{id}` again"),
                        source: Box::new(e),
                    },
                ])
                .expect("two errors"),
            }),
            written => written,
        }
    }

    /// [`Self::task_register_again`] from the check of the runs on.
    fn replace_task<P: Processes>(
        &self,
        old: &TaskSnapshot,
        definition: &TaskXml,
        runs: &Runs,
        processes: &mut P,
        prepared: &Prepared,
    ) -> Result<WriteOutcome> {
        let id = old.id();
        self.refuse_account_change_while_running(old, |pid| processes.alive(pid))?;
        // The task is registered as the old account until the create lands, so its log names it too.
        let registered = stored_principal(old.registered());
        task_records(id, runs, prepared, registered.as_slice())?;
        match self.tasks.delete(&TaskPath::of(id)) {
            // The task is gone; a folder that stays is still there for the create.
            Ok(_) | Err(Error::TaskFolderKept { .. }) => {}
            Err(e) => return Err(e),
        }
        let wrote = self.task_register(id, definition, old.armed(), false, prepared)?;
        landed(id, runs, prepared, wrote)
    }

    /// Refuses installing `old`, a task, as another account while it runs:
    /// [`Error::AccountChangeRunning`]. `alive` judges the processes the listing names.
    fn refuse_account_change_while_running(
        &self,
        old: &TaskSnapshot,
        alive: impl FnMut(u32) -> std::io::Result<Liveness>,
    ) -> Result<()> {
        match self.task_state_with(old, alive)? {
            State::Running => Err(Error::AccountChangeRunning {
                id: old.id().to_string(),
            }),
            State::Idle | State::Stopped | State::Failed | State::Unknown => Ok(()),
        }
    }

    /// The registration itself, once what it needs is acquired: the task as `armed` says, an update of
    /// the one there is or a create.
    fn task_register(
        &self,
        id: &Id,
        definition: &TaskXml,
        armed: bool,
        update: bool,
        prepared: &Prepared,
    ) -> Result<WriteOutcome> {
        let registered = self.tasks.register(
            &TaskPath::of(id),
            &definition.to_xml(armed),
            &definition.account(),
            prepared.password.as_deref(),
            update,
        )?;
        Ok(match registered {
            Registration::Created => WriteOutcome::Done,
            Registration::Raced => WriteOutcome::Raced,
        })
    }
}

/// The SID of the account a stored task runs as: its principal's, as Task Scheduler stores it. `None`
/// when it cannot be told (no principal, or a name that names no account), which is not known to be
/// the account being installed.
fn stored_principal(registered: &Registered) -> Option<Sid> {
    principal_sid(registered).ok()
}

/// The SID of the account a stored task runs as, or why it cannot be told.
fn principal_sid(registered: &Registered) -> Result<Sid> {
    let (TaskAccount::Service(text) | TaskAccount::User(text)) = TaskXml::principal(&registered.xml)?;
    identity::sid_of_principal(&text)
}

/// The record directory of `id` with permissions that name the account `prepared` holds. Nothing is read back.
fn run_records(id: &Id, runs: &Runs, prepared: &Prepared) -> Result<()> {
    runs.ensure(&prepared.principal)
        .map_err(|e| Error::os(format!("make the run records of `{id}` writable by its account"), e))
}

/// [`run_records`], and the default log of `id`, which the account may append to (the shim opens it and
/// never makes it) and so may `kept`: the accounts a write that changes the account leaves the daemon running
/// as until it lands ([`landed`] follows it). Nothing is read back.
fn task_records(id: &Id, runs: &Runs, prepared: &Prepared, kept: &[Sid]) -> Result<()> {
    run_records(id, runs, prepared)?;
    let accounts = [kept, std::slice::from_ref(&prepared.principal)].concat();
    default_log::ensure(runs.program_data(), id, &Naming::alike(&accounts)).map(drop)
}

/// Once a registration that changed the account, or a service into a task, has landed (`wrote` is
/// [`WriteOutcome::Done`]), the default log names the new account alone. Any other end leaves the log naming
/// both, which the next install corrects: it is the one that finds the task as it is.
fn landed(id: &Id, runs: &Runs, prepared: &Prepared, wrote: WriteOutcome) -> Result<WriteOutcome> {
    if matches!(wrote, WriteOutcome::Done) {
        default_log::ensure(
            runs.program_data(),
            id,
            &Naming::alike(std::slice::from_ref(&prepared.principal)),
        )?;
    }
    Ok(wrote)
}

/// The SID of the account of the service `old` that its default log names beside the task's, `task`, until the
/// retype lands: the one rule of a service's account change ([`accounts_kept_before_change`]). A native service has
/// no default log, a service whose account is gone has no one to name, and one that runs as the task's account has
/// no other; the system is asked of the account only for a hosted service that runs as another one.
fn kept_service_account(old: &ScmSnapshot, task: &TaskAccount) -> Result<Option<Sid>> {
    if old.blob().is_ok_and(|blob| !generate::hosted(&blob.spec)) {
        return Ok(None);
    }
    let config = old.config()?;
    accounts_kept_before_change(true, &registered_account(config), &service_account_name(task), || {
        registered_account_sid(config)
    })
}

/// The account `task` runs as, as the registration of a service that runs as it spells it: `None` is LocalSystem.
fn service_account_name(task: &TaskAccount) -> Option<String> {
    match task {
        TaskAccount::User(name) => generate::canonical_account(name),
        TaskAccount::Service(sid) => [Builtin::LocalSystem, Builtin::LocalService, Builtin::NetworkService]
            .into_iter()
            .find(|builtin| builtin_sid(*builtin).eq_ignore_ascii_case(sid))
            .map_or_else(|| Some(sid.clone()), |builtin| builtin.canonical().map(str::to_string)),
    }
}

/// The default log of the task `installed`, for the account it is registered as: what an install that finds
/// the task up to date makes again if it is gone, since the shim makes none.
pub(in crate::backend::scm) fn task_default_log(installed: &TaskSnapshot) -> Result<()> {
    let principal = principal_sid(installed.registered())?;
    default_log::ensure(&program_data()?, installed.id(), &Naming::alike(&[principal])).map(drop)
}

// Enable and disable ==================================================================================================

impl Scm {
    pub(in crate::backend::scm) fn task_enable(&self, snapshot: &TaskSnapshot) -> Result<()> {
        self.task_set_armed_with(snapshot, true, identity::service_password)
    }

    pub(in crate::backend::scm) fn task_disable(&self, snapshot: &TaskSnapshot) -> Result<()> {
        self.task_set_armed_with(snapshot, false, identity::service_password)
    }

    /// Registers `snapshot`'s task again armed or not, and nothing else changed: armed, with the
    /// triggers it holds, or when it holds none the ones its blob's schedule compiles to (a blob that
    /// will not decode names none, so it is the error); not armed, with none. Either way the task's
    /// own flag is true.
    /// `password` reads the real user's password, which is asked before anything is sent.
    pub(super) fn task_set_armed_with(
        &self,
        snapshot: &TaskSnapshot,
        armed: bool,
        password: impl FnOnce() -> Result<Option<String>>,
    ) -> Result<()> {
        let registered = snapshot.registered();
        let xml = if !armed {
            TaskXml::without_triggers(&registered.xml)?
        } else if TaskXml::has_triggers(&registered.xml) {
            TaskXml::with_own_flag(&registered.xml, true)?
        } else {
            let blob = snapshot.blob().map_err(|e| Error::Kept(Arc::clone(e)))?;
            TaskXml::with_triggers(&registered.xml, &blob.spec)?
        };
        let account = stored_account(registered)?;
        let password = password_of(&account, password)?;
        let path = TaskPath::of(snapshot.id());
        match self.tasks.register(&path, &xml, &account, password.as_deref(), true)? {
            Registration::Created => Ok(()),
            Registration::Raced => Err(Error::NotInstalled {
                id: snapshot.id().to_string(),
            }),
        }
    }
}

// Retype ==============================================================================================================

impl Scm {
    /// An install that changes a task into a service: the task is refused while it runs; everything the
    /// service needs is acquired; the task is removed; the service is created. A start the user makes
    /// between the check and the removal is the user's own action, and a crash between the removal and
    /// the creation leaves nothing at the id, which an install that runs again creates.
    ///
    /// The install lock is held from here to the end of the install, as the removal and the sweep after
    /// the creation need it: a service that runs the shim host holds it already, and a native one,
    /// whose install takes none, takes it here.
    pub(in crate::backend::scm) fn retype_to_service(
        &self,
        spec: &DaemonSpec,
        definition: &ScmRegistration,
        old: &TaskSnapshot,
        scope: &mut Option<InstallLock>,
    ) -> Result<WriteOutcome> {
        if scope.is_none() {
            *scope = Some(InstallLock::acquire()?);
        }
        let runs = Runs::of(&program_data()?, old.id());
        let written = written_user(&spec.common().user);
        let wrote = self.retype_to_service_with(
            old,
            &runs,
            &mut System,
            || service_gate(definition, written),
            |gated| {
                apply_gated(definition, gated.password, true)?;
                make_default_log(spec, definition, gated.checked.as_ref(), &[], Step::Narrow)?;
                Ok(WriteOutcome::Done)
            },
        )?;
        self.sweep_after_retype();
        Ok(wrote)
    }

    /// [`Self::retype_to_service`] over `processes` and the record directory `runs`, with `gate`, which
    /// acquires what the service needs and says what its creation takes, and `create`, which creates it.
    /// The order is the contract: refuse a task that runs, then `gate`, then the removal, then `create`;
    /// a failure of any step leaves the task as the steps before it left it, and a refusal or a failed
    /// `gate` leaves it untouched.
    pub(super) fn retype_to_service_with<P: Processes, G>(
        &self,
        old: &TaskSnapshot,
        runs: &Runs,
        processes: &mut P,
        gate: impl FnOnce() -> Result<G>,
        create: impl FnOnce(G) -> Result<WriteOutcome>,
    ) -> Result<WriteOutcome> {
        self.refuse_running_task_with(old, |pid| processes.alive(pid))?;
        let gated = gate()?;
        self.task_remove_with(old, runs, processes)?;
        create(gated)
    }

    /// Refuses a retype of `old`, a task, while it runs: [`Error::RetypeRunning`]. `alive` judges the
    /// processes the listing names.
    fn refuse_running_task_with(
        &self,
        old: &TaskSnapshot,
        alive: impl FnMut(u32) -> std::io::Result<Liveness>,
    ) -> Result<()> {
        refuse_running(old.id(), DaemonKind::Task, self.task_state_with(old, alive)?)
    }

    /// See [`Self::retype_to_service`]; the steps are [`Self::retype_to_task_with`]'s. The task's install holds the
    /// lock already.
    pub(in crate::backend::scm) fn retype_to_task(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        old: &ScmSnapshot,
        scope: &mut Option<InstallLock>,
    ) -> Result<WriteOutcome> {
        debug_assert!(scope.is_some(), "a task's install holds the install lock");
        let runs = runs_for_write(&spec.common().id)?;
        let wrote = self.retype_to_task_with(
            spec,
            definition,
            &runs,
            |account| prepare(account, written_user(&spec.common().user)),
            || {
                refuse_running(
                    old.id(),
                    DaemonKind::Service,
                    self.service_live_status(old)?.common().state,
                )?;
                kept_service_account(old, &definition.account())
            },
            || self.service_retire(old),
        )?;
        self.sweep_after_retype();
        Ok(wrote)
    }

    /// [`Self::retype_to_task`] over the record directory `runs`, with `prepare` (what the machine must hold for
    /// the task's account), `inspect` (refuses a service that runs, [`Error::RetypeRunning`], and gives the SID of
    /// the account it runs as, if a default log names it) and `retire` (the service is removed).
    ///
    /// The default log names the service's account and the task's before the service is retired, and the task's
    /// alone once the task is registered ([`landed`]): the service runs as its account until it is retired, so
    /// neither a failed retirement nor a failed or raced registration leaves the log unusable by what is
    /// registered, and the next install corrects what a failure left. `retire` returns once the service is
    /// stopped, a start in progress waited out first (the service is disabled, so none follows), so no shim of it
    /// can still be before opening its log when the log is narrowed. The order is the contract: the service is
    /// refused while it runs and its account is looked up (`inspect`; the lookup is skipped when it is the task's),
    /// then `prepare` and the record directory, then the log for both accounts, then `retire`, then the
    /// registration, then the log for the task's account alone. A refusal, a failed lookup, a failed preparation
    /// or a failed log leaves the service as it was.
    pub(super) fn retype_to_task_with(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        runs: &Runs,
        prepare: impl FnOnce(&TaskAccount) -> Result<Prepared>,
        inspect: impl FnOnce() -> Result<Option<Sid>>,
        retire: impl FnOnce() -> Result<()>,
    ) -> Result<WriteOutcome> {
        let id = &spec.common().id;
        let service = inspect()?;
        let prepared = prepare(&definition.account())?;
        task_records(id, runs, &prepared, service.as_slice())?;
        retire()?;
        let wrote = self.task_register(id, definition, false, false, &prepared)?;
        landed(id, runs, &prepared, wrote)
    }

    /// The sweep of what the removal left: the file a removed service's or task's host named, which
    /// the daemon now installed may or may not run. The removal sweeps nothing itself (it would delete
    /// the host the new daemon is about to name), so the install sweeps once the new daemon exists.
    /// Like an install's own sweep, it does not fail the install.
    fn sweep_after_retype(&self) {
        if let Some(shim) = &self.shim {
            self.sweep_before_install(&**shim);
        }
    }
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod write_tests;
