//! When `install` must not write over a plist launchd has loaded. Pure, so it compiles and its tests
//! run on every platform, unlike [`manager`](crate::backend::launchd::manager).
//!
//! launchd keeps the definition a job was bootstrapped with and has no way to update it. A plist
//! rewritten under a loaded job would leave `start` kickstarting the old definition and reporting
//! `Ok`, so `install` refuses the write instead of deferring it as systemd and SCM do.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::decide::{Outcome, Ownership};
use crate::error::Error;
use crate::spec::{DaemonSpec, Id};

/// Whether carrying out `outcome` would give the plist a spec other than the one the loaded job was
/// bootstrapped from.
///
/// `Stale` counts only when `spec` differs from the installed one: `decide` checks the version
/// before anything else, so a stale artifact can carry a changed spec, but a bare version bump
/// leaves the job's spec as it was and must not stop `install` from refreshing the artifact.
pub(crate) fn changes_loaded_job(outcome: &Outcome, found: &Ownership, spec: &DaemonSpec) -> bool {
    match outcome {
        // Also what `--force` turns a `Conflict` into.
        Outcome::Update { .. } => true,
        Outcome::Stale { .. } => matches!(found, Ownership::Ours { blob, .. } if blob.spec != *spec),
        Outcome::Create
        | Outcome::UpToDate
        | Outcome::Conflict { .. }
        | Outcome::RefuseForeign { .. }
        | Outcome::RefuseUnreadable { .. } => false,
    }
}

/// The error for a changed spec over a loaded job. Says "loaded", not "running": a job that has
/// exited is still loaded until it is booted out.
pub(crate) fn loaded_refusal(id: &Id) -> Error {
    Error::Other(format!(
        "`{id}` is loaded in launchd, which cannot update a loaded job, so its changed spec was not \
         installed and the installed plist is untouched: run `goetia daemon stop {id}` (a job that has \
         exited is still loaded), re-run install, then start it"
    ))
}

#[cfg(test)]
#[path = "refuse_tests.rs"]
mod refuse_tests;
