//! The write half of the task side: installing a task, changing a daemon between a service and a task,
//! and `enable` and `disable`.
//!
//! Everything a write needs is acquired before the first request to Task Scheduler: the account, the
//! password and the logon right of a real user, the event source, and the record directory with its
//! permissions. A failure of any is a failure with nothing sent.
//!
//! An account change checks what it can before it deletes the old task ([`Scm::task_register_again`]). What can
//! still fail after the delete (a permissions write, a link made at `runs\<id>` by the old account during the
//! change, the create) leaves nothing at the id, and the error says so.
//!
//! A task is armed when it has triggers; an install makes it disarmed and keeps what is installed. `disable`
//! registers the stored XML with no `Triggers`, and `enable` the stored XML as it is when it has triggers,
//! and with the ones the blob's schedule compiles to when it has none; everything outside `<Triggers>` is
//! the stored XML's, so a hand edit of it survives, and a hand-added trigger goes at `disable`.
//! Re-registering a task that logs on with a
//! password takes the password (M3): `enable` and `disable` of a real user's task read
//! `GOETIA_SERVICE_PASSWORD` as an install does.

use std::sync::Arc;

use std::path::Path;

use super::api::{Registered, Registration};
use super::live::Liveness;
use super::read::StoredRegistration;
use super::verbs::{Processes, System, gone};
use super::{TaskPath, TaskSnapshot};
use crate::backend::scm::default_log::{self, Naming, Step};
use crate::backend::scm::folder::program_data;
use crate::backend::scm::generate::{self, ScmRegistration, WindowsDefinition};
use crate::backend::scm::instance::TaskAccount;
use crate::backend::scm::manager::identity::{self, Right, Written};
use crate::backend::scm::manager::{
    Password, Scm, ScmSnapshot, accounts_kept_before_change, apply_gated, make_default_log, registered_account,
    registered_account_sid, registry, service_gate, written_user,
};
use crate::backend::scm::runs::{Runs, unfinished_change};
use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::sid::{Sid, distinct, sid_from_parts};
use crate::backend::scm::task_xml::{TaskXml, Token, builtin_sid};
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

/// As above, for the test helper.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn registration_password(account: &TaskAccount) -> Result<Option<String>> {
    password_of(account, identity::service_password)
}

/// The record directory of task `id` under `program_data`, with the folders above it seen to
/// ([`Runs::ensure_roots`], which `folders` judges).
fn runs_in(program_data: &Path, id: &Id, folders: &FolderCheck) -> Result<Runs> {
    let runs = Runs::of(program_data, id);
    runs.ensure_roots(folders)?;
    Ok(runs)
}

/// [`runs_in`] under the machine's `%ProgramData%`, with the folders always judged: whatever the manager's
/// check of the shim's folder is, a standard user can make them first, and no caller has said another way
/// keeps them safe.
fn runs_for_write(id: &Id) -> Result<Runs> {
    runs_in(&program_data()?, id, &FolderCheck::new(CheckPermissions::Enable))
}

/// What an install of a task needs of the machine before it writes anything. For a real user: the
/// account is refused unless it is a user (the right would reach every account in a group), the
/// password is read, and `SeBatchLogonRight` is granted to the SID that check found (without
/// it a first start starts nothing and raises nothing). For every account, the `Goetia` event source, which
/// the shim logs to and which only an elevated process can register.
pub(super) fn prepare(account: &TaskAccount, written: Option<Written<'_>>, reads: Password) -> Result<Prepared> {
    let (principal, password) = match account {
        TaskAccount::Service(sid) => (well_known_sid(sid)?, None),
        TaskAccount::User(name) => {
            let user = identity::require_user_account(name, written)?;
            let password = match reads {
                Password::Read => password_of(account, identity::service_password)?,
                Password::Unneeded => None,
            };
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
pub(crate) fn stored_account(registered: &Registered) -> Result<TaskAccount> {
    match TaskXml::parse(&registered.xml, &registered.data) {
        Ok(definition) => Ok(definition.account()),
        Err(_) => TaskXml::principal(&registered.xml)?.ok_or_else(|| Error::blob("the task's XML names no principal")),
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
    /// `snapshot`'s task, armed when it has triggers, whatever its own flag (a create is disarmed); every
    /// registration writes the flag true. Only a forced install or one that finds the blob stale reaches an
    /// update over a task whose own flag is off: the flag is one of the compared settings, so a plain install
    /// of it is a conflict. A task installed over one of another account is registered again instead
    /// ([`Self::task_write_with`]). Write [`Self::task_write_with`] over the machine's own record directory,
    /// processes and account.
    ///
    /// **A limit, stated:** a task whose `runs\<id>` directory is gone or unusable starts no program: its
    /// shim cannot pin the records, logs their path and ends as a shim failure. Any install makes the
    /// directory again.
    pub(in crate::backend::scm) fn task_write(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        snapshot: Option<&TaskSnapshot>,
    ) -> Result<WriteOutcome> {
        let runs = runs_for_write(&spec.common().id)?;
        self.task_write_with(spec, definition, snapshot, &runs, &mut System, |account| {
            prepare(account, written_user(&spec.common().user), Password::Read)
        })
    }

    /// [`Self::task_write`] with the record directory `runs`, `processes` (which judge the old task's
    /// run) and `prepare`, which sees to what the machine must hold for the account. `prepare` comes
    /// before the first request to Task Scheduler. [`WriteOutcome::Raced`] when the task existed for a
    /// create or was gone for an update.
    ///
    /// An account is the same when the stored principal's SID is the new account's, and then the task is
    /// updated in place under the registration's own token (see [`Self::task_register`]). When the account
    /// differs, or the stored XML names no principal or one that names no account (which is a change), an
    /// update in place would keep the security descriptor the task was registered with, which names the old
    /// account only and so leaves the task unreadable by the new account's shim. So the task is registered
    /// again ([`Self::task_register_again`]): disabled, refused while the old one runs or anything holds its
    /// records (its shim still writes its record), then the registration alone is deleted (its records stay,
    /// narrowed to the new account) and created, under a fresh token, armed as the old one's triggers say.
    ///
    /// A stored registration whose XML cannot be read at all, or whose account lookup failed for a reason other
    /// than "no such account", fails the write before anything is sent to Task Scheduler: that is not a change of
    /// account.
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
        // Read before `prepare` touches the machine.
        let stored = snapshot
            .map(|old| old.stored().map(|stored| (old, stored)))
            .transpose()?;
        let prepared = prepare(&definition.account())?;
        let registering = match stored {
            Some((old, stored)) if stored.principal.as_ref() != Some(&prepared.principal) => {
                return self.task_register_again(old, &stored, definition, runs, processes, &prepared);
            }
            Some((_, stored)) => Registering::Update {
                armed: stored.has_triggers,
                token: stored.token,
            },
            None => Registering::Create { armed: false },
        };
        task_records(id, runs, &prepared.principal, &[])?;
        self.task_register(id, definition, &prepared, registering)
    }

    /// The account change of [`Self::task_write_with`]. First the records and the default log are checked for
    /// what their creation would refuse ([`check_records`]), which depends on neither step below. A refusal
    /// sends nothing to Task Scheduler and places no marker; [`prepare`] has already granted the logon right
    /// and registered the event source. Then the task is disabled (its own flag, as an uninstall does it, which
    /// needs no password), whatever its flag was read as: a scheduled fire between the check of its runs and
    /// the delete would start a run as the old account that nothing waits for. Then, in order: the removal's
    /// marker is placed (no shim of the task pins the records from now on); the change is refused while a run
    /// is alive ([`Error::AccountChangeRunning`]) or the records are held ([`Error::AccountChangeHeld`]), the
    /// pin check sweeping the stale pins it finds; the registration is deleted; the records and the default
    /// log name the new account alone; and the task is created, armed as `stored` says: the old registration has
    /// a trigger that is switched on ([`StoredRegistration::has_triggers`]), whatever its own flag was.
    ///
    /// A refusal before the delete leaves the old task: the task's own flag is put back true, the marker (if it
    /// was placed) is taken away, and a flag or a marker that cannot be put back is reported with the failure.
    /// **After the delete** these can fail:
    /// - a permissions write;
    /// - a link made at `runs\<id>` by the old account during the change;
    /// - the create, which is also where Task Scheduler checks a real user's password.
    ///
    /// That leaves nothing at the id, which no flag is put back on: the error says the old task is removed and
    /// to install again (and, for one that was armed and whose new definition has a schedule, to enable it
    /// after, as an install makes it disarmed). The install that runs again clears the marker with its
    /// narrowing.
    ///
    /// **A scheduled run that falls in the window of the disable does not run.**
    fn task_register_again<P: Processes>(
        &self,
        old: &TaskSnapshot,
        stored: &StoredRegistration,
        definition: &TaskXml,
        runs: &Runs,
        processes: &mut P,
        prepared: &Prepared,
    ) -> Result<WriteOutcome> {
        let id = old.id();
        let path = TaskPath::of(id);
        check_records(id, runs)?;
        let disabled = match self.tasks.set_enabled(&path, false) {
            Ok(()) => true,
            Err(e) if gone(&e) => false,
            Err(e) => return Err(e),
        };
        if let Err(failed) = self.delete_for_account_change(old, runs, processes) {
            return Err(if disabled {
                self.enable_again(id, failed)
            } else {
                failed
            });
        }
        // No run of the old account can be left (it held a pin or was listed).
        let created = task_records(id, runs, &prepared.principal, &[]).and_then(|()| {
            let registering = Registering::Create {
                armed: stored.has_triggers,
            };
            self.task_register(id, definition, prepared, registering)
        });
        created.map_err(|e| Error::Step {
            what: removed_and_not_recreated(id, stored.has_triggers && definition.has_schedule()),
            source: Box::new(e),
        })
    }

    /// Puts the task's own flag back true after `failed`, which left the task: `failed` is the error, with
    /// the put-back's failure beside it. A task that is gone has no flag.
    fn enable_again(&self, id: &Id, failed: Error) -> Error {
        match self.tasks.set_enabled(&TaskPath::of(id), true) {
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
        }
    }

    /// Places the marker, refuses a run or held records ([`Self::refuse_account_change_while_running`]), and
    /// deletes the registration. A failure takes the marker away; one that cannot be taken away is reported with
    /// it.
    fn delete_for_account_change<P: Processes>(
        &self,
        old: &TaskSnapshot,
        runs: &Runs,
        processes: &mut P,
    ) -> Result<()> {
        runs.close()?;
        self.refuse_account_change_while_running(old, runs, |pid| processes.alive(pid))
            .and_then(|()| match self.tasks.delete(&TaskPath::of(old.id())) {
                // The task is gone; a folder that stays is still there for the create.
                Ok(_) | Err(Error::TaskFolderKept { .. }) => Ok(()),
                Err(e) => Err(e),
            })
            .map_err(|failed| match runs.reopen() {
                Ok(()) => failed,
                Err(e) => {
                    Error::both_all([failed, Error::os("take the removal's marker away", e)]).expect("two errors")
                }
            })
    }

    /// Refuses installing `old`, a task, as another account while it runs ([`Error::AccountChangeRunning`]) or
    /// its records are held by something that is no run of it ([`Error::AccountChangeHeld`]). One pass over the
    /// pins, and the listing, whose `alive` judges the processes it names; neither waits. A run holds its pin
    /// for its whole life, so a pin that is held is no reason to skip the listing: it is what tells a run from
    /// a shim that is starting, a `run` or `stop`, or another program. A pin that cannot be judged is the error.
    fn refuse_account_change_while_running(
        &self,
        old: &TaskSnapshot,
        runs: &Runs,
        alive: impl FnMut(u32) -> std::io::Result<Liveness>,
    ) -> Result<()> {
        let pin = runs.held_pin()?;
        let id = || old.id().to_string();
        match (self.task_state_with(old, alive)?, pin) {
            (State::Running, _) => Err(Error::AccountChangeRunning { id: id() }),
            (_, Some(pin)) => Err(Error::AccountChangeHeld { id: id(), pin }),
            (State::Idle | State::Stopped | State::Failed | State::Unknown, None) => Ok(()),
        }
    }

    /// The registration itself, once what it needs is acquired. The one place a token is chosen.
    fn task_register(
        &self,
        id: &Id,
        definition: &TaskXml,
        prepared: &Prepared,
        registering: Registering,
    ) -> Result<WriteOutcome> {
        let (armed, token, update) = match registering {
            Registering::Create { armed } => (armed, Token::fresh(), false),
            Registering::Update { armed, token } => (armed, token.unwrap_or_else(Token::fresh), true),
        };
        let registered = self.tasks.register(
            &TaskPath::of(id),
            &definition.to_xml(armed, &token),
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

/// What [`Scm::task_register`] registers.
enum Registering {
    /// A new registration of the task, armed or not, under a fresh token.
    Create { armed: bool },
    /// The stored task written again in place: armed when it has triggers (`armed`), under its own `token`; or,
    /// where it has none (a hand edit), a fresh one.
    Update { armed: bool, token: Option<Token> },
}

/// The record directory of `id` with permissions that name `accounts`. Nothing is read back.
fn run_records(id: &Id, runs: &Runs, accounts: &[Sid]) -> Result<()> {
    runs.ensure(accounts).map_err(|e| {
        Error::os(
            format!("make the run records of `{id}` writable by {}", whose(accounts)),
            e,
        )
    })
}

/// The error of an account change that fails after deleting the old task: it is gone, install again.
/// `enable_after`: the old task was armed and the new definition has a schedule, so say to `enable` it (an install
/// creates it disarmed).
fn removed_and_not_recreated(id: &Id, enable_after: bool) -> String {
    let enable = if enable_after {
        format!(", then run `goetia daemon enable {id}`, as the old task was armed")
    } else {
        String::new()
    };
    format!(
        "installing `{id}` as another account failed after its old task was already removed, so nothing is installed \
         at `{id}`: re-run install{enable}"
    )
}

/// "its account" or "its accounts", counting duplicates once.
fn whose(accounts: &[Sid]) -> &'static str {
    if distinct(accounts).len() == 1 {
        "its account"
    } else {
        "its accounts"
    }
}

/// What a write of a task makes besides its registration, and what an up-to-date install makes again
/// ([`task_beside_with`]): the record directory naming `principal`, and the default log naming `principal` and
/// `kept_log`, the accounts a service that is being retyped to this task still runs as
/// ([`Scm::retype_to_task_with`]). Nothing is read back.
fn task_records(id: &Id, runs: &Runs, principal: &Sid, kept_log: &[Sid]) -> Result<()> {
    run_records(id, runs, std::slice::from_ref(principal))?;
    let accounts = [kept_log, std::slice::from_ref(principal)].concat();
    default_log::ensure(runs.program_data(), id, &Naming::alike(&accounts)).map(drop)
}

/// What [`task_records`] would refuse as things stand, found without changing anything: the record directory
/// that is a link ([`Runs::check_directory`]), and the default log that is a directory, a link or being deleted
/// ([`default_log::check`]). An account change calls it first, before anything is sent: the delete of the old task
/// would otherwise leave nothing at the id.
fn check_records(id: &Id, runs: &Runs) -> Result<()> {
    runs.check_directory()
        .map_err(|e| Error::os(format!("use the run records of `{id}`"), e))?;
    default_log::check(runs.program_data(), id)
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

/// What an install that finds the task `installed` up to date makes again if it is gone ([`task_beside_with`]),
/// under the machine's `%ProgramData%`: the folders above the record directory are always judged, as a write
/// does ([`runs_for_write`]).
pub(in crate::backend::scm) fn task_beside(
    spec: &DaemonSpec,
    definition: &TaskXml,
    installed: &TaskSnapshot,
) -> Result<()> {
    task_beside_with(
        &program_data()?,
        &FolderCheck::new(CheckPermissions::Enable),
        spec,
        definition,
        installed,
        prepare,
    )
}

/// What a write of the task `installed` makes besides its registration, made again: the folders above the record
/// directory under `program_data` (`folders` judges them: [`runs_in`]), what the machine must hold for the account
/// `definition` registers (`prepare`, with [`Password::Unneeded`]: the event source and a real user's logon right,
/// but no password, for the task is not registered again), the record directory with the marker a dead uninstall
/// left cleared ([`Runs::ensure`]), and the default log ([`task_records`]). They name the account `prepare` found
/// alone. The record directory is not made by `runs_in`, but the folders above it may be; a refusal there comes
/// before anything the account needs (the logon right, the event source, the records, the log). Moving the task to
/// another account is a write's job, which finds the task registered as another.
fn task_beside_with(
    program_data: &Path,
    folders: &FolderCheck,
    spec: &DaemonSpec,
    definition: &TaskXml,
    installed: &TaskSnapshot,
    prepare: impl FnOnce(&TaskAccount, Option<Written<'_>>, Password) -> Result<Prepared>,
) -> Result<()> {
    let runs = runs_in(program_data, installed.id(), folders)?;
    let prepared = prepare(
        &definition.account(),
        written_user(&spec.common().user),
        Password::Unneeded,
    )?;
    task_records(installed.id(), &runs, &prepared.principal, &[])
}

// Enable and disable ==================================================================================================

impl Scm {
    pub(in crate::backend::scm) fn task_enable(&self, snapshot: &TaskSnapshot) -> Result<()> {
        let runs = Runs::of(&program_data()?, snapshot.id());
        self.task_set_armed_with(snapshot, true, &runs, identity::service_password)
    }

    pub(in crate::backend::scm) fn task_disable(&self, snapshot: &TaskSnapshot) -> Result<()> {
        let runs = Runs::of(&program_data()?, snapshot.id());
        self.task_set_armed_with(snapshot, false, &runs, identity::service_password)
    }

    /// Registers `snapshot`'s task again armed or not, and nothing else changed. `disable` removes the
    /// triggers. `enable`, when at least one stored trigger is switched on, keeps the stored triggers as
    /// they are and sets the task's own flag; when there is none, it compiles them from the blob (a blob
    /// that will not decode is the error). Either way the task's own flag is true.
    /// `password` reads the real user's password, which is asked before anything is sent.
    ///
    /// Refused first under the removal's marker ([`unfinished_change`]), before the password is asked for.
    pub(super) fn task_set_armed_with(
        &self,
        snapshot: &TaskSnapshot,
        armed: bool,
        runs: &Runs,
        password: impl FnOnce() -> Result<Option<String>>,
    ) -> Result<()> {
        let (verb, id) = (if armed { "enable" } else { "disable" }, snapshot.id());
        let closed = runs
            .is_closed()
            .map_err(|e| Error::os(format!("check whether `{id}` is being installed or uninstalled"), e))?;
        if closed {
            return Err(Error::os(
                format!("{verb} `{id}`"),
                std::io::Error::new(std::io::ErrorKind::ResourceBusy, unfinished_change(id.as_str())),
            ));
        }
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
            || service_gate(definition, written, Password::Read),
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
            |account| prepare(account, written_user(&spec.common().user), Password::Read),
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
    /// alone once the task is registered: the service runs as its account until it is retired, so
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
        task_records(id, runs, &prepared.principal, service.as_slice())?;
        retire()?;
        let wrote = self.task_register(id, definition, &prepared, Registering::Create { armed: false })?;
        // Any other end than a landed registration leaves the log named wide; the next install corrects it.
        if matches!(wrote, WriteOutcome::Done) && service.is_some() {
            let alone = std::slice::from_ref(&prepared.principal);
            default_log::ensure(runs.program_data(), id, &Naming::alike(alone))?;
        }
        Ok(wrote)
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
