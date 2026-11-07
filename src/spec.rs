//! `goetia.yaml`: types, parsing, resolution, and the injection validation
//! gate.
//!
//! `RawSpec -> DaemonSpec` (see `resolve`) is the crate's parse-don't-validate
//! boundary. A [`DaemonSpec`] has passed every field rule, so nothing
//! downstream re-derives them; only [`DaemonSpec::new`] makes one, from the
//! plain-data [`DaemonFields`]. [`load`] and `blob::decode` go through it.
//!
//! A daemon may also carry a `backend-specific:` block overriding its
//! fields per [`BackendKind`]; see `overrides`'s module doc for the
//! manifest-facing reference.

mod backend;
mod field;
mod interpolate;
mod no_null;
mod overrides;
mod raw;
mod resolve;
mod user;
mod vars;

#[cfg(test)]
pub(crate) mod test_support;

pub use backend::BackendKind;
pub use resolve::load;
pub use user::{AccountId, User};

// Read by `crate::backend::scm::generate` and `crate::backend::systemd::generate`, which cannot see through the
// private `mod backend;`.
// `windows_only_account` stays out of this line: only `BackendKind::error` reads it, inside
// `spec::backend`.
pub(crate) use backend::{
    scm_restart_delay, systemd_command_defect, systemd_env_key_defect, systemd_path_defect, windows_builtin,
};
// Named from `generate_tests.rs` alone.
#[cfg(test)]
pub(crate) use backend::MAX_SC_ACTION_DELAY;
// Named from `generate_tests.rs` alone; production code only infers it from `windows_builtin`.
#[allow(unused_imports)]
pub(crate) use backend::Builtin;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::Error;

// Id ==================================================================================================================

/// Every daemon id (and only a daemon id) must match this pattern: ASCII
/// letters, digits, `.`, `_`, `-`, 1 to 80 characters. Loose enough for a
/// docker-compose-style key, tight enough that the id is always usable
/// unescaped as a systemd unit name, a launchd label component, and an SCM
/// service name. Ids differing only in case are both valid; where the manager
/// does not tell them apart, `install` refuses the second (see
/// [`crate::decide::Outcome::RefuseOtherDaemon`]).
fn is_valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A daemon id that has already been checked against [`is_valid_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Id(String);

impl Id {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Id {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        if is_valid_id(&value) {
            Ok(Id(value))
        } else {
            Err(Error::Invalid {
                daemon: value.clone(),
                message: format!("id `{value}` does not match the required pattern ^[A-Za-z0-9._-]{{1,80}}$"),
                source: None,
            })
        }
    }
}

impl TryFrom<&str> for Id {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Error> {
        Id::try_from(value.to_owned())
    }
}

impl std::str::FromStr for Id {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        Id::try_from(s)
    }
}

// DaemonSpec ==========================================================================================================

/// What a daemon is, before goetia checks it: plain data a consumer fills in and passes to
/// [`DaemonSpec::new`]. It has no methods, so nothing reaches a [`DaemonSpec`] through `Deref`
/// that is not a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonFields {
    pub id: Id,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub user: User,
    pub restart: Restart,
    pub restart_delay: Option<Duration>,
    pub logs: Option<PathBuf>,
    pub kind: Kind,
}

/// A daemon goetia has checked: it passed every field rule. Only [`DaemonSpec::new`] makes one;
/// fields read through `Deref`, there is no `DerefMut`.
///
/// A generator may rely on all of these:
/// - `command` is non-empty and `command[0]` is an absolute path;
/// - `cwd` and `logs`, when set, are absolute paths;
/// - no emitted field (`name`, `command`, `cwd`, `logs`, `env` keys and values, `user`) holds a
///   control character or an XML noncharacter;
/// - `cwd` and `logs` are valid UTF-8;
/// - every `env` key is non-empty and has no `=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSpec(DaemonFields);

impl DaemonSpec {
    /// Check `fields` against the field rules and wrap them. Fails with [`Error::Invalid`] naming
    /// the field.
    pub fn new(fields: DaemonFields) -> Result<DaemonSpec, Error> {
        resolve::check_fields(&fields)?;
        Ok(DaemonSpec(fields))
    }

    /// Give the fields back, to build a variant and check it with [`DaemonSpec::new`].
    pub fn into_fields(self) -> DaemonFields {
        self.0
    }
}

impl std::ops::Deref for DaemonSpec {
    type Target = DaemonFields;

    fn deref(&self) -> &DaemonFields {
        &self.0
    }
}

/// Restart policy. Corresponds to systemd's `Restart=`, launchd's
/// `KeepAlive`, and SCM recovery actions.
///
/// Parsed from its authored kebab-case string by `resolve::parse_restart`,
/// not derived `Deserialize`: `RawSpec::restart` is a `String` so a `${VAR}`
/// reference can be interpolated before this type is produced; a derived
/// `Deserialize` would fail on the unresolved placeholder at YAML-parse time.
///
/// The scm generator refuses `Always`, whose recovery actions never fire after a clean exit. For
/// scm it survives only in blobs installed before that refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Never,
    OnFailure,
    Always,
}

/// Whether Goetia runs the command directly (via `goetia-shim` on Windows)
/// or the command is itself expected to behave as a native service.
///
/// Parsed from its authored kebab-case string by `resolve::parse_kind`; see
/// [`Restart`]'s doc comment for why this is not a derived `Deserialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Simple,
    Managed,
}

/// A non-fatal parse/resolve-time advisory: a property was accepted but
/// cannot be faithfully honored on some platform, or will be silently
/// transformed (e.g. a sub-second `restart-delay` rounded up for launchd).
/// Every CLI command that parses a manifest prints these to stderr.
///
/// `id` is `None` for a manifest-level advisory that belongs to the file
/// rather than the daemon — e.g. a drive-relative `-f` argument, resolved
/// by `resolve::absolutize` before any check of the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub id: Option<Id>,
    pub message: String,
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod spec_tests;
