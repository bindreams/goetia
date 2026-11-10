//! The flows every [`Backend`] shares: `install`, `preview`, `status`, `list` and the ownership
//! gate, written once over the backend's primitives.
//!
//! [`crate::backend::Backend`]'s blanket [`ServiceManager`](crate::manager::ServiceManager) impl
//! calls these. [`crate::decide::decide`] is reached only from here, so no backend can restate a
//! row of the install policy.

use std::io;
use std::sync::Arc;

use crate::backend::{Backend, InstalledService, ReadOutcome, ScanEntry, WriteOutcome};
use crate::decide::{self, ExternalConfig, Outcome, Ownership};
use crate::error::{Error, Reason, Result};
use crate::manager::{Budget, Installed, Status, Step};
use crate::spec::{DaemonSpec, Id};

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
    if blob.spec.id != *id {
        return (
            Ownership::OtherDaemon {
                installed: blob.spec.id.clone(),
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

/// What a verb run outside a sequence draws on: reserved for its own `step`, as
/// [`ServiceManager::prepare`](crate::manager::ServiceManager::prepare) would, and failing as that
/// verb for `id`.
pub(crate) fn reserved<B: Backend>(b: &B, id: &Id, step: Step, budget: Budget) -> Result<B::Reservation> {
    b.reserve(&[step], budget).map_err(|e| match e {
        Error::Reservation { id: None, source } => Error::Reservation {
            id: Some(id.to_string()),
            source,
        },
        other => other,
    })
}

// install and preview =================================================================================================

pub(crate) fn install<B: Backend>(b: &B, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
    // `install` has no `--timeout` of its own: its calls are unbounded.
    let mut reservation = reserved(b, &spec.id, Step::Install, Budget::Unbounded)?;
    // Rendered once, before the scope: what a backend takes or writes in `begin_install` (SCM's
    // lock and its provider's file) is never taken for an install that cannot be rendered.
    let desired = b.render_for_install(spec)?;
    let desired_text = desired.to_string();
    let mut scope = b.begin_install(spec, &desired)?;

    // No cap: `Raced` is a state change the backend detected, not a wait for time to pass.
    loop {
        let found = classify(b, b.read(&spec.id)?);
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
                    &mut reservation,
                )? {
                    WriteOutcome::Done => return Ok(outcome),
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
    let found = classify(b, b.read(&spec.id)?);
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
            Ok(status) => out.push(Installed::Ours {
                spec: blob.spec.clone(),
                state: status.state,
                pid: status.pid,
                enabled: status.enabled,
            }),
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
