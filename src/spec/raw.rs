//! The literal shape of `goetia.yaml`: what deserializes directly from
//! YAML, before `resolve` turns it into a `DaemonSpec`.
//!
//! [`RawSpec::parse`] is the entry point: it deserializes, and then refuses
//! every explicit `null` the deserialization accepted (see `no_null` for
//! why that is a separate walk of the same text, and why it goes second).
//!
//! `env` gets a `deserialize_with`: a typed `BTreeMap` field cannot detect
//! a duplicate YAML key, because serde's map deserializer overwrites and
//! `yaml_serde`'s duplicate-key check lives only in its `Mapping`
//! deserializer. So this module walks the entries itself and rejects a
//! repeat, or a collision differing only in case, before the `BTreeMap`.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::no_null::reject_nulls;
use super::overrides::BackendOverrides;
use super::user::RawUser;

/// The whole `goetia.yaml` document: one daemon, exactly as written in
/// YAML. No defaults are materialized and no cross-field or injection-gate
/// validation runs here — see `resolve`, the parse-don't-validate boundary.
///
/// **[`RawSpec::parse`] is the only way to get one from text.** The null
/// rule is enforced by a second walk that `parse` runs over the same text,
/// because it needs that text to report a position. So `RawSpec` does not
/// implement `Deserialize`: the derive sits on the private `Wire`, which
/// only `parse` deserializes into. The type is module-private to `spec` so
/// that library consumers reach a spec only through [`load`](super::load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawSpec {
    /// The installed service's name. Required, never overridable, and
    /// checked against the id pattern by `resolve`, not here.
    pub id: String,
    pub name: Option<String>,
    // `Option`, not `Vec`: a missing `command` is legal at parse time and is caught per backend
    // (see `overrides.rs`, `require_complete`).
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub user: Option<RawUser>,
    pub restart: Option<String>,
    pub restart_delay: Option<String>,
    pub logs: Option<String>,
    pub kind: Option<String>,
    pub backend_specific: BackendOverrides,
}

/// `RawSpec`'s serde shape, private so that `parse` is the only reader of
/// YAML into a `RawSpec`. A field added here must be added to `RawSpec`: the
/// `From` impl destructures it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "no_duplicate_env")]
    env: BTreeMap<String, String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    restart: Option<String>,
    #[serde(rename = "restart-delay", default)]
    restart_delay: Option<String>,
    #[serde(default)]
    logs: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(
        rename = "backend-specific",
        default,
        deserialize_with = "crate::spec::overrides::deserialize_overrides"
    )]
    backend_specific: BackendOverrides,
}

impl From<Wire> for RawSpec {
    fn from(wire: Wire) -> Self {
        let Wire {
            id,
            name,
            command,
            cwd,
            env,
            user,
            restart,
            restart_delay,
            logs,
            kind,
            backend_specific,
        } = wire;
        RawSpec {
            id,
            name,
            command,
            cwd,
            env,
            user,
            restart,
            restart_delay,
            logs,
            kind,
            backend_specific,
        }
    }
}

impl RawSpec {
    /// A manifest's parse entry point, and the only one. The typed parse
    /// runs first and owns every structural diagnostic (unknown field,
    /// duplicate key, wrong type); `reject_nulls` then walks the same text
    /// for explicit `null`s. See `no_null` for the split.
    pub fn parse(yaml: &str) -> Result<Self, yaml_serde::Error> {
        let wire: Wire = yaml_serde::from_str(yaml)?;
        reject_nulls(yaml)?;
        Ok(wire.into())
    }
}

/// `RawSpec::env`, walked one entry at a time for the reason in the module
/// doc. A key silently lost would give a privileged service an environment
/// the author never wrote.
fn no_duplicate_env<'de, D>(d: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_map(EnvVisitor)
}

pub(super) struct EnvVisitor;

impl<'de> Visitor<'de> for EnvVisitor {
    type Value = BTreeMap<String, String>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping of environment variable name to value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut env = BTreeMap::new();
        // Folded key -> first-seen original case, so a repeat and a case collision differ in message.
        let mut seen: BTreeMap<String, String> = BTreeMap::new();

        while let Some(key) = map.next_key::<String>()? {
            let folded = key.to_lowercase();
            if let Some(first) = seen.get(&folded) {
                return Err(if *first == key {
                    de::Error::custom(format!("duplicate env key `{key}`"))
                } else {
                    de::Error::custom(format!(
                        "env keys `{first}` and `{key}` differ only in case, and Windows looks an \
                         environment variable up case-insensitively, so one of the two would be lost"
                    ))
                });
            }
            let value = map.next_value::<String>()?;
            seen.insert(folded, key.clone());
            env.insert(key, value);
        }

        Ok(env)
    }
}

#[cfg(test)]
#[path = "raw_tests.rs"]
mod raw_tests;
