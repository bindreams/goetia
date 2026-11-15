//! When `install` must not write over a plist launchd has loaded, and the errors of a `run` that could not
//! be carried through. Pure, so it compiles and its tests run on every platform, unlike `manager`.
//!
//! launchd keeps the definition a job was bootstrapped with and has no way to update it. A plist
//! rewritten under a loaded job would leave `start` kickstarting the old definition and reporting
//! `Ok`, so `install` refuses the write instead of deferring it as systemd and SCM do.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io;
use std::path::Path;

use crate::backend::launchd::generate;
use crate::backend::launchd::state::exit_of;
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
/// call. A second pass would need more calls than `install` reserved, so the install ends, and
/// whatever it had done by then stays (a bootout it had sent, and for a task's `Create` the job's
/// plist). The re-run starts from what it then reads.
pub(crate) fn changed_during_install(id: &Id, path: &Path) -> Error {
    Error::ConcurrentChange {
        id: id.to_string(),
        path: Some(path.to_path_buf()),
    }
}

// A run `begin_run` could not carry through ===========================================================================

/// `source`, a failure after `launchctl debug` held the next launch of `id`: the run was not started, and
/// the hold stays until a launch consumes it. A launch held and not resumed waits stopped, and `run`
/// refuses it as running, so the error says what ends it: a `stop` for a launch, a `run` for a hold no
/// launch has taken. The cause stays under it, so a request left in doubt is still indeterminate, and
/// then the run may have started: only a failure that left nothing in doubt says it did not.
pub(crate) fn stranded_hold(id: &Id, source: Error) -> Error {
    let run = if source.is_indeterminate() {
        "the run may have started"
    } else {
        "the run was not started"
    };
    Error::Step {
        what: format!(
            "`launchctl debug` held the next launch of `{id}` and {run}: a launch that took the hold waits \
             stopped until `goetia daemon stop {id}` ends it, and a hold no launch has taken is consumed by \
             the next `goetia daemon run {id}`"
        ),
        source: Box::new(source),
    }
}

/// The `kickstart -p` of a `run` of `id` launched `pid`, which no hold was on: the hold the `run` armed
/// was gone, and no hold is pending, because a pending one would have held this launch (KB, "macOS -
/// launchctl debug --start-suspended holds the next launch, whoever starts it"). It is not followed; the
/// run is unrecorded, as one that ended unwatched is.
pub(crate) fn run_unfollowed(id: &Id, pid: u32) -> Error {
    Error::RunUnrecorded {
        id: id.to_string(),
        reason: format!(
            "the `launchctl kickstart` goetia made for it launched process {pid} with no hold on it, so goetia \
             does not follow it"
        ),
    }
}

/// The launch of a `run` of `id` started after the `run`'s `debug` and ended before goetia could follow it:
/// gone at a read, or exited before its first exec. `status` is the `wait(2)` status the watch kept for
/// the process, where it kept one: the process's, which is not necessarily the program's. The run is
/// unrecorded; it may have happened.
pub(crate) fn run_ended_unfollowed(id: &Id, status: Option<i32>) -> Error {
    let status = match status {
        Some(raw) => format!(" (the process's status: {})", exit_of(raw)),
        None => String::new(),
    };
    Error::RunUnrecorded {
        id: id.to_string(),
        reason: format!(
            "its launch ended before goetia could follow it{status}: its program may have failed to start or \
             exited at once, or a `goetia daemon stop` or `kill` ended it"
        ),
    }
}

/// `pid`, which `kickstart -p` named for a `run` of `id`, was gone before goetia could watch it or read its
/// start, or `launchctl procinfo` did not name it as the job's launch: it was another process's. Whose
/// launch it was is not known: this run's, or a scheduled run's, so this is in doubt, as a request goetia
/// lost track of is. The source says only what is true of each of the three ways in.
pub(crate) fn run_vanished(id: &Id, pid: u32) -> Error {
    Error::RequestInDoubt {
        request: format!("launchctl kickstart -p system/{id}"),
        reached: true,
        note: Some(format!(
            "the launch it named, process {pid}, was gone, or its pid was another process's, before goetia \
             could watch it: if it was gone, it was this run's (its program may have failed to start, or a \
             `stop` or `kill` ended it), or a scheduled run that was in flight"
        )),
        source: Box::new(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} was gone, or was not the job's launch"),
        )),
    }
}

/// An interrupt ended the run of `id`: `signals` names the signals it can be (`SIGINT or SIGTERM`: only
/// those the run took over), `received` says what it was received before (`before goetia armed a hold`),
/// and `left` what is left. It is the `source` of a [`stranded_hold`]. It says what is known where the
/// interrupt was acted on, never when it came: it was received by then, and goetia sent nothing after
/// acting on it.
pub(crate) fn run_interrupted(id: &Id, signals: &str, received: &str, left: &str) -> Error {
    Error::os(
        format!("start the run of `{id}`"),
        io::Error::new(
            io::ErrorKind::Interrupted,
            format!(
                "interrupted by {signals}, received {received}; goetia sent nothing after acting on it; \
                 {left}"
            ),
        ),
    )
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
