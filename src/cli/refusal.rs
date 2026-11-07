//! The lines `install` and `diff` print when an outcome is a conflict or a refusal.
//!
//! [`Lines::refusal`] matches every [`Outcome`] once, so the compiler checks that none of them is
//! missed, and hands the caller what is left: its own match covers only the outcomes that are not
//! refusals.

use std::fmt::{self, Write as _};
use std::io::Write;

use super::ExitCode;
use super::support::render_line;
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
/// stderr still learns of it. A line a child's stderr breaks continues under its own prefix
/// (`frpc: ` on `out`, `error: frpc: ` on `err`).
///
/// An [`Outcome`] that is neither a conflict nor a refusal: the caller reports it.
pub(super) enum NotRefused<'a> {
    Create,
    UpToDate,
    Update { spec_diff: &'a str },
    Stale { from_version: &'a str },
}

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
    /// writes nothing and returns the outcome for every other one.
    pub(super) fn refusal<'o>(
        &self,
        outcome: &'o Outcome,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<ExitCode, NotRefused<'o>> {
        Ok(match outcome {
            Outcome::Conflict {
                artifact_diff,
                unclearable_recovery,
            } => self.conflict(artifact_diff, unclearable_recovery.as_deref(), out, err),
            Outcome::RefuseForeign { recovery } => self.foreign(recovery, out, err),
            Outcome::RefuseOtherDaemon { installed } => self.other_daemon(installed, out, err),
            Outcome::RefuseUnusable { reason, recovery } => self.unusable(reason, recovery.as_deref(), out, err),
            Outcome::Create => return Err(NotRefused::Create),
            Outcome::UpToDate => return Err(NotRefused::UpToDate),
            Outcome::Update { spec_diff } => return Err(NotRefused::Update { spec_diff }),
            Outcome::Stale { from_version } => return Err(NotRefused::Stale { from_version }),
        })
    }

    fn conflict(
        &self,
        artifact_diff: &str,
        unclearable_recovery: Option<&str>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> ExitCode {
        // `--force` is named only where it resolves the conflict. When part of the cause is
        // outside the directory this backend writes, forcing leaves that part in place and the
        // next run reports the same conflict; `decide` says which case this is.
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
    /// it, and to `err` as `error: <line>`. Each stream's line continues under its own prefix.
    fn emit(&self, diff: &str, out: &mut dyn Write, err: &mut dyn Write, body: impl Fn(&mut Sink) -> fmt::Result) {
        let scope = format!("{}: ", escape(self.id.as_str()));
        let _ = writeln!(out, "{}", render_line(&scope, &body));
        let _ = write!(out, "{diff}");
        let _ = writeln!(err, "{}", render_line(&format!("error: {scope}"), &body));
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
