//! When `install` must not write over a plist launchd has loaded. Pure, so it compiles and its tests
//! run on every platform, unlike [`manager`](crate::backend::launchd::manager).
//!
//! launchd keeps the definition a job was bootstrapped with and has no way to update it. A plist
//! rewritten under a loaded job would leave `start` kickstarting the old definition and reporting
//! `Ok`, so `install` refuses the write instead of deferring it as systemd and SCM do.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::Path;

use crate::backend::launchd::generate;
use crate::error::{Error, Result};
use crate::spec::Id;

/// Whether writing `desired` over `on_disk` would give the plist other content, which is what a
/// loaded job was bootstrapped from. `install` asks it of an `Update` or `Stale` write; `--force`
/// turns a `Conflict` into an `Update`.
///
/// Compared without goetia's metadata comment ([`generate::without_metadata`]), which holds the spec
/// and version: the check follows the rendering, not the spec.
pub(crate) fn changes_loaded_job(on_disk: &str, desired: &str) -> bool {
    generate::without_metadata(on_disk) != generate::without_metadata(desired)
}

/// The error for a changed plist over a loaded job. Says "loaded", not "running": a job that has
/// exited is still loaded until it is booted out.
pub(crate) fn loaded_refusal(id: &Id) -> Error {
    Error::Other(format!(
        "`{id}` is loaded in launchd, which cannot update a loaded job, so the changed plist was not \
         installed and the installed plist is untouched: run `goetia daemon stop {id}` (a job that has \
         exited is still loaded), re-run install, then start it"
    ))
}

/// The error for a job that is loaded after `install` wrote its plist, though it was not before.
/// Which definition the job read is not known, so this is not a success. `restore` is the result of
/// putting the plist back.
pub(crate) fn loaded_after_write(id: &Id, restore: Result<()>) -> Error {
    Error::LoadedJobInDoubt {
        id: id.to_string(),
        restore_failed: restore.err().map(|e| e.to_string()),
    }
}

/// The error for a recheck after the write that could not tell whether the job is loaded: `read` is
/// the failed read. As in doubt as [`loaded_after_write`], and not a failure that left nothing
/// written, which exit `1` would claim.
pub(crate) fn state_unknown_after_write(id: &Id, read: &Error, restore: Result<()>) -> Error {
    Error::LoadedStateInDoubt {
        id: id.to_string(),
        detail: read.to_string(),
        restore_failed: restore.err().map(|e| e.to_string()),
    }
}

/// The error for a plist that was taken from `Create` after a stale job was booted out: `found` is
/// what is at `path` now.
pub(crate) fn slot_taken_after_bootout(id: &Id, path: &Path, found: &str) -> Error {
    Error::StaleJobBootedOut {
        id: id.to_string(),
        path: path.to_path_buf(),
        found: found.to_owned(),
    }
}

/// The error for a plist that was taken, or removed, under `install` after it had made a `launchctl`
/// call but before it sent a request. A second pass would need more calls than `install` reserved, so
/// the install ends: nothing was sent and no plist written.
pub(crate) fn changed_during_install(id: &Id, path: &Path) -> Error {
    Error::Other(format!(
        "`{id}`: the plist at {} changed while `install` was running, so no plist was written and nothing \
         was sent to launchd: re-run install",
        path.display()
    ))
}

#[cfg(test)]
#[path = "refuse_tests.rs"]
mod refuse_tests;
