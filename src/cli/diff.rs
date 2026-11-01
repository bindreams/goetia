//! `goetia daemon diff [-f FILE]`
//!
//! Read-only: never checks elevation. Always needs a manager (comparing
//! against installed state is the whole point), `-f` or not.
//!
//! Renders [`ServiceManager::preview_install`] — the exact same
//! [`crate::decide::decide`] call `install` itself uses — rather than
//! reimplementing a partial version of that policy against `list()`'s
//! output. That is deliberate, not merely tidy: `list()` excludes foreign
//! services and never carries the raw on-disk artifact text, so a
//! `list()`-based diff could not distinguish "absent" from "occupied by a
//! stranger's service" or "up to date" from "hand-edited outside Goetia" —
//! both real defects an earlier version of this module had.

use std::io::Write;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::support::load_and_warn;
use crate::decide::Outcome;
use crate::error::{Error, Result};
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
) -> i32 {
    let spec = match load_and_warn(&args.file, err) {
        Ok(spec) => spec,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    match mgr.preview_install(&spec) {
        Ok(Outcome::Create) => {
            let _ = writeln!(out, "{}: not installed (would be created)", spec.id);
            3
        }
        Ok(Outcome::UpToDate) => {
            let _ = writeln!(out, "{}: up to date", spec.id);
            0
        }
        Ok(Outcome::Update { spec_diff }) => {
            let _ = writeln!(out, "{}:", spec.id);
            let _ = write!(out, "{spec_diff}");
            3
        }
        Ok(Outcome::Stale { from_version }) => {
            let _ = writeln!(
                out,
                "{}: would be regenerated (built by goetia {from_version})",
                spec.id
            );
            3
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
            5
        }
        Ok(Outcome::RefuseForeign { recovery }) => {
            let line = format!(
                "{}: would be refused: not a goetia-managed service. {recovery}",
                spec.id
            );
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            1
        }
        Ok(Outcome::RefuseUnreadable { reason, recovery }) => {
            // `4`, not `1`: `diff` was asked a question and could not
            // determine the answer — indeterminate, not a failure.
            // `install` genuinely fails to install here instead, which
            // is the one row this deliberately differs from `install`.
            let line = match recovery {
                Some(recovery) => format!("{}: would be refused: {reason}. {recovery}", spec.id),
                None => format!("{}: would be refused: {reason}", spec.id),
            };
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            4
        }
        // `4`, for the same reason `RefuseUnreadable` above is `4`:
        // nothing was attempted and nothing refused — a read
        // `preview_install` needed in order to answer failed, so the
        // question stands unanswered. `1` would claim an operation ran
        // and failed. `install` returns `4` here too, through its own
        // `failure_code`: the two verbs agree about *this* error and
        // disagree only about `Outcome::RefuseUnreadable`, where
        // `install` genuinely did fail to install. See `dispatch`'s doc
        // comment, which owns the vocabulary.
        Err(e @ Error::Undetermined { .. }) => {
            let _ = writeln!(err, "error: {}: {e}", spec.id);
            4
        }
        Err(e) => {
            let _ = writeln!(err, "error: {}: {e}", spec.id);
            1
        }
    }
}
