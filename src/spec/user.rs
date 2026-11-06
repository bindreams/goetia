//! The `user:` field: a bare string, or a struct with exactly one of `name`
//! or `id`.
//!
//! [`RawUser`] is what deserializes; [`User`] is what `resolve` produces
//! from it. They are separate because the `root` reserved word applies to
//! the bare string form *only*, and once a bare `root` has become
//! `User::Root` nothing records which syntax it came from, so the rule
//! could not be re-applied to an interpolated value without also
//! mis-applying it to the struct form, where `{name: root}` is a literal
//! account: `{name: ${U}}` with `U=root` would become `User::Root`.
//!
//! `RawUser`'s `Deserialize` is hand-written, not `#[serde(untagged)]`: the
//! untagged deserializer squashes every failed variant into one message
//! that names no offending field.

use std::fmt;

use serde::de::{self, MapAccess, Unexpected, Visitor};
use serde::{Deserialize, Deserializer};

use super::no_null::NULL_ACCOUNT_ID;

/// The `user:` field exactly as authored, before the `root` reserved word
/// is applied, so `resolve` can apply that rule to interpolated text exactly as to
/// typed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RawUser {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountId {
    Uid(u32),
    Sid(String),
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(AccountIdVisitor)
    }
}

/// Hand-written rather than `#[serde(untagged)]`, which reports an explicit `null` here as
/// `data did not match any variant of untagged enum AccountId`. `no_null`'s scan cannot give a
/// better message: it only runs on a document this parse has already accepted.
struct AccountIdVisitor;

impl<'de> Visitor<'de> for AccountIdVisitor {
    type Value = AccountId;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a numeric account id, or a SID string")
    }

    fn visit_unit<E>(self) -> Result<AccountId, E>
    where
        E: de::Error,
    {
        Err(E::custom(NULL_ACCOUNT_ID))
    }

    fn visit_u64<E>(self, v: u64) -> Result<AccountId, E>
    where
        E: de::Error,
    {
        u32::try_from(v)
            .map(AccountId::Uid)
            .map_err(|_| E::invalid_value(Unexpected::Unsigned(v), &self))
    }

    fn visit_i64<E>(self, v: i64) -> Result<AccountId, E>
    where
        E: de::Error,
    {
        u32::try_from(v)
            .map(AccountId::Uid)
            .map_err(|_| E::invalid_value(Unexpected::Signed(v), &self))
    }

    fn visit_str<E>(self, v: &str) -> Result<AccountId, E>
    where
        E: de::Error,
    {
        Ok(AccountId::Sid(v.to_owned()))
    }
}

/// `RawUser` has no `Deserialize` of its own, for the reason `RawSpec` has none: `user: {name:
/// null}` reads as the text `null`, which only `parse`'s `no_null` walk refuses. So
/// [`deserialize_user`] is the only reader, and it is wired in by the remote derives of `RawSpec`
/// and `RawOverride`.
struct UserWire(RawUser);

impl<'de> Deserialize<'de> for UserWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UserVisitor).map(UserWire)
    }
}

/// The `deserialize_with` for an `Option<RawUser>` field: absent and `null` are `None`, as for any
/// `Option`.
pub(super) fn deserialize_user<'de, D>(d: D) -> Result<Option<RawUser>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<UserWire>::deserialize(d).map(|user| user.map(|UserWire(user)| user))
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
