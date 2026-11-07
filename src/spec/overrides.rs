//! The `backend-specific:` manifest key: a per-[`BackendKind`] set of field
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
//!   A key repeated *within* one `env` map, or colliding with another by
//!   case alone, is an error naming it — `raw.rs`'s check, reused here.
//! - An explicit YAML `null` is an error for all nine fields and for every
//!   leaf under them. That is `no_null`'s scan, not this module: the scan
//!   is schema-blind, so it walks an override like any other subtree, and
//!   `Slot` routes one exactly like the document it overrides.
//! - `${VAR}` is substituted only in the override for the backend actually
//!   being installed to; the other two are checked as authored, never
//!   substituted. A backend's own override is validated on every host,
//!   but only the native one is completed and installed.
//!
//! Fields mirror `RawSpec` (`raw.rs`), not `DaemonSpec`: `user`, `restart`,
//! `restart-delay` and `type` stay authored text so `${VAR}` can be
//! interpolated before `resolve` parses them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::BackendKind;
use super::field::Field;
use super::raw::{EnvVisitor, RawSpec};
use super::user::{RawUser, deserialize_user};

pub(super) type BackendOverrides = BTreeMap<BackendKind, RawOverride>;

/// One `backend-specific.<backend>` entry: every field optional, since an
/// override only needs to name the fields it actually overrides. Merging
/// an override into a base `RawSpec` is [`RawSpec::merged_for`]'s job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RawOverride {
    pub name: Option<String>,
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub user: Option<RawUser>,
    pub restart: Option<String>,
    pub restart_delay: Option<String>,
    pub logs: Option<String>,
    pub kind: Option<String>,
}

/// `RawOverride`'s serde shape; read only under `RawSpec::parse` (see `Wire`).
#[derive(Deserialize)]
#[serde(
    remote = "RawOverride",
    deny_unknown_fields,
    expecting = "a mapping of backend-specific overrides"
)]
struct OverrideWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "no_duplicate_env")]
    env: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_user")]
    user: Option<RawUser>,
    #[serde(default)]
    restart: Option<String>,
    #[serde(rename = "restart-delay", default)]
    restart_delay: Option<String>,
    #[serde(default)]
    logs: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

struct OverrideEntry(RawOverride);

impl<'de> Deserialize<'de> for OverrideEntry {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        OverrideWire::deserialize(d).map(OverrideEntry)
    }
}

/// Which fields a backend's override actually supplied: provenance the merged spec cannot
/// carry, so a verdict for one backend (`BackendKind::error`, and the `env` names `BackendKind::refuse`
/// judges) is not laid on an override that never wrote the value. Only `user` and the `env` names are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Supplied {
    pub user: bool,
    /// The `env` names the override wrote.
    pub env: BTreeSet<String>,
}

impl Supplied {
    /// No override at all: every field is a base value.
    pub const NONE: Supplied = Supplied {
        user: false,
        env: BTreeSet::new(),
    };

    /// Whether the override wrote `field`'s value; for an `env` field, the entry of that name. A field
    /// that is not recorded is never supplied.
    pub fn supplies(&self, field: &Field) -> bool {
        match field {
            Field::User => self.user,
            Field::EnvKey(key) => self.env.contains(&**key),
            Field::EnvValue(key) => self.env.contains(key),
            Field::Name
            | Field::Command(_)
            | Field::Cwd
            | Field::Logs
            | Field::Restart
            | Field::RestartDelay
            | Field::Kind => false,
        }
    }
}

impl RawOverride {
    /// Which fields (see [`Supplied`]) this override sets.
    pub fn supplied(&self) -> Supplied {
        Supplied {
            user: self.user.is_some(),
            env: self.env.iter().flat_map(|env| env.keys().cloned()).collect(),
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
    /// paired with `Supplied::NONE`. A per-backend rejection may only be
    /// applied to a field the returned `Supplied` marks `true`.
    pub fn merged_for(&self, backend: BackendKind) -> (RawSpec, Supplied) {
        let Some(ovr) = self.backend_specific.get(&backend) else {
            return (self.without_overrides(), Supplied::NONE);
        };

        let mut env = self.env.clone();
        if let Some(ovr_env) = &ovr.env {
            env.extend(ovr_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        let merged = RawSpec {
            id: self.id.clone(),
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
            id: self.id.clone(),
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

/// `RawSpec::backend_specific`, like `raw.rs`'s `EnvVisitor`: a plain
/// `BTreeMap<BackendKind, RawOverride>` would silently keep the last of two
/// `scm:` blocks.
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

        while let Some(backend) = map.next_key::<BackendKind>()? {
            if overrides.contains_key(&backend) {
                return Err(de::Error::custom(format!("duplicate backend key `{backend}`")));
            }
            let OverrideEntry(value) = map.next_value()?;
            overrides.insert(backend, value);
        }

        Ok(overrides)
    }
}

/// `RawOverride::env`, checked by the same visitor as `RawSpec::env`, so base and override agree
/// on what a duplicate is. Always `Some`: a missing key never reaches this function, and a `null`
/// is a type error from the visitor.
fn no_duplicate_env<'de, D>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_map(EnvVisitor).map(Some)
}

#[cfg(test)]
#[path = "overrides_tests.rs"]
mod overrides_tests;
