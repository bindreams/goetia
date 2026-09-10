//! The `user:` field: a bare string, or a struct with exactly one of `name`
//! or `id`.
//!
//! [`RawUser`] is what deserializes; [`User`] is what `resolve` produces
//! from it. They are separate types because the `root` reserved word
//! applies to the bare string form *only* — see [`RawUser`] — and once a
//! bare `root` has become `User::Root`, nothing records which syntax it
//! came from, so the rule cannot be re-applied to a substituted value
//! without also mis-applying it to the struct form.
//!
//! `#[derive(Deserialize)]` with `#[serde(untagged)]` was tried and
//! rejected for the deserialized type: serde's untagged deserializer
//! buffers the whole value, tries every variant in turn, and on failure
//! reports every attempt squashed into one message — for a struct with a
//! typo'd field name, nothing in that message names the offending field.
//! `RawUser`'s `Deserialize` is hand-written instead, so a bad value still
//! does.

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// The `user:` field exactly as authored, before the `root` reserved word
/// is applied. `RawSpec` holds one of these so that interpolation can
/// substitute the text and `resolve` can then apply that rule to the
/// result, exactly as it would to text the author typed — see the module
/// doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawUser {
    /// A bare string: `user: root` or `user: bindreams`. The only form the
    /// `root` reserved word applies to.
    Scalar(String),
    /// `{name: ...}`: always a literal username, with no special-casing —
    /// this form is the escape hatch for an account genuinely called
    /// `root`.
    Name(String),
    /// `{id: ...}`: a numeric UID, or a Windows SID string.
    Id(AccountId),
}

/// A resolved account identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum User {
    /// The bare string `root`, or an absent `user:` field: the platform's
    /// superuser, emitted explicitly (`User=0`, `LocalSystem`,
    /// `UserName: root`). Never reached from `{name: root}`, which is the
    /// literal account named root.
    Root,
    /// Any other bare string, or `{name: ...}` — including `{name: root}`,
    /// which is the literal username `"root"` with no special-casing.
    Name(String),
    /// `{id: ...}`: a numeric UID, or a Windows SID string.
    Id(AccountId),
}

/// The value of a `user.id` struct field: numeric YAML values become
/// `Uid`, everything else (including a quoted digit string) becomes `Sid`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum AccountId {
    Uid(u32),
    Sid(String),
}

impl<'de> Deserialize<'de> for RawUser {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UserVisitor)
    }
}

/// A `user.name` that refuses an explicit YAML `null`. `no_null` guards the
/// `user:` field one level above, which does not reach this one: a plain
/// `String` here would render the scalar's *text* and install a service
/// under the literal account `"null"` — see `no_null`'s doc comment for why
/// only `Option::<String>::deserialize` sees a `null` at all. `user.id`
/// needs no such wrapper: `AccountId` is `#[serde(untagged)]` and buffers
/// through `Value`, which refuses a unit outright.
struct NoNullName(String);

impl<'de> Deserialize<'de> for NoNullName {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        super::no_null::string_or_null(d, "an explicit `null` is not a username; omit the `user:` key instead")
            .map(NoNullName)
    }
}

struct UserVisitor;

impl<'de> Visitor<'de> for UserVisitor {
    type Value = RawUser;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a username string, `root`, or a struct with exactly one of `name` or `id`")
    }

    /// The bare string is carried through verbatim: `root` is a reserved
    /// word here, but applying it is `resolve`'s job, after interpolation
    /// has had its turn at the text.
    fn visit_str<E>(self, v: &str) -> Result<RawUser, E>
    where
        E: de::Error,
    {
        Ok(RawUser::Scalar(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<RawUser, E>
    where
        E: de::Error,
    {
        Ok(RawUser::Scalar(v))
    }

    fn visit_map<A>(self, mut map: A) -> Result<RawUser, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut name: Option<String> = None;
        let mut id: Option<AccountId> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "name" => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value::<NoNullName>()?.0);
                }
                "id" => {
                    if id.is_some() {
                        return Err(de::Error::duplicate_field("id"));
                    }
                    id = Some(map.next_value()?);
                }
                other => return Err(de::Error::unknown_field(other, &["name", "id"])),
            }
        }

        match (name, id) {
            (Some(name), None) => Ok(RawUser::Name(name)),
            (None, Some(id)) => Ok(RawUser::Id(id)),
            (Some(_), Some(_)) => Err(de::Error::custom(
                "user struct must set exactly one of `name` or `id`, not both",
            )),
            (None, None) => Err(de::Error::custom("user struct must set exactly one of `name` or `id`")),
        }
    }
}

#[cfg(test)]
#[path = "user_tests.rs"]
mod user_tests;
