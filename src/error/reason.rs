//! [`Reason`]: why goetia says what it does about an id it cannot use or classify.

use std::fmt::{self, Write as _};
use std::sync::Arc;

use super::Error;
use super::report::{Sink, write_chain, write_remedies};

/// Why goetia says what it does about an id it cannot use or classify: a refused install, or a
/// `list` entry. It is cheap to clone, and two reasons are equal when they read the same.
///
/// `Display` prints it as [`Error::report`] prints an error.
#[derive(Debug, Clone)]
pub enum Reason {
    /// goetia's own words, with no error under them.
    Said(String),
    /// The error goetia failed on, and nothing else.
    Failed(Arc<Error>),
    /// The error goetia failed on, set inside its own words: `before`, the error's report, `after`.
    Around {
        before: String,
        error: Arc<Error>,
        after: String,
    },
}

impl Reason {
    pub fn said(text: impl Into<String>) -> Self {
        Reason::Said(text.into())
    }

    pub fn failed(error: impl Into<Arc<Error>>) -> Self {
        Reason::Failed(error.into())
    }

    /// `what`, then the error under it: `what: error`.
    pub fn because(what: impl Into<String>, error: Error) -> Self {
        Reason::failed(Error::Step {
            what: what.into(),
            source: Box::new(error),
        })
    }

    pub fn around(before: impl Into<String>, error: impl Into<Arc<Error>>, after: impl Into<String>) -> Self {
        Reason::Around {
            before: before.into(),
            error: error.into(),
            after: after.into(),
        }
    }

    /// The error classification reads (see [`Error::cause`]), when there is one.
    pub fn cause(&self) -> Option<&Error> {
        match self {
            Reason::Said(_) => None,
            Reason::Failed(error) | Reason::Around { error, .. } => Some(error.cause()),
        }
    }
}

impl fmt::Display for Reason {
    /// As [`Error::report`] prints an error: the control and direction characters of every part
    /// are written as `\u{XXXX}`. An [`Around`](Reason::Around) error's remedies print after
    /// `after`, as `report` prints them after a chain.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_to(&mut Sink::new(f, true))
    }
}

impl Reason {
    /// `Display` with every character as it is: what `--json` carries, and what two reasons are
    /// compared by.
    pub(crate) fn unescaped(&self) -> impl fmt::Display + '_ {
        Unescaped(self)
    }

    /// `Display`, written into `out`, so what a caller writes next is placed after a stderr this
    /// ends on as the report places it.
    pub(crate) fn write_to(&self, out: &mut Sink) -> fmt::Result {
        let (before, error, after) = match self {
            Reason::Said(text) => ("", None, text.as_str()),
            Reason::Failed(error) => ("", Some(error), ""),
            Reason::Around { before, error, after } => (before.as_str(), Some(error), after.as_str()),
        };
        let mut remedies = Vec::new();
        out.write_str(before)?;
        if let Some(error) = error {
            write_chain(error.as_ref(), out, &mut remedies)?;
        }
        out.write_str(after)?;
        write_remedies(out, &remedies)
    }
}

struct Unescaped<'a>(&'a Reason);

impl fmt::Display for Unescaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.write_to(&mut Sink::new(f, false))
    }
}

impl PartialEq for Reason {
    fn eq(&self, other: &Self) -> bool {
        self.unescaped().to_string() == other.unescaped().to_string()
    }
}

impl Eq for Reason {}

#[cfg(test)]
#[path = "reason_tests.rs"]
mod reason_tests;
