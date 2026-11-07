//! The lines `install` and `diff` print when an outcome is a conflict or a refusal.
//!
//! [`Lines::refusal`] matches every [`Outcome`] once, so the compiler checks that none of them is
//! missed and the two callers need no match of their own for the ones that are.

use std::fmt::Write as _;
use std::io::Write;

use super::ExitCode;
use crate::decide::{self, Outcome};
use crate::error::{Reason, Sink, escape};
use crate::spec::Id;

/// Whether the outcome happened or is the one the command would meet: `install` says `refused`,
/// `diff` says `would be refused`.
#[derive(Debug, Clone, Copy)]
pub(super) enum Mood {
    Done,
    Would,
}

/// Writes the line of a conflict or a refusal of `id`, and returns its exit code. The line goes to
/// `out` with the diff under it, and to `err` as `error: <line>`, so a script that only captures
/// stderr still learns of it.
///
/// A line is written as [`Error::report`](crate::error::Error::report) writes an error: goetia's
/// own words through the strict writer, since the `recovery` and the id come from the artifact and
/// the host and a line feed in either would start a line of its own, and a `reason`'s child's
/// stderr in the layout, with goetia's words after it on a line of their own.
pub(super) struct Lines<'a> {
    pub(super) id: &'a Id,
    pub(super) mood: Mood,
}

impl Lines<'_> {
    /// Writes the line of `outcome` when it is a conflict or a refusal and returns its exit code;
    /// writes nothing and returns `None` for every other outcome.
    pub(super) fn refusal(&self, outcome: &Outcome, out: &mut dyn Write, err: &mut dyn Write) -> Option<ExitCode> {
        Some(match outcome {
            Outcome::Conflict {
                artifact_diff,
                unclearable_recovery,
            } => self.conflict(artifact_diff, unclearable_recovery.as_deref(), out, err),
            Outcome::RefuseForeign { recovery } => self.foreign(recovery, out, err),
            Outcome::RefuseOtherDaemon { installed } => self.other_daemon(installed, out, err),
            Outcome::RefuseUnusable { reason, recovery } => self.unusable(reason, recovery.as_deref(), out, err),
            Outcome::Create | Outcome::UpToDate | Outcome::Update { .. } | Outcome::Stale { .. } => return None,
        })
    }

    fn conflict(
        &self,
        artifact_diff: &str,
        unclearable_recovery: Option<&str>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let id = self.id;
        // `--force` is named only where it resolves the conflict. When part of the cause is
        // outside the directory this backend writes, forcing leaves that part in place and the
        // next run reports the same conflict; `decide` says which case this is.
        let line = match (unclearable_recovery, self.mood) {
            (None, Mood::Done) => format!("{id}: conflict (re-run with --force to overwrite)"),
            (None, Mood::Would) => {
                format!("{id}: would conflict (hand-edited outside goetia; `install --force` would overwrite it)")
            }
            (Some(recovery), Mood::Done) => format!("{id}: conflict. {recovery}"),
            (Some(recovery), Mood::Would) => format!("{id}: would conflict. {recovery}"),
        };
        emit(&escape(&line), artifact_diff, out, err);
        ExitCode::Conflict
    }

    fn foreign(&self, recovery: &str, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let line = format!(
            "{}: {}: not a goetia-managed service. {recovery}",
            self.id,
            refused(self.mood)
        );
        emit(&escape(&line), "", out, err);
        ExitCode::Error
    }

    fn other_daemon(&self, installed: &Id, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let line = format!(
            "{}: {}: {}",
            self.id,
            refused(self.mood),
            decide::other_daemon_refusal(installed.as_str())
        );
        emit(&escape(&line), "", out, err);
        ExitCode::Error
    }

    fn unusable(&self, reason: &Reason, recovery: Option<&str>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let mut line = String::new();
        let mut sink = Sink::new(&mut line, true);
        let written = write!(sink, "{}: {}: ", self.id, refused(self.mood))
            .and_then(|()| reason.write_to(&mut sink))
            .and_then(|()| recovery.map_or(Ok(()), |recovery| write!(sink, ". {recovery}")));
        written.expect("writing to a String cannot fail");
        emit(&line, "", out, err);
        // `install` failed to install; `diff` could not answer.
        match self.mood {
            Mood::Done => ExitCode::Error,
            Mood::Would => ExitCode::Indeterminate,
        }
    }
}

/// Writes `line`, already escaped, to both streams.
fn emit(line: &str, diff: &str, out: &mut dyn Write, err: &mut dyn Write) {
    let _ = writeln!(out, "{line}");
    let _ = write!(out, "{diff}");
    let _ = writeln!(err, "error: {line}");
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
