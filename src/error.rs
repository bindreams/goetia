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

    /// No daemon named `id` is managed by Goetia, per a [`ServiceManager`]
    /// mutating or querying verb (`uninstall`/`start`/`stop`/`enable`/
    /// `disable`/`status`/`show`) that needs one to already exist.
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("daemon `{id}` is not installed")]
    NotInstalled { id: String },

    /// Something exists at `id`, but it carries no Goetia marker at all —
    /// distinct from [`NotInstalled`](Error::NotInstalled), which means
    /// nothing is there. Every [`ServiceManager`] verb other than `install`
    /// refuses a foreign id with this, never `NotInstalled`: reporting a
    /// service that demonstrably exists on the machine as merely "not
    /// installed" sends the user looking for a missing service instead of
    /// to the actual remedy in `recovery` (identical wording to
    /// `decide::Outcome::RefuseForeign`'s, via
    /// [`crate::decide::foreign_recovery`]).
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("daemon `{id}` exists but is not managed by goetia: {recovery}")]
    Foreign { id: String, recovery: String },

    /// A mutating CLI subcommand was invoked without the elevation
    /// (root/Administrator) it requires. Never returned for `list`,
    /// `status`, `show`, `diff`, or `install --dry-run`, none of which
    /// mutate anything.
    #[error("`{subcommand}` requires elevation (root/Administrator): re-run as root or Administrator")]
    ElevationRequired { subcommand: String },

    /// [`crate::manager::native`] has no [`ServiceManager`] implementation
    /// for the running platform yet — true for every platform until Tasks
    /// 11-13 land. A message here, never a panic: a CLI user hitting this
    /// gets a diagnosable error instead of a crash.
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("no backend for {platform} yet")]
    UnsupportedPlatform { platform: String },

    /// A lower-level failure, annotated with context a call site adds
    /// rather than a dedicated variant of its own — e.g. `daemon restart`
    /// disclosing that a daemon is now stopped, not merely that starting it
    /// back up failed. Not a catch-all for new call sites to reach for by
    /// default: prefer a real variant when the failure recurs anywhere else.
    #[error("{0}")]
    Other(String),
}

/// This crate's `Result` alias, used throughout [`crate::manager`] and
/// [`crate::cli`].
pub type Result<T> = std::result::Result<T, Error>;
