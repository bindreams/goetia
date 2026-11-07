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
    /// An [`Around`](Reason::Around) error's remedies print after `after`, as [`Error::report`]
    /// prints them after a chain.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (before, error, after) = match self {
            Reason::Said(text) => return f.write_str(text),
            Reason::Failed(error) => ("", error, ""),
            Reason::Around { before, error, after } => (before.as_str(), error, after.as_str()),
        };
        let mut remedies = Vec::new();
        f.write_str(before)?;
        let mut out = Sink::new(f, false);
        write_chain(error.as_ref(), &mut out, &mut remedies)?;
        out.write_str(after)?;
        write_remedies(&mut out, &remedies)
    }
}

impl PartialEq for Reason {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

impl Eq for Reason {}

#[cfg(test)]
#[path = "reason_tests.rs"]
mod reason_tests;
