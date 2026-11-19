//! The read half of the task side: what Task Scheduler holds at an id, the definition it stores,
//! the listing of the tasks in `\Goetia`, and a task's status.
//!
//! A task is goetia's by the marker in its `Data`, as a service is by its `Parameters`; its blob
//! may not decode, and the marker alone is proof of ownership.

use std::cell::Cell;
use std::io;
use std::sync::Arc;

use super::api::{Instance, Registered};
use super::live::{Liveness, live_instances, probe};
use super::status::{run_state, task_status};
use super::{TaskApi, TaskPath};
use crate::backend::scm::generate::{self, WindowsDefinition};
use crate::backend::scm::instance::TaskAccount;
use crate::backend::scm::manager::{Scm, ScmSnapshot, WindowsSnapshot, identity, open_refused};
use crate::backend::scm::sid::Sid;
use crate::backend::scm::task_xml::{TaskXml, Token, extract_from_data};
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
    /// What the stored XML says of the registration's token, read once: `Err` carries why it could not be.
    token: std::result::Result<Option<Token>, String>,
    /// The SID of the account the stored principal names, `None` when it cannot be told (no principal, or a
    /// name that names no account): not known to be any account being installed.
    principal: Option<Sid>,
}

impl TaskSnapshot {
    pub(in crate::backend::scm) fn new(id: Id, blob: Decoded, registered: Registered) -> Self {
        let token = TaskXml::token_of(&registered.xml, id.as_str()).map_err(|e| e.report().to_string());
        let principal = TaskXml::principal(&registered.xml)
            .ok()
            .and_then(|(TaskAccount::Service(text) | TaskAccount::User(text))| identity::sid_of_principal(&text).ok());
        Self {
            id,
            blob,
            registered,
            token,
            principal,
        }
    }

    /// The installed definition, read back from the stored XML.
    fn definition(&self) -> Result<TaskXml> {
        TaskXml::parse(&self.registered.xml, &self.registered.data)
    }

    /// The token the stored registration carries, `None` when its arguments end with none (a hand edit).
    /// An error when the XML could not be read: no token is then no licence to write another.
    pub(super) fn token(&self) -> Result<Option<Token>> {
        self.token
            .clone()
            .map_err(|why| Error::blob(format!("read the token of the registration of `{}`: {why}", self.id)))
    }

    /// The SID of the account the stored principal names, `None` when it cannot be told.
    pub(super) fn principal(&self) -> Option<&Sid> {
        self.principal.as_ref()
    }

    /// Whether every trigger is enabled.
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

    /// The definition as installed: the stored XML read back. The armed state is no part of it.
    pub(in crate::backend::scm) fn task_installed(
        &self,
        snapshot: &TaskSnapshot,
    ) -> Result<InstalledDefinition<WindowsDefinition>> {
        Ok(InstalledDefinition {
            definition: WindowsDefinition::Task(snapshot.definition()?),
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
