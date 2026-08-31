//! Crate-wide error type.

use std::path::PathBuf;

/// Errors produced while reading, parsing, or resolving a `goetia.yaml`
/// manifest.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading `path` from disk failed.
    #[error("failed to read {path}: {source}", path = path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The document is not valid YAML, or fails a shape-level constraint
    /// checked during deserialization: a duplicate or case-insensitively
    /// colliding daemon id, or a malformed `user` field. `serde_yaml_ng`
    /// attaches a line/column to the message.
    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),

    /// A `resolve()`-time validation failure: an invalid id, a control
    /// character in a user-supplied string, an `=` in an env key, or an
    /// empty command. Also produced by `blob::decode`, which re-runs
    /// these same checks against a spec deserialized from an untrusted
    /// artifact.
    #[error("daemon `{daemon}`: {message}")]
    Invalid { daemon: String, message: String },

    /// A metadata blob (`blob::decode`) is not usable: malformed base64,
    /// malformed JSON, or a schema this build does not understand.
    /// Distinct from `Invalid`, which a *structurally valid* blob can
    /// still trigger once its envelope decodes cleanly.
    #[error("blob: {0}")]
    Blob(String),
}
