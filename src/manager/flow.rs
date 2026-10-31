//! The flows every [`Backend`] shares: `install`, `preview`, `status`, `list` and the ownership
//! gate, written once over the backend's primitives.
//!
//! [`crate::backend::Backend`]'s blanket [`ServiceManager`](crate::manager::ServiceManager) impl
//! calls these. [`crate::decide::decide`] is reached only from here, so no backend can restate a
//! row of the install policy.

use std::io;

use crate::backend::{Backend, ReadOutcome, WriteOutcome};
use crate::blob::Blob;
use crate::decide::{self, ExternalConfig, Outcome, Ownership};
use crate::error::{Error, Result};
use crate::manager::{Installed, Status};
use crate::spec::{DaemonSpec, Id};

// Ownership ===========================================================================================================

/// What `decide` is given for one `read`, with the snapshot a `write` needs.
struct Found<S> {
    ownership: Ownership,
    external: ExternalConfig,
    snapshot: Option<S>,
}

/// Classify `read` as `decide` wants it. A foreign id is never compared.
fn classify<B: Backend>(b: &B, id: &Id, read: ReadOutcome<B::Snapshot>) -> Found<B::Snapshot> {
    let (ownership, external, snapshot) = match read {
        ReadOutcome::Absent => (Ownership::Absent, ExternalConfig::Absent, None),
        ReadOutcome::Foreign { .. } => (Ownership::Foreign, ExternalConfig::Absent, None),
        ReadOutcome::Marked { blob, snapshot } => {
            let (ownership, external) = judge(b, id, blob, &snapshot);
            (ownership, external, Some(snapshot))
        }
    };
    Found {
        ownership,
        external,
        snapshot,
    }
}

/// A marked id's ownership: its embedded spec is rendered, and compared with what is installed.
fn judge<B: Backend>(b: &B, id: &Id, blob: Result<Blob>, snapshot: &B::Snapshot) -> (Ownership, ExternalConfig) {
    let unreadable = |reason, recovery| (Ownership::OursUnreadable { reason, recovery }, ExternalConfig::Absent);
    let uninstall_advice = || Some(decide::unreadable_recovery(id.as_str()));

    let blob = match blob {
        Ok(blob) => blob,
        Err(e) => return unreadable(e.to_string(), uninstall_advice()),
    };
    let regenerated = match b.render(&blob.spec) {
        Ok(regenerated) => regenerated.to_string(),
        Err(e) => {
            return unreadable(format!("cannot regenerate the embedded spec: {e}"), uninstall_advice());
        }
    };
    match b.installed(id, snapshot) {
        // No goetia action unblocks a comparison that could not be read.
        Err(e) => unreadable(e.to_string(), None),
        Ok(on_disk) => (
            Ownership::Ours {
                blob,
                regenerated,
                on_disk: on_disk.definition.to_string(),
            },
            on_disk.external,
        ),
    }
}

/// The marked id's blob and snapshot, or the error an id that is not marked means.
pub(crate) fn require_marked<S>(id: &Id, read: ReadOutcome<S>) -> Result<(Result<Blob>, S)> {
    match read {
        ReadOutcome::Absent => Err(Error::NotInstalled {
            id: id.as_str().to_string(),
        }),
        ReadOutcome::Foreign { recovery } => Err(Error::Foreign {
            id: id.as_str().to_string(),
            recovery: recovery.unwrap_or_else(|| decide::foreign_recovery(id.as_str())),
        }),
        ReadOutcome::Marked { blob, snapshot } => Ok((blob, snapshot)),
    }
}

/// The ownership gate of every verb but `install`: the id's snapshot if goetia's marker is on it,
/// otherwise the error that says why not. A blob that will not decode still passes: the marker
/// alone is proof of ownership, and `uninstall` is the way out of an unreadable one.
pub(crate) fn owned<B: Backend>(b: &B, id: &Id) -> Result<B::Snapshot> {
    require_marked(id, b.read(id)?).map(|(_, snapshot)| snapshot)
}

// install and preview =================================================================================================

pub(crate) fn install<B: Backend>(b: &B, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
    let mut scope = b.begin_install(spec)?;
    let desired = b.render(spec)?;
    let desired_text = desired.to_string();

    // No cap: `Raced` is a state change the backend detected, not a wait for time to pass.
    loop {
        let found = classify(b, &spec.id, b.read(&spec.id)?);
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
                match b.write(spec, &desired, &outcome, found.snapshot.as_ref(), &mut scope)? {
                    WriteOutcome::Done => return Ok(outcome),
                    WriteOutcome::Raced => {}
                }
            }
            Outcome::UpToDate
            | Outcome::Conflict { .. }
            | Outcome::RefuseForeign { .. }
            | Outcome::RefuseUnreadable { .. } => return Ok(outcome),
        }
    }
}

/// Always as if `force` were `false`: the forced outcome would hide the conflict `--force` exists
/// to let a user decide about.
pub(crate) fn preview<B: Backend>(b: &B, spec: &DaemonSpec) -> Result<Outcome> {
    let desired = b.render(spec)?.to_string();
    let found = classify(b, &spec.id, b.read(&spec.id)?);
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
    let (blob, snapshot) = require_marked(id, b.read(id)?)?;
    let blob = blob?;
    b.render(&blob.spec)
        .map_err(|e| Error::Other(format!("`{id}`'s embedded spec cannot be regenerated: {e}")))?;
    b.live_status(id, &snapshot)
}

pub(crate) fn list<B: Backend>(b: &B) -> Result<Vec<Installed>> {
    let scan = b.scan();
    let mut out = Vec::new();
    for (name, read) in scan.entries {
        let (blob, snapshot) = match read {
            ReadOutcome::Absent | ReadOutcome::Foreign { .. } => continue,
            ReadOutcome::Marked { blob, snapshot } => (blob, snapshot),
        };
        let unreadable = |reason: String| Installed::OursUnreadable {
            name: name.clone(),
            reason,
        };
        let blob = match blob {
            Ok(blob) => blob,
            Err(e) => {
                out.push(unreadable(e.to_string()));
                continue;
            }
        };
        if let Err(e) = b.render(&blob.spec) {
            out.push(unreadable(format!(
                "marked ours, but its embedded spec cannot be regenerated: {e}"
            )));
            continue;
        }
        // One entry's failure must not take down the listing of every other daemon, except that
        // no manager to ask is every entry's.
        let id = match Id::try_from(name.as_str()) {
            Ok(id) => id,
            Err(e) => {
                out.push(unreadable(format!(
                    "marked ours, but its name is not a valid daemon id: {e}"
                )));
                continue;
            }
        };
        match b.live_status(&id, &snapshot) {
            Ok(status) => out.push(Installed::Ours {
                spec: blob.spec,
                state: status.state,
                pid: status.pid,
                enabled: status.enabled,
            }),
            Err(e @ Error::NoManager { .. }) => return Err(e),
            Err(e) => out.push(unreadable(format!(
                "marked ours, but its live state could not be queried: {e}"
            ))),
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
///   `detail("open", e)` names the failure.
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
    detail: impl Fn(&'static str, &io::Error) -> String,
    mut keep: impl FnMut(T),
) -> Option<Installed> {
    let mut resource = match opened {
        Ok(resource) => resource,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Installed::scan_incomplete(location, &detail("open", &e))),
    };
    for entry in entries(&mut resource) {
        match entry {
            Ok(item) => keep(item),
            Err(e) => return Some(Installed::scan_incomplete(location, &detail("enumerate", &e))),
        }
    }
    None
}

/// What [`scan`] reports of a directory it could not finish: `op` is `"open"` or `"enumerate"`.
/// One wording for every backend whose container is a directory.
#[cfg(unix)]
pub(crate) fn directory_detail(op: &'static str, e: &io::Error) -> String {
    match op {
        "open" => format!("failed to read directory: {e}"),
        _ => format!("failed to read a directory entry: {e}"),
    }
}

#[cfg(test)]
#[path = "flow_tests.rs"]
mod flow_tests;
