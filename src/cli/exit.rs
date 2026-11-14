//! The process exit code.

use crate::error::Error;
use crate::manager::Exit;

/// The process exit code, named as in the README's exit-code table.
///
/// Declared in precedence order, so `max()` combines the codes of several ids or steps: `Error` beats
/// `Indeterminate`, which beats `Conflict`, `Drift` and `Success`. `Usage` never combines with
/// another code; it is declared last only so the order is total. `Run` is `daemon run`'s own: the
/// program's code, which is never combined with another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExitCode {
    Success,
    Drift,
    Conflict,
    Indeterminate,
    Error,
    Usage,
    Run(i32),
}

impl ExitCode {
    /// The code of a failed operation: `Indeterminate` when [`Error::is_indeterminate`], else `Error`.
    pub(crate) fn of(e: &Error) -> ExitCode {
        if e.is_indeterminate() {
            ExitCode::Indeterminate
        } else {
            ExitCode::Error
        }
    }

    /// What `daemon run` exits with after a run: the program's code, or `128` plus the signal that
    /// ended it. A program's `4` and goetia's `4` are indistinguishable by design.
    pub(crate) fn of_run(exit: Exit) -> ExitCode {
        match exit {
            Exit::Code(code) => ExitCode::Run(code),
            Exit::Signal(signal) => ExitCode::Run(128 + signal),
        }
    }

    /// The number the process exits with.
    pub fn code(self) -> i32 {
        match self {
            ExitCode::Success => 0,
            ExitCode::Drift => 3,
            ExitCode::Conflict => 5,
            ExitCode::Indeterminate => 4,
            ExitCode::Error => 1,
            ExitCode::Usage => 2,
            ExitCode::Run(code) => code,
        }
    }
}

#[cfg(test)]
#[path = "exit_tests.rs"]
mod exit_tests;
