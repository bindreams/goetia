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
use super::support::load_and_warn;
use crate::decide::{self, Outcome};
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

    match mgr.preview_install(&spec) {
        Ok(Outcome::Create) => {
            let _ = writeln!(out, "{}: not installed (would be created)", spec.id);
            ExitCode::Drift
        }
        Ok(Outcome::UpToDate) => {
            let _ = writeln!(out, "{}: up to date", spec.id);
            ExitCode::Success
        }
        Ok(Outcome::Update { spec_diff }) => {
            let _ = writeln!(out, "{}:", spec.id);
            let _ = write!(out, "{spec_diff}");
            ExitCode::Drift
        }
        Ok(Outcome::Stale { from_version }) => {
            let _ = writeln!(
                out,
                "{}: would be regenerated (built by goetia {from_version})",
                spec.id
            );
            ExitCode::Drift
        }
        // The same two flavours `install` renders, in the subjunctive:
        // `--force` is named only where it would actually resolve this.
        Ok(Outcome::Conflict {
            artifact_diff,
            unclearable_recovery,
        }) => {
            let line = match &unclearable_recovery {
                None => format!(
                    "{}: would conflict (hand-edited outside goetia; `install --force` would overwrite it)",
                    spec.id
                ),
                Some(recovery) => format!("{}: would conflict. {recovery}", spec.id),
            };
            let _ = writeln!(out, "{line}");
            let _ = write!(out, "{artifact_diff}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Conflict
        }
        Ok(Outcome::RefuseForeign { recovery }) => {
            let line = format!(
                "{}: would be refused: not a goetia-managed service. {recovery}",
                spec.id
            );
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Error
        }
        Ok(Outcome::RefuseOtherDaemon { installed }) => {
            let line = format!(
                "{}: would be refused: {}",
                spec.id,
                decide::other_daemon_refusal(installed.as_str())
            );
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Error
        }
        Ok(Outcome::RefuseUnusable { reason, recovery }) => {
            let line = match recovery {
                Some(recovery) => format!("{}: would be refused: {reason}. {recovery}", spec.id),
                None => format!("{}: would be refused: {reason}", spec.id),
            };
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Indeterminate
        }
        Err(e) => {
            let _ = writeln!(err, "error: {}: {}", spec.id, e.report());
            ExitCode::of(&e)
        }
    }
}
