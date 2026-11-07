//! The lines `install` and `diff` print when an outcome is a conflict or a refusal.

use std::io::Write;

use super::ExitCode;
use crate::decide::{self, Outcome};
use crate::error::escape;
use crate::spec::Id;

/// Whether the outcome happened or is the one the command would meet: `install` says `refused`,
/// `diff` says `would be refused`.
#[derive(Debug, Clone, Copy)]
pub(super) enum Mood {
    Done,
    Would,
}

/// Writes `outcome`, if it is a conflict or a refusal, and returns its exit code; `None`, writing
/// nothing, for any other. The line goes to `out` with the diff under it, and to `err` as
/// `error: <line>`, so a script that only captures stderr still learns of it.
///
/// A line is written whole through the strict writer: its `recovery` and `reason` come from the
/// artifact and the host, and a line feed in either would start a line of its own.
pub(super) fn report(
    id: &Id,
    outcome: &Outcome,
    mood: Mood,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<ExitCode> {
    let (line, diff, code) = match outcome {
        Outcome::Conflict {
            artifact_diff,
            unclearable_recovery,
        } => {
            // `--force` is named only where it resolves the conflict. When part of the cause is
            // outside the directory this backend writes, forcing leaves that part in place and the
            // next run reports the same conflict; `decide` says which case this is.
            let line = match (unclearable_recovery, mood) {
                (None, Mood::Done) => format!("{id}: conflict (re-run with --force to overwrite)"),
                (None, Mood::Would) => {
                    format!("{id}: would conflict (hand-edited outside goetia; `install --force` would overwrite it)")
                }
                (Some(recovery), Mood::Done) => format!("{id}: conflict. {recovery}"),
                (Some(recovery), Mood::Would) => format!("{id}: would conflict. {recovery}"),
            };
            (line, artifact_diff.as_str(), ExitCode::Conflict)
        }
        Outcome::RefuseForeign { recovery } => (
            format!("{id}: {}: not a goetia-managed service. {recovery}", refused(mood)),
            "",
            ExitCode::Error,
        ),
        Outcome::RefuseOtherDaemon { installed } => (
            format!(
                "{id}: {}: {}",
                refused(mood),
                decide::other_daemon_refusal(installed.as_str())
            ),
            "",
            ExitCode::Error,
        ),
        Outcome::RefuseUnusable { reason, recovery } => {
            let line = match recovery {
                Some(recovery) => format!("{id}: {}: {reason}. {recovery}", refused(mood)),
                None => format!("{id}: {}: {reason}", refused(mood)),
            };
            // `install` failed to install; `diff` could not answer.
            let code = match mood {
                Mood::Done => ExitCode::Error,
                Mood::Would => ExitCode::Indeterminate,
            };
            (line, "", code)
        }
        Outcome::Create | Outcome::UpToDate | Outcome::Update { .. } | Outcome::Stale { .. } => return None,
    };
    let line = escape(&line);
    let _ = writeln!(out, "{line}");
    let _ = write!(out, "{diff}");
    let _ = writeln!(err, "error: {line}");
    Some(code)
}

fn refused(mood: Mood) -> &'static str {
    match mood {
        Mood::Done => "refused",
        Mood::Would => "would be refused",
    }
}

#[cfg(test)]
#[path = "refusal_tests.rs"]
mod refusal_tests;
