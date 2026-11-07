//! The lines `install` and `diff` print when an outcome is a conflict or a refusal.
//!
//! [`Lines::refusal`] matches every [`Outcome`] once, so the compiler checks that none of them is
//! missed, and hands the caller what is left: its own match covers only the outcomes that are not
//! refusals.

use std::fmt::{self, Write as _};
use std::io::Write;
use std::ops::ControlFlow;

use super::ExitCode;
use super::support::print_line;
use crate::decide::{self, Outcome};
use crate::error::{Reason, Sink};
use crate::spec::Id;

/// Whether the outcome happened or is the one the command would meet: `install` says `refused`,
/// `diff` says `would be refused`.
#[derive(Debug, Clone, Copy)]
pub(super) enum Mood {
    Done,
    Would,
}

/// An [`Outcome`] that is neither a conflict nor a refusal: the caller reports it.
pub(super) enum NotRefused<'a> {
    Create,
    UpToDate,
    Update { spec_diff: &'a str },
    Stale { from_version: &'a str },
}

/// The lines of a conflict or a refusal of `id`. A line is written as
/// [`Error::report`](crate::error::Error::report) writes an error: goetia's own words through the
/// strict writer, since the `recovery` and the id come from the artifact and the host and a line
/// feed in either would start a line of its own, and a `reason`'s child's stderr with its layout.
pub(super) struct Lines<'a> {
    pub(super) id: &'a Id,
    pub(super) mood: Mood,
}

impl Lines<'_> {
    /// For a conflict or a refusal: writes its line and breaks with the exit code. The line goes
    /// to `out` with the diff under it, and to `err` as `error: <line>`, so a script that only
    /// captures stderr still learns of it. Any other outcome writes nothing and continues with
    /// what the caller reports.
    pub(super) fn refusal<'o>(
        &self,
        outcome: &'o Outcome,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ControlFlow<ExitCode, NotRefused<'o>> {
        ControlFlow::Break(match outcome {
            Outcome::Conflict {
                artifact_diff,
                unclearable_recovery,
            } => self.conflict(artifact_diff, unclearable_recovery.as_deref(), out, err),
            Outcome::RefuseForeign { recovery } => self.foreign(recovery, out, err),
            Outcome::RefuseOtherDaemon { installed } => self.other_daemon(installed, out, err),
            Outcome::RefuseUnusable { reason, recovery } => self.unusable(reason, recovery.as_deref(), out, err),
            Outcome::Create => return ControlFlow::Continue(NotRefused::Create),
            Outcome::UpToDate => return ControlFlow::Continue(NotRefused::UpToDate),
            Outcome::Update { spec_diff } => return ControlFlow::Continue(NotRefused::Update { spec_diff }),
            Outcome::Stale { from_version } => return ControlFlow::Continue(NotRefused::Stale { from_version }),
        })
    }

    fn conflict(
        &self,
        artifact_diff: &str,
        unclearable_recovery: Option<&str>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        // `--force` is named only where it resolves the conflict (see `decide`).
        let line = match (unclearable_recovery, self.mood) {
            (None, Mood::Done) => "conflict (re-run with --force to overwrite)".to_string(),
            (None, Mood::Would) => {
                "would conflict (hand-edited outside goetia; `install --force` would overwrite it)".to_string()
            }
            (Some(recovery), Mood::Done) => format!("conflict. {recovery}"),
            (Some(recovery), Mood::Would) => format!("would conflict. {recovery}"),
        };
        self.emit(artifact_diff, out, err, |sink| sink.write_str(&line));
        ExitCode::Conflict
    }

    fn foreign(&self, recovery: &str, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let mood = refused(self.mood);
        self.emit("", out, err, |sink| {
            write!(sink, "{mood}: not a goetia-managed service. {recovery}")
        });
        ExitCode::Error
    }

    fn other_daemon(&self, installed: &Id, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let mood = refused(self.mood);
        self.emit("", out, err, |sink| {
            write!(sink, "{mood}: {}", decide::other_daemon_refusal(installed.as_str()))
        });
        ExitCode::Error
    }

    fn unusable(&self, reason: &Reason, recovery: Option<&str>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
        let mood = refused(self.mood);
        self.emit("", out, err, |sink| {
            write!(sink, "{mood}: ")?;
            reason.write_to(sink)?;
            recovery.map_or(Ok(()), |recovery| write!(sink, ". {recovery}"))
        });
        // `install` failed to install; `diff` could not answer.
        match self.mood {
            Mood::Done => ExitCode::Error,
            Mood::Would => ExitCode::Indeterminate,
        }
    }

    /// Writes the line `body` writes, led by the id, to both streams: to `out` with `diff` under
    /// it, and to `err` as `error: <line>`.
    fn emit(&self, diff: &str, out: &mut dyn Write, err: &mut dyn Write, body: impl Fn(&mut Sink) -> fmt::Result) {
        let id = Some(self.id.as_str());
        print_line(out, None, id, &body);
        let _ = write!(out, "{diff}");
        print_line(err, Some("error"), id, &body);
    }
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
