//! The `user:` field: a bare string, or a struct with exactly one of `name`
//! or `id`.
//!
//! `#[derive(Deserialize)]` with `#[serde(untagged)]` was tried and
//! rejected for `User` itself: serde's untagged deserializer buffers the
//! whole value, tries every variant in turn, and on failure reports every
//! attempt squashed into one message — for a struct with a typo'd field
//! name, nothing in that message names the offending field. `User`'s
//! `Deserialize` is hand-written instead, so a bad value still does.

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// A resolved account identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum User {
    /// The bare string `root`, or `{name: root}`: the platform's
    /// superuser, emitted explicitly (`User=0`, `LocalSystem`,
    /// `UserName: root`).
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

impl<'de> Deserialize<'de> for User {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UserVisitor)
    }
}

struct UserVisitor;

impl<'de> Visitor<'de> for UserVisitor {
    type Value = User;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a username string, `root`, or a struct with exactly one of `name` or `id`")
    }

    fn visit_str<E>(self, v: &str) -> Result<User, E>
    where
        E: de::Error,
    {
        Ok(if v == "root" {
            User::Root
        } else {
            User::Name(v.to_owned())
        })
    }

    fn visit_string<E>(self, v: String) -> Result<User, E>
    where
        E: de::Error,
    {
        self.visit_str(&v)
    }

    fn visit_map<A>(self, mut map: A) -> Result<User, A::Error>
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
                    name = Some(map.next_value()?);
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
            (Some(name), None) => Ok(User::Name(name)),
            (None, Some(id)) => Ok(User::Id(id)),
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
