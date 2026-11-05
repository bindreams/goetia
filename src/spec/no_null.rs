//! A scan that refuses an explicit YAML `null` anywhere in a manifest the
//! typed parse has already accepted.
//!
//! An explicit `null` is an error wherever a manifest value is expected:
//! the nine daemon fields, the daemon `id` itself, the keys and the values
//! of `env`, the elements of `command`, and `user`'s `name` and `id`.
//! `env: {A: ""}`, `env: {}` and `command: [/bin/frpc, ""]` stay legal —
//! only `null` is refused, not emptiness — and a *quoted* `"null"` is
//! ordinary text everywhere.
//!
//! # Why a scan, and not a `deserialize_with` on each field
//!
//! Two properties are wanted from one rejection, and no single
//! `Deserializer` call gives both.
//!
//! - **Position.** An error raised after `deserialize_option` has returned
//!   `Ok(None)` has lost the node's position, and the enclosing container
//!   stamps its own start line on it. Only `deserialize_any` reports the real
//!   one.
//! - **Authored text.** `deserialize_any` hands the visitor a plain scalar
//!   already coerced to a bool or number (`0x10` arrives as `16`), but an
//!   environment variable must carry what the author wrote
//!   (`resolve_tests`' `load_preserves_authored_scalar_text_alongside_interpolation`).
//!
//! So the two jobs are split across two walks of the same text. The typed
//! parse reads each scalar as a string, so the authored text survives;
//! this scan reads every node with `deserialize_any`, which sees a null
//! exactly and reports it from the right place, and throws every value
//! away. Both walk the same unmodified text, so neither can move a position
//! or change a message.
//!
//! # Why it runs second, and may only ever report a `null`
//!
//! The scan is schema-blind, so run first it would be handed errors that are
//! not its business: `bogus: null` refused as a null instead of as ``unknown
//! field `bogus` ``, and `env: {V: !!int abc}`, which the typed parse accepts
//! as the text `abc`, turned into a hard error because `deserialize_any`
//! checks a standard tag's content.
//!
//! So the typed parse goes first and owns unknown fields, duplicate keys
//! and type errors, and only a structurally valid document reaches the scan.
//! Anything else the scan objects to is something the typed parse already
//! accepted, and is discarded, not avoided: `yaml_serde` checks a standard
//! tag's content against the tag before the visitor is reached, so the
//! scan cannot decline to see that kind of complaint, only decline to
//! report it. Discarding means *keep walking*: each container skips the
//! node it could not read and carries on, so a `null` after a tag the scan
//! could not read is still found.
//!
//! A `null` in a position the typed parse rejects for its own reasons
//! reports that reason (`command: null` is `invalid type: unit value,
//! expected a sequence`). The positions this module answers are those the
//! typed parse *accepts*: every `Option` field, `env` keys and values,
//! `command` elements, the daemon `id`, `user`, `user.name`, and a container
//! written empty rather than `null` (`env:` with no body is an empty
//! mapping).

use std::cell::Cell;
use std::fmt;

use serde::de::{self, DeserializeSeed, EnumAccess, IgnoredAny, MapAccess, SeqAccess, VariantAccess, Visitor};

/// Refuse every explicit `null` in `yaml`, naming the first one found.
/// `yaml` must be a document the typed parse has accepted; any other complaint
/// this walk raises is discarded (see the module doc).
pub(super) fn reject_nulls(yaml: &str) -> Result<(), yaml_serde::Error> {
    let found = Cell::new(false);
    let scan = Scan(Slot::Document, &found);
    match scan.deserialize(yaml_serde::Deserializer::from_str(yaml)) {
        Err(error) if found.get() => Err(error),
        _ => Ok(()),
    }
}

/// What a `null` `user.id` is refused with. Raised by `user`'s own `Deserialize`, not this scan,
/// which never sees one — see [`AccountId`](super::user::AccountId).
pub(super) const NULL_ACCOUNT_ID: &str = "an explicit `null` is not an account id; omit the `user:` key instead";

/// Where in a manifest a node sits. It decides only what a refusal says: an unknown slot
/// becomes [`Slot::Field`], so a field added to `RawSpec` without a line here is still refused,
/// with the generic wording.
#[derive(Clone, Copy)]
enum Slot {
    /// The whole manifest, which is the daemon.
    Document,
    Id,
    BackendSpecific,
    Override,
    Field,
    Key,
    Env,
    EnvKey,
    EnvValue,
    Command,
    CommandElement,
    User,
    UserName,
    UserId,
}

impl Slot {
    /// What a `null` in this slot is refused with, if this scan is the one
    /// to refuse it. Each names the right remedy: "omit the key" suits a
    /// field with a default, not a required key or a map entry.
    fn message(self) -> Option<&'static str> {
        Some(match self {
            // A `null` document gets the typed parse's type error, which names what it wanted.
            Slot::Document => return None,
            Slot::Id => "an explicit `null` is not a daemon id; quote the value to use its literal text",
            Slot::BackendSpecific => {
                "an explicit `null` is not a set of backend overrides; write at least one `<backend>:` entry under it"
            }
            Slot::Override => "an explicit `null` is not a backend override; give it at least one field to override",
            Slot::Field => "an explicit `null` is not a way to unset this field; omit the key instead",
            Slot::Key => "an explicit `null` is not a field name; quote the key to use its literal text",
            Slot::Env => {
                "an explicit `null` is not an environment block; omit the `env:` key, or write `{}` for no variables"
            }
            Slot::EnvKey => {
                "an explicit `null` is not an environment variable name; quote the key to use its literal text"
            }
            Slot::EnvValue => {
                "an explicit `null` is not an environment value; omit the key, or write `\"\"` to set it empty"
            }
            Slot::Command => "an explicit `null` is not a command; write the program and its arguments as a sequence",
            Slot::CommandElement => {
                "an explicit `null` is not a command element; remove it, or write `\"\"` for an empty argument"
            }
            Slot::User => "an explicit `null` is not a user; omit the `user:` key instead",
            Slot::UserName => "an explicit `null` is not a username; omit the `user:` key instead",
            Slot::UserId => NULL_ACCOUNT_ID,
        })
    }

    /// The slot of the value stored under `key` in this mapping.
    fn value(self, key: &str) -> Slot {
        match (self, key) {
            (Slot::Document, "id") => Slot::Id,
            (Slot::Document, "backend-specific") => Slot::BackendSpecific,
            (Slot::BackendSpecific, _) => Slot::Override,
            (Slot::Document | Slot::Override, "env") => Slot::Env,
            (Slot::Document | Slot::Override, "command") => Slot::Command,
            (Slot::Document | Slot::Override, "user") => Slot::User,
            (Slot::Env, _) => Slot::EnvValue,
            (Slot::User, "name") => Slot::UserName,
            (Slot::User, "id") => Slot::UserId,
            _ => Slot::Field,
        }
    }

    /// The slot of a key of this mapping. Only `env` names its keys;
    /// elsewhere a key is a field name.
    fn key(self) -> Slot {
        match self {
            Slot::Env => Slot::EnvKey,
            _ => Slot::Key,
        }
    }

    /// The slot of an element of this sequence.
    fn element(self) -> Slot {
        match self {
            Slot::Command => Slot::CommandElement,
            _ => Slot::Field,
        }
    }
}

/// One node of the scan. `deserialize_any` reaches `visit_unit` for a `null` from inside the
/// position the error needs, and resolves `!!null "null"`, which `deserialize_option` sees only
/// when the scalar is unquoted.
///
/// The flag says whether the error travelling up the stack is this scan's own refusal; any other
/// is dropped and the walk carries on (see the module doc).
#[derive(Clone, Copy)]
struct Scan<'a>(Slot, &'a Cell<bool>);

impl Scan<'_> {
    /// Whether an error from the node below is one this scan may report.
    fn refused_a_null(self) -> bool {
        self.1.get()
    }
}

impl<'de> DeserializeSeed<'de> for Scan<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Scan<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a manifest value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        match self.0.message() {
            Some(message) => {
                self.1.set(true);
                Err(E::custom(message))
            }
            None => Ok(()),
        }
    }

    /// An empty document (`Event::Void`); the typed parse's `missing field \`id\`` says more.
    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _v: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _v: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _v: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_i128<E: de::Error>(self, _v: i128) -> Result<(), E> {
        Ok(())
    }

    fn visit_u128<E: de::Error>(self, _v: u128) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _v: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: de::Error>(self, _v: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        loop {
            match seq.next_element_seed(Scan(self.0.element(), self.1)) {
                Ok(Some(())) => {}
                Ok(None) => return Ok(()),
                Err(error) if self.refused_a_null() => return Err(error),
                Err(_) => {}
            }
        }
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        loop {
            let key = match map.next_key_seed(ScanKey(Scan(self.0.key(), self.1))) {
                Ok(Some(key)) => key,
                Ok(None) => return Ok(()),
                Err(error) if self.refused_a_null() => return Err(error),
                // The key is consumed even when unreadable, so take the value beside it or the
                // walk reads values as keys. Take it as a value, not skip it: a `null` under it
                // is part of the manifest. Only the slot the key's text would have chosen is lost.
                Err(_) => String::new(),
            };
            match map.next_value_seed(Scan(self.0.value(&key), self.1)) {
                Ok(()) => {}
                Err(error) if self.refused_a_null() => return Err(error),
                Err(_) => {}
            }
        }
    }

    /// A node carrying a local tag (`name: !mine text`) arrives as an enum.
    /// Recursing into the content keeps `!mine null` refused.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<(), A::Error> {
        let (_tag, variant) = data.variant::<IgnoredAny>()?;
        variant.newtype_variant_seed(self)
    }
}

/// A mapping key. Same refusal as [`Scan`], plus the key's text, which picks the slot of the
/// value beside it. The text is then discarded, so the coercion of a plain numeric key is
/// harmless: `env`'s keys reach `RawSpec` from the typed parse.
struct ScanKey<'a>(Scan<'a>);

impl<'de> DeserializeSeed<'de> for ScanKey<'_> {
    type Value = String;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ScanKey<'_> {
    type Value = String;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping key")
    }

    fn visit_unit<E: de::Error>(self) -> Result<String, E> {
        self.0.visit_unit().map(|()| String::new())
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_i128<E: de::Error>(self, v: i128) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        Ok(v.to_owned())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<String, A::Error> {
        self.0.visit_seq(seq).map(|()| String::new())
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<String, A::Error> {
        self.0.visit_map(map).map(|()| String::new())
    }

    /// A key carrying a local tag (`!mine env:`) arrives as an enum. The typed parse honours
    /// it as the field it names, so hand back the same text; dropping it puts the value in the
    /// wrong slot and the refusal contradicts its own key path.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<String, A::Error> {
        let (_tag, variant) = data.variant::<IgnoredAny>()?;
        variant.newtype_variant_seed(self)
    }
}

#[cfg(test)]
#[path = "no_null_tests.rs"]
mod no_null_tests;
