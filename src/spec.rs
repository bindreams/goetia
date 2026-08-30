//! `goetia.yaml`: types, parsing, resolution, and the injection validation
//! gate.
//!
//! `RawSpec -> DaemonSpec` (see `resolve`) is the crate's parse-don't-validate
//! boundary: every `DaemonSpec` that exists has already passed every check
//! below, so nothing downstream — a generator, the metadata blob — needs to
//! re-derive them. There is no separate `validate()` function anywhere in
//! the crate; `resolve` is both parse and validate.

mod raw;
mod resolve;
mod user;

pub use raw::{RawManifest, RawSpec};
pub use resolve::{load, resolve};
pub use user::{AccountId, User};

// Re-exported so `blob::decode` can re-run the same injection-gate checks
// `resolve` uses, against a spec deserialized from an untrusted artifact,
// instead of duplicating the rules.
pub(crate) use resolve::{
    reject_control_chars, reject_empty_command, reject_env_key_with_equals, reject_relative_path,
};

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::error::Error;

// Id ==================================================================================================================

/// Every daemon id (and only a daemon id) must match this pattern: ASCII
/// letters, digits, `.`, `_`, `-`, 1 to 80 characters. Loose enough for a
/// docker-compose-style key, tight enough that the id is always usable
/// unescaped as a systemd unit name, a launchd label component, and an SCM
/// service name — and case-insensitively distinct, since a launchd label
/// and an SCM service name both live on case-insensitive filesystems/stores
/// on some hosts.
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

/// A fully resolved daemon: every relative path made absolute, every
/// default materialized, every user-supplied string checked for the
/// characters that would let it break out of a generated directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSpec {
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

/// Restart policy. Corresponds to systemd's `Restart=`, launchd's
/// `KeepAlive`, and (for `Kind::Managed`) SCM recovery actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Never,
    OnFailure,
    Always,
}

/// Whether Goetia runs the command directly (via `goetia-shim` on Windows)
/// or the command is itself expected to behave as a native service. See
/// the design spec's §2 mapping table for what each `Kind` can and cannot
/// express per platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Simple,
    Managed,
}

/// A non-fatal parse/resolve-time advisory: a property was accepted but
/// cannot be faithfully honored on some platform, or will be silently
/// transformed (e.g. a sub-second `restart-delay` rounded up for launchd).
/// Every CLI command that parses a manifest prints these to stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub id: Id,
    pub message: String,
}
