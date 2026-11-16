//! The write half of the task side: installing a task, changing a daemon between a service and a task,
//! and `enable` and `disable`.
//!
//! Everything a write needs is acquired before the first request to Task Scheduler: the account, the
//! password and the logon right of a real user, the event source, and the record directory with its
//! permissions. A failure of any is a failure with nothing sent.
//!
//! A task is armed when its triggers are enabled; an install makes it disarmed and keeps what is
//! installed, and `enable` and `disable` change the flags of the stored XML and nothing else, so a hand
//! edit survives them. Re-registering a task that logs on with a password takes the password (M3): `enable`
//! and `disable` of a real user's task read `GOETIA_SERVICE_PASSWORD` as an install does.

use super::api::{Registered, Registration};
use super::live::Liveness;
use super::verbs::{Processes, System};
use super::{TaskApi, TaskPath, TaskSnapshot};
use crate::backend::scm::folder::program_data;
use crate::backend::scm::generate::{self, ScmRegistration, WindowsDefinition};
use crate::backend::scm::instance::TaskAccount;
use crate::backend::scm::manager::identity::{self, Right, Written};
use crate::backend::scm::manager::{Scm, ScmSnapshot, apply_gated, registry, service_gate, written_user};
use crate::backend::scm::runs::Runs;
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::sid::{Sid, sid_from_parts};
use crate::backend::scm::task_xml::TaskXml;
use crate::backend::{Identity, InstalledService, WriteOutcome};
use crate::error::{Error, Result};
use crate::manager::State;
use crate::spec::{DaemonKind, DaemonSpec, Id};

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

/// `registered` registered again as `account`, with its triggers armed or not and nothing else changed.
/// `password` is the real user's, which Task Scheduler needs to register a password-logon task again.
pub(super) fn set_armed(
    tasks: &dyn TaskApi,
    path: &TaskPath,
    registered: &Registered,
    armed: bool,
    account: &TaskAccount,
    password: Option<&str>,
) -> Result<Registration> {
    let xml = TaskXml::with_armed(&registered.xml, armed)?;
    tasks.register(path, &xml, account, password, true)
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
    /// `snapshot`'s task that keeps whether it is armed (a create is disarmed). Write
    /// [`Self::task_write_with`] over the machine's own record directory and account.
    pub(in crate::backend::scm) fn task_write(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        snapshot: Option<&TaskSnapshot>,
    ) -> Result<WriteOutcome> {
        let runs = Runs::of(&program_data()?, &spec.common().id);
        self.task_write_with(spec, definition, snapshot, &runs, |account| {
            prepare(account, written_user(&spec.common().user))
        })
    }

    /// [`Self::task_write`] with the record directory `runs` and `prepare`, which sees to what the
    /// machine must hold for the account. Both come before the first request to Task Scheduler.
    /// [`WriteOutcome::Raced`] when the task existed for a create or was gone for an update.
    pub(super) fn task_write_with(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        snapshot: Option<&TaskSnapshot>,
        runs: &Runs,
        prepare: impl FnOnce(&TaskAccount) -> Result<Prepared>,
    ) -> Result<WriteOutcome> {
        let id = &spec.common().id;
        let prepared = task_acquire(id, definition, runs, prepare)?;
        self.task_register(id, definition, snapshot, &prepared)
    }

    /// The registration itself, once [`task_acquire`] has seen to what it needs.
    fn task_register(
        &self,
        id: &Id,
        definition: &TaskXml,
        snapshot: Option<&TaskSnapshot>,
        prepared: &Prepared,
    ) -> Result<WriteOutcome> {
        let armed = snapshot.is_some_and(TaskSnapshot::armed);
        let registered = self.tasks.register(
            &TaskPath::of(id),
            &definition.to_xml(armed),
            &definition.account(),
            prepared.password.as_deref(),
            snapshot.is_some(),
        )?;
        Ok(match registered {
            Registration::Created => WriteOutcome::Done,
            Registration::Raced => WriteOutcome::Raced,
        })
    }
}

/// Everything a registration of `definition` needs, before any request: what the machine holds for the
/// account (`prepare`), and the record directory with permissions that name it. Every write makes it, so
/// a reinstall as another user can still write its records.
fn task_acquire(
    id: &Id,
    definition: &TaskXml,
    runs: &Runs,
    prepare: impl FnOnce(&TaskAccount) -> Result<Prepared>,
) -> Result<Prepared> {
    let prepared = prepare(&definition.account())?;
    runs.ensure(&prepared.principal)
        .map_err(|e| Error::os(format!("make the run records of `{id}` writable by its account"), e))?;
    Ok(prepared)
}

// Enable and disable ==================================================================================================

impl Scm {
    pub(in crate::backend::scm) fn task_enable(&self, snapshot: &TaskSnapshot) -> Result<()> {
        self.task_set_armed_with(snapshot, true, identity::service_password)
    }

    pub(in crate::backend::scm) fn task_disable(&self, snapshot: &TaskSnapshot) -> Result<()> {
        self.task_set_armed_with(snapshot, false, identity::service_password)
    }

    /// Registers `snapshot`'s task again with its triggers armed or not, and nothing else changed.
    /// `password` reads the real user's password, which is asked before anything is sent.
    pub(super) fn task_set_armed_with(
        &self,
        snapshot: &TaskSnapshot,
        armed: bool,
        password: impl FnOnce() -> Result<Option<String>>,
    ) -> Result<()> {
        let registered = snapshot.registered();
        let account = stored_account(registered)?;
        let password = password_of(&account, password)?;
        let path = TaskPath::of(snapshot.id());
        match set_armed(&*self.tasks, &path, registered, armed, &account, password.as_deref())? {
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
            |password| {
                apply_gated(definition, password, true)?;
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

    /// An install that changes a service into a task: the service is refused while it runs; everything
    /// the task needs is acquired; the service is stopped and deleted; the task is registered. The task's
    /// install holds the lock already. See [`Self::retype_to_service`].
    pub(in crate::backend::scm) fn retype_to_task(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        old: &ScmSnapshot,
        scope: &mut Option<InstallLock>,
    ) -> Result<WriteOutcome> {
        debug_assert!(scope.is_some(), "a task's install holds the install lock");
        let runs = Runs::of(&program_data()?, &spec.common().id);
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
                )
            },
            || self.service_retire(old),
        )?;
        self.sweep_after_retype();
        Ok(wrote)
    }

    /// [`Self::retype_to_task`] over the record directory `runs`, with `prepare`, `refuse` (the service
    /// runs: [`Error::RetypeRunning`]) and `retire` (the service is removed). The order is the contract:
    /// `refuse`, then everything the task needs ([`task_acquire`]), then `retire`, then the
    /// registration; a refusal or a failed acquisition leaves the service untouched.
    pub(super) fn retype_to_task_with(
        &self,
        spec: &DaemonSpec,
        definition: &TaskXml,
        runs: &Runs,
        prepare: impl FnOnce(&TaskAccount) -> Result<Prepared>,
        refuse: impl FnOnce() -> Result<()>,
        retire: impl FnOnce() -> Result<()>,
    ) -> Result<WriteOutcome> {
        let id = &spec.common().id;
        refuse()?;
        let prepared = task_acquire(id, definition, runs, prepare)?;
        retire()?;
        self.task_register(id, definition, None, &prepared)
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
