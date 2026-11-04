//! The process exit code.

use crate::error::Error;

/// The process exit code, named as in the README's exit-code table.
///
/// Declared in precedence order, so `max()` combines the codes of several ids or steps: `Error` beats
/// `Indeterminate`, which beats `Conflict`, `Drift` and `Success`. `Usage` never combines with
/// another code; it is declared last only so the order is total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExitCode {
    Success,
    Drift,
    Conflict,
    Indeterminate,
    Error,
    Usage,
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

    /// The number the process exits with.
    pub fn code(self) -> i32 {
        match self {
            ExitCode::Success => 0,
            ExitCode::Drift => 3,
            ExitCode::Conflict => 5,
            ExitCode::Indeterminate => 4,
            ExitCode::Error => 1,
            ExitCode::Usage => 2,
        }
    }
}

#[cfg(test)]
#[path = "exit_tests.rs"]
mod exit_tests;
