//! The flows every [`Backend`] shares: `install`, `preview`, `status`, `list`, `run` and the
//! ownership gate, written once over the backend's primitives.
//!
//! [`crate::backend::Backend`]'s blanket [`ServiceManager`](crate::manager::ServiceManager) impl
//! calls these. [`crate::decide::decide`] is reached only from here, so no backend can restate a
//! row of the install policy.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::{Backend, InstalledService, ReadOutcome, RunHandle, ScanEntry, WriteOutcome};
use crate::decide::{self, ExternalConfig, Outcome, Ownership};
use crate::error::{Error, Reason, Result};
use crate::manager::{Budget, Exit, Installed, State, Status, Step, output};
use crate::spec::{DaemonKind, DaemonSpec, Id};

// Ownership ===========================================================================================================

/// What `decide` is given for one `read`, with the snapshot a `write` needs.
struct Found<S> {
    ownership: Ownership,
    external: ExternalConfig,
    snapshot: Option<S>,
}

/// Classify `read` as `decide` wants it. A foreign id is never compared.
fn classify<B: Backend>(b: &B, read: ReadOutcome<B::Snapshot>) -> Found<B::Snapshot> {
    let (ownership, external, snapshot) = match read {
        ReadOutcome::Absent => (Ownership::Absent, ExternalConfig::Absent, None),
        ReadOutcome::Foreign { .. } => (Ownership::Foreign, ExternalConfig::Absent, None),
        ReadOutcome::Marked(snapshot) => {
            let (ownership, external) = judge(b, &snapshot);
            (ownership, external, Some(snapshot))
        }
    };
    Found {
        ownership,
        external,
        snapshot,
    }
}

/// Why [`Backend::regenerate`] failed. A backend that reads what is installed to regenerate (SCM)
/// can fail on that read, which is not a spec that cannot be regenerated: no goetia action
/// unblocks a comparison that could not be read, so it carries no advice to uninstall.
enum RegenerationFailure {
    /// The read of what is installed failed ([`Error::InstalledUnreadable`]): already says what failed.
    HostUnreadable(Error),
    /// The spec cannot be rendered.
    SpecUnregenerable(Error),
}

impl RegenerationFailure {
    /// Classified by the chain, not the outermost error: a backend may wrap the read's failure in a
    /// `Step` or hand it out `Kept`. The whole error is carried, wrappers included.
    fn of(error: Error) -> Self {
        if error.wraps_unreadable_host() {
            Self::HostUnreadable(error)
        } else {
            Self::SpecUnregenerable(error)
        }
    }
}

/// A marked service's ownership: its embedded spec must name the requested id, and is regenerated
/// against the snapshot and compared with what is installed.
fn judge<B: Backend>(b: &B, snapshot: &B::Snapshot) -> (Ownership, ExternalConfig) {
    let id = snapshot.id();
    let unusable = |reason, recovery| (Ownership::OursUnusable { reason, recovery }, ExternalConfig::Absent);
    let uninstall_advice = || Some(decide::unusable_recovery(id.as_str()));

    let blob = match snapshot.blob() {
        Ok(blob) => blob,
        Err(e) => return unusable(Reason::failed(Arc::clone(e)), uninstall_advice()),
    };
    // A manager that matches ids case-insensitively finds `frpc`'s service for `FRPC`. Nothing else
    // is read: writing would replace that daemon's service.
    if blob.spec.common().id != *id {
        return (
            Ownership::OtherDaemon {
                installed: blob.spec.common().id.clone(),
            },
            ExternalConfig::Absent,
        );
    }
    // Read first: a backend that reads what is installed to regenerate (SCM) would otherwise report
    // a failed read as a spec that cannot be regenerated, with advice to uninstall a healthy service.
    // No goetia action unblocks a comparison that could not be read.
    let on_disk = match b.installed(snapshot) {
        Ok(on_disk) => on_disk,
        Err(e) => return unusable(Reason::failed(e), None),
    };
    match b.regenerate(&blob.spec, snapshot) {
        Ok(regenerated) => (
            Ownership::Ours {
                blob: blob.clone(),
                regenerated: regenerated.to_string(),
                on_disk: on_disk.definition.to_string(),
            },
            on_disk.external,
        ),
        Err(e) => match RegenerationFailure::of(e) {
            RegenerationFailure::HostUnreadable(read) => unusable(Reason::failed(read), None),
            RegenerationFailure::SpecUnregenerable(e) => unusable(
                Reason::because("cannot regenerate the embedded spec", e),
                uninstall_advice(),
            ),
        },
    }
}

/// The marked id's snapshot, or the error an id that is not marked means.
pub(crate) fn require_marked<S>(id: &Id, read: ReadOutcome<S>) -> Result<S> {
    match read {
        ReadOutcome::Absent => Err(Error::NotInstalled {
            id: id.as_str().to_string(),
        }),
        ReadOutcome::Foreign { recovery } => Err(Error::Foreign {
            id: id.as_str().to_string(),
            recovery: recovery.unwrap_or_else(|| decide::foreign_recovery(id.as_str())),
        }),
        ReadOutcome::Marked(snapshot) => Ok(snapshot),
    }
}

/// The ownership gate of every verb but `install`: the id's snapshot if goetia's marker is on it,
/// otherwise the error that says why not. A blob that will not decode still passes: the marker
/// alone is proof of ownership, and `uninstall` is the way out of an unusable one. The blanket
/// [`ServiceManager`](crate::manager::ServiceManager) impl is the only caller, so a verb that
/// takes a snapshot has been gated.
pub(crate) fn owned<B: Backend>(b: &B, id: &Id) -> Result<B::Snapshot> {
    require_marked(id, b.read(id)?)
}

/// What a verb run outside a sequence draws on: reserved for its own `steps`, as
/// [`ServiceManager::prepare`](crate::manager::ServiceManager::prepare) would, and failing as that
/// verb for `id`.
pub(crate) fn reserved<B: Backend>(b: &B, id: &Id, steps: &[Step], budget: Budget) -> Result<B::Reservation> {
    b.reserve(steps, budget).map_err(|e| match e {
        Error::Reservation { id: None, source } => Error::Reservation {
            id: Some(id.to_string()),
            source,
        },
        other => other,
    })
}

/// What [`require_armable`] refuses a task with: `install --enable` says the same before it writes.
pub(crate) const NOTHING_TO_ARM: &str =
    "`schedule` is missing, so there is nothing to arm: `enable` applies to a task with a schedule";

/// A task's `enable` arms its schedule, so a task with none has nothing to arm. A service, and a blob
/// that will not decode (the marker alone is proof of ownership, and a verb must still work on it),
/// pass: the backend decides what it does with them.
pub(crate) fn require_armable<S: InstalledService>(snapshot: &S) -> Result<()> {
    match snapshot.blob() {
        Ok(blob) => match &blob.spec {
            DaemonSpec::Task(task) if task.schedule.is_none() => {
                Err(crate::spec::invalid(snapshot.id(), NOTHING_TO_ARM))
            }
            _ => Ok(()),
        },
        Err(_) => Ok(()),
    }
}

// install and preview =================================================================================================

/// `spec` installed, drawing on `reservation`, which the caller made for [`Step::Install`] and which
/// every retry shares.
pub(crate) fn install<B: Backend>(
    b: &B,
    spec: &DaemonSpec,
    force: bool,
    reservation: &mut B::Reservation,
) -> Result<Outcome> {
    // Rendered once, before the scope: what a backend takes or writes in `begin_install` (SCM's
    // lock and its provider's file) is never taken for an install that cannot be rendered.
    let desired = b.render_for_install(spec)?;
    let desired_text = desired.to_string();
    let mut scope = b.begin_install(spec, &desired, reservation)?;

    // No cap: `Raced` is a state change the backend detected, not a wait for time to pass.
    loop {
        let found = classify(b, b.read(&spec.common().id)?);
        let outcome = decide::decide(
            &found.ownership,
            &desired_text,
            spec,
            crate::version(),
            force,
            &found.external,
        );
        match outcome {
            Outcome::Create | Outcome::Update { .. } | Outcome::Stale { .. } => {
                match b.write(
                    spec,
                    &desired,
                    &outcome,
                    found.snapshot.as_ref(),
                    &mut scope,
                    reservation,
                )? {
                    WriteOutcome::Done => return Ok(outcome),
                    WriteOutcome::Refused(refusal) => return Ok(refusal),
                    WriteOutcome::Raced => {}
                }
            }
            Outcome::UpToDate
            | Outcome::Conflict { .. }
            | Outcome::RefuseForeign { .. }
            | Outcome::RefuseOtherDaemon { .. }
            | Outcome::RefuseUnusable { .. } => return Ok(outcome),
        }
    }
}

/// Always as if `force` were `false`: the forced outcome would hide the conflict `--force` exists
/// to let a user decide about.
pub(crate) fn preview<B: Backend>(b: &B, spec: &DaemonSpec) -> Result<Outcome> {
    let desired = b.render_for_install(spec)?.to_string();
    let found = classify(b, b.read(&spec.common().id)?);
    Ok(decide::decide(
        &found.ownership,
        &desired,
        spec,
        crate::version(),
        false,
        &found.external,
    ))
}

// status and list =====================================================================================================

pub(crate) fn status<B: Backend>(b: &B, id: &Id) -> Result<Status> {
    let snapshot = owned(b, id)?;
    let blob = snapshot.blob().map_err(|e| Error::Kept(Arc::clone(e)))?;
    b.regenerate(&blob.spec, &snapshot)
        .map_err(|e| match RegenerationFailure::of(e) {
            RegenerationFailure::HostUnreadable(read) => read,
            RegenerationFailure::SpecUnregenerable(e) => Error::Step {
                what: format!("`{id}`'s embedded spec cannot be regenerated"),
                source: Box::new(e),
            },
        })?;
    b.live_status(&snapshot)
}

pub(crate) fn list<B: Backend>(b: &B) -> Result<Vec<Installed>> {
    let scan = b.scan();
    let mut out = Vec::new();
    for (name, entry) in scan.entries {
        // A name that is no id has no snapshot to ask for its live state, which is reported after
        // the checks any marked entry gets.
        let (blob, snapshot) = match &entry {
            ScanEntry::Read(ReadOutcome::Absent | ReadOutcome::Foreign { .. }) => continue,
            ScanEntry::Read(ReadOutcome::Marked(snapshot)) => (snapshot.blob(), Ok(snapshot)),
            ScanEntry::Misnamed { blob, name } => (blob.as_ref(), Err(name)),
        };
        let unusable = |reason| Installed::OursUnusable {
            name: name.clone(),
            reason,
        };
        let blob = match blob {
            Ok(blob) => blob,
            Err(e) => {
                out.push(unusable(Reason::failed(Arc::clone(e))));
                continue;
            }
        };
        // A name that is no id has no snapshot to regenerate against, so it is reported by its name
        // alone.
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(e) => {
                out.push(unusable(Reason::around(
                    "marked ours, but its name is not a valid daemon id: ",
                    Arc::clone(e),
                    "",
                )));
                continue;
            }
        };
        if let Err(e) = b.regenerate(&blob.spec, snapshot) {
            // A backend that reads what is installed to regenerate can find the service gone: as
            // for `live_status`, absence is established and the entry is omitted.
            if !matches!(e.cause(), Error::NotInstalled { .. }) {
                out.push(unusable(match RegenerationFailure::of(e) {
                    RegenerationFailure::HostUnreadable(read) => Reason::failed(read),
                    RegenerationFailure::SpecUnregenerable(e) => {
                        Reason::because("marked ours, but its embedded spec cannot be regenerated", e)
                    }
                }));
            }
            continue;
        }
        // One entry's failure must not take down the listing of every other daemon, except that
        // no manager to ask is every entry's.
        match b.live_status(snapshot) {
            Ok(status) => {
                debug_assert_eq!(
                    blob.spec.kind(),
                    status.kind(),
                    "a backend answered another kind's status"
                );
                out.push(Installed::Ours {
                    spec: blob.spec.clone(),
                    status,
                });
            }
            Err(e) => match e.cause() {
                Error::NoManager { .. } => return Err(e),
                // The query completed and found the service gone: absence is established.
                Error::NotInstalled { .. } => {}
                _ => out.push(unusable(Reason::because(
                    "marked ours, but its live state could not be queried",
                    e,
                ))),
            },
        }
    }
    out.extend(scan.reported);
    Ok(out)
}

// run =================================================================================================================

/// Runs the idle task `id` once and follows the run: its output to `out` until it ends, then how it
/// ended. See [`ServiceManager::run`](crate::manager::ServiceManager::run).
///
/// The state is read once, before the request: a task found running is refused without anything
/// being reserved or sent. The thread that waits for the run is made before the request too, so a
/// start that was sent is never left unwatched for want of one.
pub(crate) fn run<B: Backend>(b: &B, id: &Id, out: &mut dyn Write) -> Result<Exit> {
    let snapshot = owned(b, id)?;
    let blob = snapshot.blob().map_err(|e| Error::Kept(Arc::clone(e)))?;
    if blob.spec.kind() != DaemonKind::Task {
        return Err(Error::NotATask { id: id.to_string() });
    }
    match b.task_state(&snapshot)? {
        State::Idle => {}
        State::Running => return Err(Error::AlreadyRunning { id: id.to_string() }),
        State::Stopped | State::Failed | State::Unknown => return Err(Error::RunStateUnknown { id: id.to_string() }),
    }
    let mut reservation = reserved(b, id, &[Step::Start], Budget::Immediate)?;
    let waiter = Waiter::<B::Run>::spawn(id)?;

    match b.begin_run(snapshot, &mut reservation) {
        Ok(run) => waiter.follow(run, id, out),
        Err(e) => {
            waiter.abandon();
            Err(e)
        }
    }
}

/// The thread that waits for a run, made before the run is requested.
struct Waiter<R> {
    /// How the run reaches the thread once the request has made one.
    hand_over: crossbeam_channel::Sender<R>,
    /// Closed by the thread when the wait has finished: the stream's signal that the run ended.
    ended: crossbeam_channel::Receiver<()>,
    /// Set by the thread before it closes `ended`: whether the wait found how the run ended.
    exit_recorded: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Option<Result<Exit>>>,
}

impl<R: RunHandle> Waiter<R> {
    /// Nothing has been sent when this fails, so it fails as any step that could not be made ready.
    fn spawn(id: &Id) -> Result<Self> {
        let (hand_over, handed) = crossbeam_channel::bounded::<R>(1);
        // Nothing is sent on `ended`: closing it is the signal, and it closes on a panic too, so the
        // stream never outlives a run whose wait is gone.
        let (end, ended) = crossbeam_channel::bounded::<()>(0);
        let exit_recorded = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&exit_recorded);
        let thread = std::thread::Builder::new()
            .name(format!("goetia run {id}"))
            .spawn(move || {
                // `None`: the request failed, so there was no run.
                let exit = handed.recv().ok().map(RunHandle::wait);
                recorded.store(matches!(exit, Some(Ok(_))), Ordering::SeqCst);
                drop(end);
                exit
            })
            .map_err(|source| Error::Reservation {
                id: Some(id.to_string()),
                source,
            })?;
        Ok(Self {
            hand_over,
            ended,
            exit_recorded,
            thread,
        })
    }

    /// The request failed: no run is coming, and the thread ends having waited for nothing.
    fn abandon(self) {
        drop(self.hand_over);
        if let Err(panic) = self.thread.join() {
            std::panic::resume_unwind(panic);
        }
    }

    /// Shows `run`'s output while the thread waits for its end, and returns how it ended.
    ///
    /// The run is in flight, so it is waited for whatever happens to the output. If the wait failed
    /// its error is returned: the exit is the more basic fact. Otherwise a failure to show the output
    /// is returned, naming the exit.
    fn follow(self, run: R, id: &Id, out: &mut dyn Write) -> Result<Exit> {
        let source = run.output();
        // The thread is alive until it has a run or its sender is dropped: this cannot fail.
        let handed = self.hand_over.send(run).is_ok();
        debug_assert!(handed, "the wait thread ended before it was given the run");

        let shown = output::stream(source, id, out, &self.ended, &self.exit_recorded);
        let waited = match self.thread.join() {
            Ok(Some(waited)) => waited,
            Ok(None) => unreachable!("the wait thread was given the run"),
            Err(panic) => std::panic::resume_unwind(panic),
        };
        match (waited, shown) {
            (Err(e), _) => Err(e),
            (Ok(exit), Ok(())) => Ok(exit),
            (Ok(exit), Err(shown)) => Err(Error::Step {
                what: format!("`{id}` ended with {exit}, but goetia stopped showing its output"),
                source: Box::new(shown),
            }),
        }
    }
}

// scan ================================================================================================================

/// One pass over a backend's container of services (a directory, a registry key): hands each
/// entry to `keep`, and returns the entry for what the pass did not reach, if anything.
///
/// - `opened` not found: the container is not there, which establishes absence. `None`.
/// - `opened` failed otherwise: nothing was enumerated and no absence is established.
///   `detail("open", e)` is the failure.
/// - The first entry that faults ends the pass with `detail("enumerate", e)`. What `keep` was
///   given before it stays: returning an error instead discards every one of those, reporting each
///   id absent.
///
/// `entries` borrows the opened resource mutably because the iterator it makes borrows it
/// (`RegKey::enum_keys`, `&mut ReadDir`).
pub(crate) fn scan<R, T>(
    location: &str,
    opened: io::Result<R>,
    entries: impl for<'r> FnOnce(&'r mut R) -> Box<dyn Iterator<Item = io::Result<T>> + 'r>,
    detail: impl Fn(&'static str, io::Error) -> Error,
    mut keep: impl FnMut(T),
) -> Option<Installed> {
    let mut resource = match opened {
        Ok(resource) => resource,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Installed::scan_incomplete(location, detail("open", e))),
    };
    for entry in entries(&mut resource) {
        match entry {
            Ok(item) => keep(item),
            Err(e) => return Some(Installed::scan_incomplete(location, detail("enumerate", e))),
        }
    }
    None
}

/// What [`scan`] reports of a directory it could not finish: `op` is `"open"` or `"enumerate"`.
/// One wording for every backend whose container is a directory.
#[cfg(unix)]
pub(crate) fn directory_detail(op: &'static str, e: io::Error) -> Error {
    match op {
        "open" => Error::os("read directory", e),
        _ => Error::os("read a directory entry", e),
    }
}

#[cfg(test)]
#[path = "flow_tests.rs"]
mod flow_tests;
