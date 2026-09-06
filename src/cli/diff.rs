//! `goetia daemon diff [ID...] [-f FILE]`
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

use super::report;
use super::support::{load_and_warn, select_by_ids};
use crate::decide::Outcome;
use crate::error::{Error, Result};
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to diff. With none, diffs every daemon in the file.
    pub ids: Vec<String>,
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
    let specs = match load_and_warn(&args.file, err) {
        Ok(specs) => specs,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let selected = match select_by_ids(&specs, &args.ids) {
        Ok(s) => s,
        Err(msg) => {
            let _ = writeln!(err, "error: {msg}");
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

    // Each outcome contributes at most one exit-code class, combined by
    // `report::precedence` — the same rule and function `list`/`status`
    // use, reused rather than re-derived here (see `dispatch`'s doc
    // comment for the vocabulary and the precedence order itself). Never
    // `max()` on the codes themselves: a `Create` sitting next to a
    // `Conflict` must not read back as `3` (drift only), even though `3`
    // is the smaller number.
    let mut codes: Vec<i32> = Vec::new();
    for spec in selected {
        match mgr.preview_install(spec) {
            Ok(Outcome::Create) => {
                let _ = writeln!(out, "{}: not installed (would be created)", spec.id);
                codes.push(3);
            }
            Ok(Outcome::UpToDate) => {
                let _ = writeln!(out, "{}: up to date", spec.id);
            }
            Ok(Outcome::Update { spec_diff }) => {
                let _ = writeln!(out, "{}:", spec.id);
                let _ = write!(out, "{spec_diff}");
                codes.push(3);
            }
            Ok(Outcome::Stale { from_version }) => {
                let _ = writeln!(
                    out,
                    "{}: would be regenerated (built by goetia {from_version})",
                    spec.id
                );
                codes.push(3);
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
                codes.push(5);
            }
            Ok(Outcome::RefuseForeign { recovery }) => {
                let line = format!(
                    "{}: would be refused: not a goetia-managed service. {recovery}",
                    spec.id
                );
                let _ = writeln!(out, "{line}");
                let _ = writeln!(err, "error: {line}");
                codes.push(1);
            }
            Ok(Outcome::RefuseUnreadable { reason, recovery }) => {
                // `4`, not `1`: `diff` was asked a question and could not
                // determine the answer — indeterminate, not a failure.
                // `install` genuinely fails to install here instead, which
                // is the one row this deliberately differs from `install`.
                let line = format!("{}: would be refused: {reason}. {recovery}", spec.id);
                let _ = writeln!(out, "{line}");
                let _ = writeln!(err, "error: {line}");
                codes.push(4);
            }
            // `4`, for the same reason `RefuseUnreadable` above is `4`:
            // nothing was attempted and nothing refused — a read
            // `preview_install` needed in order to answer failed, so the
            // question stands unanswered. `1` would claim an operation ran
            // and failed. `install` keeps this at `1` on purpose: there the
            // operation is the install, and it genuinely did not happen —
            // the same row the two verbs already disagree on.
            Err(e @ Error::Undetermined { .. }) => {
                let _ = writeln!(err, "error: {}: {e}", spec.id);
                codes.push(4);
            }
            Err(e) => {
                let _ = writeln!(err, "error: {}: {e}", spec.id);
                codes.push(1);
            }
        }
    }
    codes
        .into_iter()
        .max_by_key(|code| report::precedence(*code))
        .unwrap_or(0)
}
