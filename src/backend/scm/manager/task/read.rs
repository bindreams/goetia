//! The read half of the task side: what Task Scheduler holds at an id, the definition it stores,
//! the listing of the tasks in `\Goetia`, and a task's status.
//!
//! A task is goetia's by the marker in its `Data`, as a service is by its `Parameters`; its blob
//! may not decode, and the marker alone is proof of ownership.

use std::cell::Cell;
use std::io::{self, Write as _};
use std::sync::Arc;

use super::api::{Instance, Registered};
use super::live::{Liveness, live_instances, probe};
use super::status::{run_state, task_status};
use super::{TaskApi, TaskPath};
use crate::backend::scm::generate::{self, WindowsDefinition};
use crate::backend::scm::instance::TaskAccount;
use crate::backend::scm::manager::identity::Written;
use crate::backend::scm::manager::{Scm, ScmSnapshot, WindowsSnapshot, identity, open_refused, written_user};
use crate::backend::scm::sid::{Sid, sid_display};
use crate::backend::scm::task_xml::{TaskXml, Token, account_of, extract_from_data};
use crate::backend::{Decoded, Identity, InstalledDefinition, InstalledService, ReadOutcome, Scan, ScanEntry, Unnamed};
use crate::blob::Blob;
use crate::decide::ExternalConfig;
use crate::error::{Error, Reason, Result};
use crate::manager::{Installed, State, Status};
use crate::spec::{DaemonSpec, Id};

/// What `read` saw of a task goetia marked: its stored definition, as Task Scheduler gave it.
pub(crate) struct TaskSnapshot {
    id: Id,
    blob: Decoded,
    registered: Registered,
}

/// What a task's stored registration says ([`TaskSnapshot::stored`]).
pub(super) struct StoredRegistration {
    /// The SID of the account its principal names; `None` when the XML names no principal or the name names no
    /// account: not known to be any account being installed.
    pub principal: Option<Sid>,
    /// Whether it has a trigger that is switched on, whatever the task's own flag is: the armed state an account
    /// change or an install gives the registration it writes, since the flag may be off only because an earlier
    /// change or uninstall that was cut short disabled the task.
    pub has_triggers: bool,
    /// The token its arguments end with; `None` when they end with no goetia ending for this task (a hand
    /// edit, a malformed token included).
    pub token: Option<Token>,
}

impl TaskSnapshot {
    pub(in crate::backend::scm) fn new(id: Id, blob: Decoded, registered: Registered) -> Self {
        Self { id, blob, registered }
    }

    /// The installed definition, read back from the stored XML.
    fn definition(&self) -> Result<TaskXml> {
        TaskXml::parse(&self.registered.xml, &self.registered.data)
    }

    /// What the stored registration says, from one read of its XML and one lookup of its account, for the write
    /// path alone. An error when the XML cannot be read, or the account could not be looked up for any other
    /// reason than "no such account" (a transient RPC or domain controller failure): the registration is then
    /// not known to be another account's.
    pub(super) fn stored(&self) -> Result<StoredRegistration> {
        let stored = TaskXml::stored(&self.registered.xml, self.id.as_str())?;
        let principal = match principal_text(stored.principal) {
            Some(text) => known_account(identity::sid_of_principal(&text)).map_err(|e| Error::Step {
                what: format!("look up the account of the registration of `{}`", self.id),
                source: Box::new(e),
            })?,
            None => None,
        };
        Ok(StoredRegistration {
            principal,
            has_triggers: stored.has_triggers,
            token: stored.token,
        })
    }

    /// What the registration is for, when that is not the account the blob's user names now: the registered
    /// SID's text, the principal's text marked as naming no account ([`unmapped`]), or [`NO_PRINCIPAL`] when
    /// there is none. `None` when it is for that account, when the account is built in, and when a lookup fails
    /// for any other reason than "no such account": such a difference is not known, so the blob's account stands
    /// until a lookup answers, and a line on stderr says the check was skipped.
    ///
    /// The one comparison `read` shows and `enable` and `disable` refuse by, the latter only for a registration
    /// that stays under the blob's account ([`StoredAccount::Blob`](super::write::StoredAccount)). Compares the
    /// blob's account with the principal of the stored XML, which is read once. An error when the blob did not
    /// decode, when its user names no task account, and when the XML's principal cannot be read.
    pub(super) fn other_account(
        &self,
        lookup: impl Fn(&str, Option<Written<'_>>) -> Result<Sid>,
    ) -> Result<Option<String>> {
        let blob = self.blob.as_ref().map_err(Error::kept)?;
        let common = blob.spec.common();
        let TaskAccount::User(name) = account_of(&common.id, &common.user)? else {
            return Ok(None);
        };
        let principal = TaskXml::principal(&self.registered.xml).map_err(|e| Error::Step {
            what: format!("read the principal of the registration of `{}`", self.id),
            source: Box::new(e),
        })?;
        let Some(text) = principal_text(principal) else {
            return Ok(Some(NO_PRINCIPAL.to_owned()));
        };
        let written = written_user(&common.user);
        compared(&text, &name, written, lookup).or_else(|e| {
            // Not `eprintln!`, which panics when the write fails: this is a fallback that fails nothing.
            let _ = writeln!(io::stderr(), "{}", unchecked_warning(&self.id, &e));
            Ok(None)
        })
    }

    /// Whether the task has triggers and is itself enabled.
    pub(super) fn armed(&self) -> bool {
        TaskXml::armed(&self.registered.xml)
    }

    /// The definition as Task Scheduler stores it: what a verb that registers the task again starts from.
    pub(super) fn registered(&self) -> &Registered {
        &self.registered
    }
}

impl InstalledService for TaskSnapshot {
    fn id(&self) -> &Id {
        &self.id
    }

    fn blob(&self) -> std::result::Result<&Blob, &Arc<Error>> {
        self.blob.as_ref()
    }
}

/// What a task's `Data` says of its ownership: no goetia marker is foreign, and a marker whose blob
/// will not decode is marked with the failure kept.
fn classify(registered: Registered) -> ReadOutcome<Unnamed<Registered>> {
    let blob = match extract_from_data(&registered.data) {
        Ok(None) => return ReadOutcome::Foreign { recovery: None },
        Ok(Some(blob)) => Ok(blob),
        Err(e) => Err(Arc::new(e)),
    };
    ReadOutcome::Marked(Unnamed { blob, body: registered })
}

/// The SID a principal lookup found, `None` when the system says no such account exists: not known to be
/// any account being installed. Any other failure of the lookup is the error.
pub(super) fn known_account(lookup: Result<Sid>) -> Result<Option<Sid>> {
    match lookup {
        Ok(sid) => Ok(Some(sid)),
        Err(Error::AccountLookup { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// [`TaskSnapshot::other_account`] of the principal `text` and the blob's account `name`, written as `written`.
fn compared(
    text: &str,
    name: &str,
    written: Option<Written<'_>>,
    lookup: impl Fn(&str, Option<Written<'_>>) -> Result<Sid>,
) -> Result<Option<String>> {
    let Some(registered) = known_account(lookup(text, None))? else {
        return Ok(Some(unmapped(text)));
    };
    let named = known_account(lookup(name, written))?;
    Ok((named.as_ref() != Some(&registered)).then(|| sid_display(&registered)))
}

/// The line that says the account the task `id` is registered for could not be checked, `failed` being why.
fn unchecked_warning(id: &Id, failed: &Error) -> String {
    format!(
        "warning: the account the task `{id}` is registered for could not be checked, so it is taken to be the one \
         its user names: {}",
        failed.report()
    )
}

/// What [`TaskSnapshot::other_account`] says of a registration with no principal.
pub(super) const NO_PRINCIPAL: &str = "<none>";

/// What [`TaskSnapshot::other_account`] says of a principal whose text `text` names no account.
fn unmapped(text: &str) -> String {
    format!("{text} (no such account)")
}

/// The system's account lookup for [`TaskSnapshot::other_account`]. With `written` (the blob's user as its spec
/// wrote it) it resolves as an install does for each form: a `user.id` is the SID its string spells, with no
/// account looked up, and a `user.name` is looked up by name. With none, `name` is a registration's principal,
/// as Task Scheduler stores it ([`identity::sid_of_principal`]).
pub(super) fn sid_of_account(name: &str, written: Option<Written<'_>>) -> Result<Sid> {
    match written {
        Some(_) => identity::service_account_sid(Some(name), written),
        None => identity::sid_of_principal(name),
    }
}

/// The name a registration's principal is looked up by, as Task Scheduler stores it.
fn principal_text(principal: Option<TaskAccount>) -> Option<String> {
    match principal? {
        TaskAccount::Service(text) | TaskAccount::User(text) => Some(text),
    }
}

fn snapshot_of(id: Id, blob: Decoded, registered: Registered) -> WindowsSnapshot {
    WindowsSnapshot::Task(TaskSnapshot::new(id, blob, registered))
}

// One task ------------------------------------------------------------------------------------------------------------

impl Scm {
    /// What Task Scheduler holds at `\Goetia\<id>`. `Err` where it could not be asked: neither absence
    /// nor ownership is established.
    pub(in crate::backend::scm) fn task_read(&self, id: &Id) -> Result<ReadOutcome<TaskSnapshot>> {
        Ok(match self.tasks.get(&TaskPath::of(id))? {
            None => ReadOutcome::Absent,
            Some(registered) => classify(registered).named(id, TaskSnapshot::new),
        })
    }

    /// `spec` as a task, run through the host the provider names. A task's account is read from the
    /// spec alone, so none is resolved: the identity is a service's, and a task has no use for it.
    pub(in crate::backend::scm) fn task_render(&self, spec: &DaemonSpec) -> Result<WindowsDefinition> {
        generate::registration(spec, &Identity { user: String::new() }, &|| self.provider()?.preview())
    }

    /// The definition as installed: [`Self::task_installed_with`] over the system's account lookup.
    pub(in crate::backend::scm) fn task_installed(
        &self,
        snapshot: &TaskSnapshot,
    ) -> Result<InstalledDefinition<WindowsDefinition>> {
        self.task_installed_with(snapshot, sid_of_account)
    }

    /// The definition as installed: the stored XML read back, showing the account the task is registered for
    /// ([`TaskSnapshot::other_account`]) in place of the blob's when they differ. A lookup failure other than
    /// "no such account" shows the blob's account. The armed state is no part of it.
    pub(super) fn task_installed_with(
        &self,
        snapshot: &TaskSnapshot,
        lookup: impl Fn(&str, Option<Written<'_>>) -> Result<Sid>,
    ) -> Result<InstalledDefinition<WindowsDefinition>> {
        let mut definition = snapshot.definition()?;
        let registered = snapshot.other_account(lookup).map_err(|e| Error::InstalledUnreadable {
            id: snapshot.id.as_str().to_string(),
            source: Box::new(e),
        })?;
        if let Some(shown) = registered {
            definition = definition.shown_for(shown);
        }
        Ok(InstalledDefinition {
            definition: WindowsDefinition::Task(definition),
            external: ExternalConfig::Absent,
        })
    }

    /// `spec` as `render` would write it, run through the host the installed action names, so that a
    /// host another provider put there is the same spec and no provider is consulted. A stored
    /// definition that cannot be read is [`Error::InstalledUnreadable`], not a spec that cannot be
    /// regenerated.
    pub(in crate::backend::scm) fn task_regenerate(
        &self,
        spec: &DaemonSpec,
        snapshot: &TaskSnapshot,
    ) -> Result<WindowsDefinition> {
        let installed = snapshot.definition().map_err(|e| Error::InstalledUnreadable {
            id: snapshot.id.as_str().to_string(),
            source: Box::new(e),
        })?;
        generate::registration(spec, &Identity { user: String::new() }, &|| Ok(installed.host()))
    }

    /// The status of a task: [`Self::task_status_with`] over the real process probe.
    pub(in crate::backend::scm) fn task_live_status(&self, snapshot: &TaskSnapshot) -> Result<Status> {
        self.task_status_with(snapshot, probe)
    }

    /// The run state of a task: [`Self::task_state_with`] over the real process probe.
    pub(in crate::backend::scm) fn task_run_state(&self, snapshot: &TaskSnapshot) -> Result<State> {
        self.task_state_with(snapshot, probe)
    }

    /// The status of `snapshot`'s task, a run being live where `alive` says its process is. What
    /// cannot be read fails the status: the listing, a probe, and Task Scheduler's own record of the
    /// task's runs. A state that was not established is never reported.
    pub(super) fn task_status_with(
        &self,
        snapshot: &TaskSnapshot,
        alive: impl FnMut(u32) -> io::Result<Liveness>,
    ) -> Result<Status> {
        let path = TaskPath::of(&snapshot.id);
        let live = live_runs(&*self.tasks, "status", &snapshot.id, &path, alive)?;
        let info = self.tasks.info(&path)?;
        Ok(Status::Task(task_status(&info, &live, snapshot.armed())))
    }

    /// The run state of `snapshot`'s task from its listing and the liveness of what it lists alone:
    /// everything [`flow::run`](crate::manager::flow::run) reads before it requests a run, so that
    /// a part of the status that can fail (Task Scheduler's own record) cannot refuse one. What
    /// cannot be read is the error, which `run` reports in place of a state.
    pub(super) fn task_state_with(
        &self,
        snapshot: &TaskSnapshot,
        alive: impl FnMut(u32) -> io::Result<Liveness>,
    ) -> Result<State> {
        let live = live_runs(&*self.tasks, "run", &snapshot.id, &TaskPath::of(&snapshot.id), alive)?;
        Ok(run_state(&live).0)
    }
}

/// The runs of the task at `path` that are alive, for the verb `operation`. The listing that failed is
/// its own error. A process that cannot be judged is the error too, worded as [`open_refused`] words a
/// denial: a task that runs as another account (SYSTEM, LocalService, an Administrator's own elevated
/// task) has a process that only an Administrator may open, so from any other prompt its state cannot
/// be told, and re-running as Administrator clears it.
pub(super) fn live_runs(
    tasks: &dyn TaskApi,
    operation: &str,
    id: &Id,
    path: &TaskPath,
    alive: impl FnMut(u32) -> io::Result<Liveness>,
) -> Result<Vec<Instance>> {
    let listed = tasks.instances(path)?;
    let judged = Cell::new(None);
    let mut alive = alive;
    live_instances(listed, |pid| {
        judged.set(Some(pid));
        alive(pid)
    })
    .map_err(|e| {
        let pid = judged.get().unwrap_or_default();
        open_refused(
            operation,
            &format!("the process of `{id}`'s run (pid {pid})"),
            &format!("open the process of `{id}`'s run (pid {pid}) to check that it runs"),
            e,
        )
    })
}

// Every task ----------------------------------------------------------------------------------------------------------

/// Where a scan of the tasks says it stopped.
const FOLDER: &str = "Task Scheduler's `\\Goetia` folder";

/// What the scan of Task Scheduler's tasks found: each task in `\Goetia` whose name is an id and
/// that `held` does not say the SCM holds, as `read` would have classified it, and what it could not
/// read. A task that could not be read leaves its own id undetermined, and an enumeration that
/// could not be finished is reported without a name: nothing it did not reach is known.
pub(super) fn scan_tasks(tasks: &dyn TaskApi, held: impl Fn(&str) -> bool) -> Scan<WindowsSnapshot> {
    let mut scan = Scan {
        entries: Vec::new(),
        reported: Vec::new(),
    };
    let names = match tasks.names() {
        Ok(names) => names,
        Err(e) => {
            scan.reported.push(Installed::scan_incomplete(FOLDER, e));
            return scan;
        }
    };
    for name in names {
        let Some(id) = TaskPath::id(&name) else {
            continue;
        };
        if held(&name) {
            continue;
        }
        match tasks.get(&TaskPath::of(&id)) {
            // Gone since it was listed: not there to list.
            Ok(None) => {}
            Ok(Some(registered)) => {
                let entry = ScanEntry::of(&name, classify(registered), snapshot_of);
                scan.entries.push((name, entry));
            }
            Err(e) => scan.reported.push(Installed::Undetermined {
                name: Some(name),
                reason: Reason::around("", e, " — so whether this task is one of goetia's is unknown."),
            }),
        }
    }
    scan
}

/// The SCM's `services` and Task Scheduler's tasks as one scan. An id both stores hold is listed
/// once, as the SCM's entry, whatever that is: the task is not even read, as [`which`] reads the id.
///
/// [`which`]: super::which
pub(in crate::backend::scm) fn combined(services: Scan<ScmSnapshot>, tasks: &dyn TaskApi) -> Scan<WindowsSnapshot> {
    let mut entries: Vec<_> = services
        .entries
        .into_iter()
        .map(|(name, entry)| (name, entry.map(WindowsSnapshot::Service)))
        .collect();
    let in_scm = |name: &str| entries.iter().any(|(held, _)| held.eq_ignore_ascii_case(name));
    let found = scan_tasks(tasks, in_scm);
    entries.extend(found.entries);
    Scan {
        entries,
        reported: services.reported.into_iter().chain(found.reported).collect(),
    }
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod read_tests;
