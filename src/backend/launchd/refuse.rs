//! When `install` must not write over a plist launchd has loaded, and the errors of a `run` that could not
//! be carried through. Pure, so it compiles and its tests run on every platform, unlike `manager`.
//!
//! launchd keeps the definition a job was bootstrapped with and has no way to update it. A plist
//! rewritten under a loaded job would leave `start` kickstarting the old definition and reporting
//! `Ok`, so `install` refuses the write instead of deferring it as systemd and SCM do.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::Path;

use crate::backend::launchd::generate;
use crate::error::{Error, Occupant, Result};
use crate::spec::{DaemonKind, Id};

/// Whether writing `desired` over `on_disk` would give the plist other content, which is what a
/// loaded job was bootstrapped from. `install` asks it of an `Update` or `Stale` write; `--force`
/// turns a `Conflict` into an `Update`.
///
/// Compared without goetia's metadata comment ([`generate::without_metadata`]), which holds the spec
/// and version: the check follows the rendering, not the spec.
pub(crate) fn changes_loaded_job(on_disk: &str, desired: &str) -> bool {
    generate::without_metadata(on_disk) != generate::without_metadata(desired)
}

/// The error for a changed plist over a loaded job of `kind`. A service's says "loaded", not
/// "running": a job that has exited is still loaded until it is booted out. A task's names its run
/// in flight, the only thing that makes a task's plist unreplaceable.
pub(crate) fn loaded_refusal(id: &Id, kind: DaemonKind) -> Error {
    Error::LoadedJob {
        id: id.to_string(),
        kind,
    }
}

/// The error for a job that is loaded after `install` wrote its plist, though it was not before.
/// Which definition the job read is not known, so this is not a success. `restore` is the result of
/// putting the plist back.
pub(crate) fn loaded_after_write(id: &Id, restore: Result<()>) -> Error {
    Error::LoadedJobInDoubt {
        id: id.to_string(),
        restore_failed: restore.err().map(Box::new),
    }
}

/// The error for a recheck after the write that could not tell whether the job is loaded: `read` is
/// the failed read. As in doubt as [`loaded_after_write`], and not a failure that left nothing
/// written, which exit `1` would claim.
pub(crate) fn state_unknown_after_write(id: &Id, read: Error, restore: Result<()>) -> Error {
    Error::LoadedStateInDoubt {
        id: id.to_string(),
        read_failed: Box::new(read),
        restore_failed: restore.err().map(Box::new),
    }
}

/// The error for a plist that was taken from `Create` after a stale job was booted out: `found` is
/// what is at `path` now.
pub(crate) fn slot_taken_after_bootout(id: &Id, path: &Path, found: Occupant) -> Error {
    Error::StaleJobBootedOut {
        id: id.to_string(),
        path: path.to_path_buf(),
        found,
    }
}

/// The error for a plist that was taken, or removed, under `install` after it had made a `launchctl`
/// call. A second pass would need more calls than `install` reserved, so the install ends. Mostly
/// nothing was sent and no plist written; a task's `Create` that finds its timer's name taken has
/// written the job's plist already, which the re-run reads as goetia's and finishes.
pub(crate) fn changed_during_install(id: &Id, path: &Path) -> Error {
    Error::ConcurrentChange {
        id: id.to_string(),
        path: Some(path.to_path_buf()),
    }
}

// A run `begin_run` could not carry through ===========================================================================

/// `source`, a failure after `launchctl debug` held the next launch of `id`: the run was not started, and
/// the hold stays until a launch consumes it. A launch held and not resumed waits stopped, so the error
/// says what ends it. The cause stays under it, so a request left in doubt is still indeterminate, and
/// then the run may have started: only a failure that left nothing in doubt says it did not.
pub(crate) fn stranded_hold(id: &Id, source: Error) -> Error {
    let run = if source.is_indeterminate() {
        "the run may have started"
    } else {
        "the run was not started"
    };
    Error::Step {
        what: format!(
            "`launchctl debug` held the next launch of `{id}` and {run}: that launch, a \
             scheduled run or the next `run`, may wait stopped until `goetia daemon stop {id}` or `goetia \
             daemon run {id}` ends it"
        ),
        source: Box::new(source),
    }
}

/// The held launch of `id` was gone, or not the job's any more, by the time goetia could watch it.
pub(crate) fn run_unwatched(id: &Id) -> Error {
    Error::RunUnrecorded {
        id: id.to_string(),
        reason: "it was ended before goetia could watch it (a `stop`, a `kill`, or the system)".to_string(),
    }
}

/// The retry of a `run` of `id` started `pid`, which was not held: the launch `debug` held was taken and
/// ended unwatched, and a launch after a pending hold is held, so a second slipped-in run cannot be what
/// `kickstart -p` named. It is not followed; the run is unrecorded, as one that ended unwatched is.
pub(crate) fn run_unfollowed(id: &Id, pid: u32) -> Error {
    Error::Step {
        what: format!(
            "the launch `launchctl debug` held for this run of `{id}` was taken and ended before goetia could \
             watch it, and the start goetia made after it launched process {pid}, which goetia does not follow"
        ),
        source: Box::new(run_unwatched(id)),
    }
}

/// The held launch of `id` was gone by the time goetia could resume it.
pub(crate) fn run_unresumed(id: &Id) -> Error {
    Error::RunUnrecorded {
        id: id.to_string(),
        reason: "it was ended before goetia could resume it".to_string(),
    }
}

/// A wait for the run of `id` that failed: the run may still be going, so this is in doubt, as a request
/// goetia lost track of is.
pub(crate) fn run_lost(id: &Id, source: Error) -> Error {
    Error::RequestInDoubt {
        request: format!("launchctl kickstart -p system/{id}"),
        reached: true,
        note: Some("its run could not be waited for".to_string()),
        source: Box::new(source),
    }
}

#[cfg(test)]
#[path = "refuse_tests.rs"]
mod refuse_tests;
