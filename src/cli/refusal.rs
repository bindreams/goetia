//! The lines `install` and `diff` print when an outcome is a conflict or a refusal.
//!
//! One method per [`Outcome`] variant that is one, so each caller's single `match` over the outcome
//! names them and the compiler checks that none is missed.

use std::io::Write;

use super::ExitCode;
use crate::decide;
use crate::error::{Reason, escape};
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
/// A line is written whole through the strict writer: its `recovery` and `reason` come from the
/// artifact and the host, and a line feed in either would start a line of its own.
pub(super) struct Lines<'a> {
    pub(super) id: &'a Id,
    pub(super) mood: Mood,
}

impl Lines<'_> {
    pub(super) fn conflict(
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
        emit(&line, artifact_diff, out, err);
        ExitCode::Conflict
    }

    pub(super) fn foreign(&self, recovery: &str, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let line = format!(
            "{}: {}: not a goetia-managed service. {recovery}",
            self.id,
            refused(self.mood)
        );
        emit(&line, "", out, err);
        ExitCode::Error
    }

    pub(super) fn other_daemon(&self, installed: &Id, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let line = format!(
            "{}: {}: {}",
            self.id,
            refused(self.mood),
            decide::other_daemon_refusal(installed.as_str())
        );
        emit(&line, "", out, err);
        ExitCode::Error
    }

    pub(super) fn unusable(
        &self,
        reason: &Reason,
        recovery: Option<&str>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        let (id, refused) = (self.id, refused(self.mood));
        let line = match recovery {
            Some(recovery) => format!("{id}: {refused}: {reason}. {recovery}"),
            None => format!("{id}: {refused}: {reason}"),
        };
        emit(&line, "", out, err);
        // `install` failed to install; `diff` could not answer.
        match self.mood {
            Mood::Done => ExitCode::Error,
            Mood::Would => ExitCode::Indeterminate,
        }
    }
}

fn emit(line: &str, diff: &str, out: &mut dyn Write, err: &mut dyn Write) {
    let line = escape(line);
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
