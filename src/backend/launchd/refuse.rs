//! When `install` must not write over a plist launchd has loaded. Pure, so it compiles and its tests
//! run on every platform, unlike [`manager`](crate::backend::launchd::manager).
//!
//! launchd keeps the definition a job was bootstrapped with and has no way to update it. A plist
//! rewritten under a loaded job would leave `start` kickstarting the old definition and reporting
//! `Ok`, so `install` refuses the write instead of deferring it as systemd and SCM do.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::backend::launchd::generate;
use crate::decide::Outcome;
use crate::error::Error;
use crate::spec::Id;

/// Whether carrying out `outcome` would give the plist other content than the one on disk, which is
/// what a loaded job was bootstrapped from.
///
/// Compared are the plist to be written (`desired`) and `on_disk`, each without goetia's own
/// metadata comment ([`generate::without_metadata`]): it holds the spec and the version, and a
/// change in either alone leaves what launchd reads as it was. So the check follows the rendering,
/// not the spec. A spec change the plist does not show, or a release whose generator renders the
/// same text, does not stop `install` from refreshing the artifact; a generator change or another
/// resolved account for an unchanged spec does.
pub(crate) fn changes_loaded_job(outcome: &Outcome, on_disk: Option<&str>, desired: &str) -> bool {
    match outcome {
        // `Update` is also what `--force` turns a `Conflict` into. Both are `Ours`, so there is
        // text on disk; none read as a change.
        Outcome::Update { .. } | Outcome::Stale { .. } => match on_disk {
            Some(on_disk) => generate::without_metadata(on_disk) != generate::without_metadata(desired),
            None => true,
        },
        Outcome::Create
        | Outcome::UpToDate
        | Outcome::Conflict { .. }
        | Outcome::RefuseForeign { .. }
        | Outcome::RefuseUnreadable { .. } => false,
    }
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
/// The write cannot be undone, and which definition the job read is not known, so this is not a
/// success.
pub(crate) fn loaded_after_write(id: &Id) -> Error {
    Error::LoadedJobInDoubt { id: id.to_string() }
}

/// The error for a recheck after the write that could not tell whether the job is loaded: `read` is
/// the failed read. As in doubt as [`loaded_after_write`], and not a failure that left nothing
/// written, which exit `1` would claim.
pub(crate) fn state_unknown_after_write(id: &Id, read: &Error) -> Error {
    Error::LoadedStateInDoubt {
        id: id.to_string(),
        detail: read.to_string(),
    }
}

#[cfg(test)]
#[path = "refuse_tests.rs"]
mod refuse_tests;
