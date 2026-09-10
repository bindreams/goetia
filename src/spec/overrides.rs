//! The `backend-specific:` manifest key: a per-[`Backend`] set of field
//! overrides.
//!
//! A manifest author's reference:
//!
//! - Keyed by `systemd`, `launchd`, or `scm`. An unknown key is an error
//!   naming it; a repeated key is too.
//! - Every field but the daemon's id is overridable: `name`, `command`,
//!   `cwd`, `env`, `user`, `restart`, `restart-delay`, `logs`, `type`.
//! - A scalar or list field replaces the base value outright; `env` merges
//!   key by key instead, the override winning where both set the same key.
//! - An explicit YAML `null` is an error for all nine fields, and for both
//!   the keys and the values of `env`. `env: {A: ""}` stays a legal empty
//!   assignment — only `null` is refused, not emptiness.
//! - `${VAR}` is substituted only in the override for the backend actually
//!   being installed to; the other two are checked as authored, never
//!   substituted. Every backend's merged spec is validated on every host,
//!   but only the native one is completed and installed.
//!
//! Every field here is copied from `RawSpec` (`raw.rs`), not from
//! `DaemonSpec`: `user`, `restart`, `restart-delay` and `type` stay the
//! authored-text types (`RawUser`, `String`) so that `${VAR}` can still be
//! interpolated into them before `resolve` parses the result — see
//! `RawSpec`'s own field comments.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::Backend;
use super::raw::RawSpec;
use super::user::RawUser;

pub type BackendOverrides = BTreeMap<Backend, RawOverride>;

/// One `backend-specific.<backend>` entry: every field optional, since an
/// override only needs to name the fields it actually overrides. Merging
/// an override into a base `RawSpec` is [`RawSpec::merged_for`]'s job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOverride {
    #[serde(default)]
    pub name: Option<String>,
    // Same shape as `RawSpec::command`, now that both are optional.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub user: Option<RawUser>,
    #[serde(default)]
    pub restart: Option<String>,
    #[serde(rename = "restart-delay", default)]
    pub restart_delay: Option<String>,
    #[serde(default)]
    pub logs: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

/// Which of the nine fields a backend's override actually supplied.
/// Provenance the merged spec cannot carry: after the merge a `restart`
/// value is just a value, and `Backend::error` must not reject a base
/// value for a backend the author never wrote a line for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Supplied {
    pub name: bool,
    pub command: bool,
    pub cwd: bool,
    pub env: bool,
    pub user: bool,
    pub restart: bool,
    pub restart_delay: bool,
    pub logs: bool,
    pub kind: bool,
}

impl Supplied {
    /// No override at all: every field is a base value.
    pub const NONE: Supplied = Supplied {
        name: false,
        command: false,
        cwd: false,
        env: false,
        user: false,
        restart: false,
        restart_delay: false,
        logs: false,
        kind: false,
    };
}

impl RawOverride {
    /// Which of the nine fields this override sets. One `is_some()` per
    /// field, and `env` counts as supplied when the key is present even
    /// if the map it holds is empty — an empty `env:` under a backend is
    /// still something the author wrote.
    pub fn supplied(&self) -> Supplied {
        Supplied {
            name: self.name.is_some(),
            command: self.command.is_some(),
            cwd: self.cwd.is_some(),
            env: self.env.is_some(),
            user: self.user.is_some(),
            restart: self.restart.is_some(),
            restart_delay: self.restart_delay.is_some(),
            logs: self.logs.is_some(),
            kind: self.kind.is_some(),
        }
    }
}

impl RawSpec {
    /// `self` with `backend`'s override applied and no overrides left,
    /// plus which fields that override supplied.
    ///
    /// A scalar/vector field wholesale-replaces when the override sets it;
    /// `env` unions instead, the override winning per key. On a backend
    /// with no override entry this is exactly `self.without_overrides()`,
    /// paired with `Supplied::NONE` — see the module-level guard this
    /// exists to hold: a per-backend rejection may only ever be applied to
    /// a field the returned `Supplied` marks `true`.
    pub fn merged_for(&self, backend: Backend) -> (RawSpec, Supplied) {
        let Some(ovr) = self.backend_specific.get(&backend) else {
            return (self.without_overrides(), Supplied::NONE);
        };

        let mut env = self.env.clone();
        if let Some(ovr_env) = &ovr.env {
            env.extend(ovr_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        let merged = RawSpec {
            name: ovr.name.clone().or_else(|| self.name.clone()),
            command: ovr.command.clone().or_else(|| self.command.clone()),
            cwd: ovr.cwd.clone().or_else(|| self.cwd.clone()),
            env,
            user: ovr.user.clone().or_else(|| self.user.clone()),
            restart: ovr.restart.clone().or_else(|| self.restart.clone()),
            restart_delay: ovr.restart_delay.clone().or_else(|| self.restart_delay.clone()),
            logs: ovr.logs.clone().or_else(|| self.logs.clone()),
            kind: ovr.kind.clone().or_else(|| self.kind.clone()),
            backend_specific: BackendOverrides::new(),
        };

        (merged, ovr.supplied())
    }

    /// `self` with every override stripped and nothing else changed.
    pub fn without_overrides(&self) -> RawSpec {
        RawSpec {
            name: self.name.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            user: self.user.clone(),
            restart: self.restart.clone(),
            restart_delay: self.restart_delay.clone(),
            logs: self.logs.clone(),
            kind: self.kind.clone(),
            backend_specific: BackendOverrides::new(),
        }
    }
}

/// `RawSpec::backend_specific`: a hand-written `Visitor` over `MapAccess`,
/// in the shape of `raw.rs`'s `DaemonsVisitor` — a plain
/// `BTreeMap<Backend, RawOverride>` would silently keep the last of two
/// `scm:` blocks rather than rejecting the duplicate.
pub(super) fn deserialize_overrides<'de, D>(d: D) -> Result<BackendOverrides, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_map(OverridesVisitor)
}

struct OverridesVisitor;

impl<'de> Visitor<'de> for OverridesVisitor {
    type Value = BackendOverrides;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping of backend name to override")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut overrides = BTreeMap::new();

        while let Some(backend) = map.next_key::<Backend>()? {
            if overrides.contains_key(&backend) {
                return Err(de::Error::custom(format!("duplicate backend key `{backend}`")));
            }
            let value: RawOverride = map.next_value()?;
            overrides.insert(backend, value);
        }

        Ok(overrides)
    }
}

#[cfg(test)]
#[path = "overrides_tests.rs"]
mod overrides_tests;
