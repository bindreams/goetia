//! The `backend-specific:` manifest key: a per-[`Backend`] set of field
//! overrides.
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
use super::user::RawUser;

pub type BackendOverrides = BTreeMap<Backend, RawOverride>;

/// One `backend-specific.<backend>` entry: every field optional, since an
/// override only needs to name the fields it actually overrides. Merging
/// an override into a base `RawSpec` is Task 3's job.
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
