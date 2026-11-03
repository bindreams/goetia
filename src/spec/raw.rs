//! The literal shape of `goetia.yaml`: what deserializes directly from
//! YAML, before `resolve` turns it into a `DaemonSpec`.
//!
//! [`RawSpec::parse`] is the entry point: it deserializes, and then refuses
//! every explicit `null` the deserialization accepted (see `no_null` for
//! why that is a separate walk of the same text, and why it goes second).
//!
//! `env` gets a `deserialize_with`: a typed `BTreeMap` field cannot detect
//! a duplicate YAML key, because serde's map deserializer inserts and
//! overwrites and `yaml_serde`'s own duplicate-key check lives only in its
//! `Mapping` deserializer, which a typed map never reaches. So this module
//! walks the mapping's key/value pairs itself, one entry at a time, and
//! rejects a repeat, or a collision that differs only in case, before it
//! ever reaches a `BTreeMap`.

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
/// **Construct one with [`RawSpec::parse`], not with `Deserialize`.**
/// The null rule is enforced by a second walk that `parse` runs over the
/// same text, because it needs that text to report a position — a
/// `Deserializer` no longer has one by the time a field could object. So
/// `yaml_serde::from_str::<RawSpec>` compiles, and silently accepts every
/// explicit `null` this module exists to refuse.
///
/// That is a real gap in the public surface, not a theoretical one: this
/// type is exported, so a library consumer can reach the unguarded path.
/// It is stated here rather than left to be discovered. Narrowing it means
/// deciding whether [`resolve`](super::resolve) — the only reason this type
/// is public at all — belongs in the public API beside
/// [`load`](super::load), which takes a path and is always safe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpec {
    /// The installed service's name. Required, never overridable, and
    /// checked against the id pattern by `resolve`, not here.
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    // `Option`, not `Vec`: a missing `command` is legal at parse time, and
    // becomes one backend's problem instead of the document's — see
    // `overrides.rs` and `require_complete`'s `reject_empty_command` call.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "no_duplicate_env")]
    pub env: BTreeMap<String, String>,
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
    #[serde(
        rename = "backend-specific",
        default,
        deserialize_with = "crate::spec::overrides::deserialize_overrides"
    )]
    pub backend_specific: BackendOverrides,
}

impl RawSpec {
    /// A manifest's parse entry point, and the only one. The typed parse
    /// runs first and owns every structural diagnostic — unknown field,
    /// duplicate key, wrong type — and `reject_nulls` then walks the same
    /// unmodified text for the one thing that parse cannot refuse with a
    /// position of its own: an explicit `null`. See `no_null` for both
    /// halves of that split.
    pub fn parse(yaml: &str) -> Result<Self, yaml_serde::Error> {
        let spec = yaml_serde::from_str(yaml)?;
        reject_nulls(yaml)?;
        Ok(spec)
    }
}

/// `RawSpec::env`, walked one entry at a time for the reason in the module
/// doc. These values become a privileged service's environment, so a
/// key lost that way means the daemon runs with an environment the author
/// never wrote and cannot see they lost.
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
        // Folded key -> the original-case key it was first seen as, so an
        // exact repeat and a differently-cased collision get distinct
        // messages.
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
