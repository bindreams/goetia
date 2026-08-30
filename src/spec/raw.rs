//! The literal shape of `goetia.yaml`: what deserializes directly from
//! YAML, before `resolve` turns it into `DaemonSpec`s.
//!
//! `RawManifest`'s `Deserialize` is hand-written rather than derived. A
//! typed `BTreeMap<String, RawSpec>` field cannot detect a duplicate YAML
//! key: serde's map deserializer inserts and overwrites, and
//! `serde_yaml_ng`'s own duplicate-key check lives only in its `Mapping`
//! deserializer, which a typed map never reaches — a manifest declaring
//! `frpc` twice would silently deserialize to one entry holding the
//! *second* command. So this module walks the `daemons` mapping's
//! key/value pairs itself, one entry at a time, and rejects a repeated or
//! case-insensitively colliding id before it ever reaches a `BTreeMap`.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::de::{self, DeserializeSeed, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::user::User;
use super::{Kind, Restart};

/// The whole `goetia.yaml` document.
#[derive(Debug, Clone, Default)]
pub struct RawManifest {
    pub daemons: BTreeMap<String, RawSpec>,
}

/// One `daemons.<id>` entry, exactly as written in YAML. No defaults are
/// materialized and no cross-field or injection-gate validation runs here
/// — see `resolve`, the parse-don't-validate boundary.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpec {
    pub name: Option<String>,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub user: Option<User>,
    pub restart: Option<Restart>,
    #[serde(rename = "restart-delay", default, with = "humantime_serde::option")]
    pub restart_delay: Option<Duration>,
    pub logs: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<Kind>,
}

impl<'de> Deserialize<'de> for RawManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ManifestVisitor)
    }
}

struct ManifestVisitor;

impl<'de> Visitor<'de> for ManifestVisitor {
    type Value = RawManifest;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping with a `daemons` key")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut daemons = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "daemons" => {
                    if daemons.is_some() {
                        return Err(de::Error::duplicate_field("daemons"));
                    }
                    daemons = Some(map.next_value_seed(DaemonsSeed)?);
                }
                other => return Err(de::Error::unknown_field(other, &["daemons"])),
            }
        }
        let daemons = daemons.ok_or_else(|| de::Error::missing_field("daemons"))?;
        Ok(RawManifest { daemons })
    }
}

/// A `DeserializeSeed` that walks the `daemons` mapping's key/value pairs
/// directly via `MapAccess`, checking each new key against every key seen
/// so far before it is inserted — the point at which a typed map alone
/// cannot catch a duplicate.
struct DaemonsSeed;

impl<'de> DeserializeSeed<'de> for DaemonsSeed {
    type Value = BTreeMap<String, RawSpec>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(DaemonsVisitor)
    }
}

struct DaemonsVisitor;

impl<'de> Visitor<'de> for DaemonsVisitor {
    type Value = BTreeMap<String, RawSpec>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping of daemon id to daemon spec")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut daemons = BTreeMap::new();
        // Lowercased key -> the original-case key it was first seen as, so
        // an exact repeat and a differently-cased collision get distinct
        // messages.
        let mut seen: BTreeMap<String, String> = BTreeMap::new();

        while let Some(key) = map.next_key::<String>()? {
            let lower = key.to_lowercase();
            if let Some(first) = seen.get(&lower) {
                return Err(if *first == key {
                    de::Error::custom(format!("duplicate daemon id `{key}`"))
                } else {
                    de::Error::custom(format!("daemon ids `{first}` and `{key}` collide case-insensitively"))
                });
            }
            seen.insert(lower, key.clone());
            let spec: RawSpec = map.next_value()?;
            daemons.insert(key, spec);
        }

        Ok(daemons)
    }
}

#[cfg(test)]
#[path = "raw_tests.rs"]
mod raw_tests;
