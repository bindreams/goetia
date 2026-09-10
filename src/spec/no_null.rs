//! Deserializers that refuse an explicit YAML `null`, for `raw.rs`'s
//! fields and for every leaf under them.
//!
//! An explicit `null` is an error for all nine `daemons.<id>` fields, for
//! the daemon id itself, for the keys and the values of `env`, for the
//! elements of `command`, and for `user`'s `name`. `env: {A: ""}` and
//! `command: [/bin/frpc, ""]` stay legal — only `null` is refused, not
//! emptiness, and a *quoted* `"null"` is ordinary text everywhere.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// The one place an `Option<T>`-shaped field refuses an explicit YAML
/// `null`, instead of silently reading it as "unset".
///
/// **Do not write the variant that deserializes `T` and wraps it in
/// `Some`.** It does not work, and the failure is silent:
/// `serde_yaml_ng` renders a *plain scalar's text* when asked for a
/// `String`, and `null`, `~` and an empty value are all plain scalars, so
/// `String::deserialize` succeeds on them and yields `"null"`, `"~"` and
/// `""` respectively. Only `Option::<T>::deserialize` reaches
/// `visit_none`/`visit_some`, and there is no tension with the
/// unquoted-scalar-coercion requirement: `visit_some` hands the inner
/// `String::deserialize` the same plain scalar it would have got
/// directly, so `name: 42` still coerces to `Some("42")` here.
pub(super) fn no_null<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    match Option::<T>::deserialize(d)? {
        Some(value) => Ok(Some(value)),
        None => Err(de::Error::custom(
            "an explicit `null` is not a way to unset this field; omit the key instead",
        )),
    }
}

/// The one place a `String` position refuses an explicit YAML `null`.
/// `Option::<String>::deserialize` is the only form that sees one: see
/// `no_null` above, same reason.
pub(super) fn string_or_null<'de, D>(d: D, message: &'static str) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(d)?.ok_or_else(|| de::Error::custom(message))
}

/// A `command` element. `command[0]` is the executable path — a `null`
/// there is absolutized into `<manifest dir>/null`, which `reject_empty`
/// cannot catch because it is not empty — and a `null` in any later
/// position is a literal argv string. `""` stays a legal argv element,
/// which is why the message names it.
struct NoNullArg(String);

impl<'de> Deserialize<'de> for NoNullArg {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_null(
            d,
            "an explicit `null` is not a command element; remove it, or write `\"\"` for an empty argument",
        )
        .map(NoNullArg)
    }
}

/// `RawSpec::command`: still a required sequence, with no element of it
/// allowed to be `null`.
pub(super) fn no_null_command<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Vec::<NoNullArg>::deserialize(d)?.into_iter().map(|arg| arg.0).collect())
}

/// An `env` key.
struct NoNullEnvKey(String);

impl<'de> Deserialize<'de> for NoNullEnvKey {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_null(
            d,
            "an explicit `null` is not an environment variable name; quote the key to use its literal text",
        )
        .map(NoNullEnvKey)
    }
}

/// An `env` value. `""` is a legal empty assignment and stays legal; only
/// a `null` is refused, which is why the message names `""`.
struct NoNullEnvValue(String);

impl<'de> Deserialize<'de> for NoNullEnvValue {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_null(
            d,
            "an explicit `null` is not an environment value; omit the key, or write `\"\"` to set it empty",
        )
        .map(NoNullEnvValue)
    }
}

/// `RawSpec::env`: the map is required to be a map, and neither its keys
/// nor its values may be `null` or repeated.
pub(super) fn no_null_env<'de, D>(d: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_map(EnvVisitor)
}

/// A hand-written `Visitor` rather than a `BTreeMap<NoNullEnvKey,
/// NoNullEnvValue>`, for the reason `raw.rs`'s `DaemonsVisitor` exists:
/// serde's map deserializer inserts and overwrites, and `serde_yaml_ng`'s
/// own duplicate-key check lives only in its `Mapping` deserializer, which
/// a typed map never reaches. These values become a privileged service's
/// environment, so a key lost that way means the daemon runs with an
/// environment the author never wrote and cannot see they lost.
struct EnvVisitor;

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

        while let Some(NoNullEnvKey(key)) = map.next_key::<NoNullEnvKey>()? {
            if env.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate env key `{key}`")));
            }
            let NoNullEnvValue(value) = map.next_value()?;
            env.insert(key, value);
        }

        Ok(env)
    }
}
