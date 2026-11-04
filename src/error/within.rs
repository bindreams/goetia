use std::io;

/// `source` said in `context`, with its kind kept. The failure under `context` is the error's
/// [`source`](std::error::Error::source), which [`Error::report`](crate::Error::report) prints
/// after it: `context: source`.
pub(crate) fn within(context: impl Into<String>, source: io::Error) -> io::Error {
    #[derive(Debug, thiserror::Error)]
    #[error("{context}")]
    struct Within {
        context: String,
        #[source]
        source: io::Error,
    }
    io::Error::new(
        source.kind(),
        Within {
            context: context.into(),
            source,
        },
    )
}

#[cfg(test)]
#[path = "within_tests.rs"]
mod within_tests;
