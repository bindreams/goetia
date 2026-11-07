//! `goetia daemon diff [-f FILE]`
//!
//! Read-only: never checks elevation. Always needs a manager, `-f` or not.
//!
//! Renders [`ServiceManager::preview_install`] — the same
//! [`crate::decide::decide`] call `install` uses — rather than reimplementing
//! that policy against `list()`'s output: `list()` excludes foreign services
//! and never carries the raw on-disk artifact text, so it could not tell
//! "absent" from "occupied by a stranger's service" or "up to date" from
//! "hand-edited outside Goetia".

use std::io::Write;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::refusal::{self, Mood};
use super::support::load_and_warn;
use crate::decide::Outcome;
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to goetia.yaml, or a directory containing it.
    #[arg(short = 'f', long = "file", default_value = ".")]
    pub file: PathBuf,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let spec = match load_and_warn(&args.file, err) {
        Ok(spec) => spec,
        Err(e) => {
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };

    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };

    let outcome = match mgr.preview_install(&spec) {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = writeln!(err, "error: {}: {}", spec.id, e.report());
            return ExitCode::of(&e);
        }
    };
    if let Some(code) = refusal::report(&spec.id, &outcome, Mood::Would, out, err) {
        return code;
    }
    match outcome {
        Outcome::Create => {
            let _ = writeln!(out, "{}: not installed (would be created)", spec.id);
        }
        Outcome::UpToDate => {
            let _ = writeln!(out, "{}: up to date", spec.id);
            return ExitCode::Success;
        }
        Outcome::Update { spec_diff } => {
            let _ = writeln!(out, "{}:", spec.id);
            let _ = write!(out, "{spec_diff}");
        }
        Outcome::Stale { from_version } => {
            let _ = writeln!(
                out,
                "{}: would be regenerated (built by goetia {from_version})",
                spec.id
            );
        }
        Outcome::Conflict { .. }
        | Outcome::RefuseForeign { .. }
        | Outcome::RefuseOtherDaemon { .. }
        | Outcome::RefuseUnusable { .. } => unreachable!("`refusal::report` wrote {outcome:?}"),
    }
    ExitCode::Drift
}
