//! A pre-parse scan that refuses an explicit YAML `null` anywhere in a
//! manifest.
//!
//! An explicit `null` is an error wherever a manifest value is expected:
//! the `daemons` mapping and every daemon under it, all nine
//! `daemons.<id>` fields, the daemon id itself, the keys and the values of
//! `env`, the elements of `command`, and `user`'s `name` and `id`.
//! `env: {A: ""}`, `env: {}` and `command: [/bin/frpc, ""]` stay legal —
//! only `null` is refused, not emptiness — and a *quoted* `"null"` is
//! ordinary text everywhere.
//!
//! # Why a scan, and not a `deserialize_with` on each field
//!
//! Two properties are wanted from one rejection, and no single
//! `Deserializer` call gives both.
//!
//! - **Position.** `serde_yaml_ng` attaches the offending node's line and
//!   its key path in `error::fix_mark`, which each `deserialize_*` method
//!   applies to whatever error came out of it, filling a position in only
//!   if one is not set already — so the innermost one wins.
//!   `deserialize_option` is one of the two methods that never calls it.
//!   A refusal built after `Option::<T>::deserialize` has returned
//!   `Ok(None)` has therefore already left the deserializer that knew the
//!   position, and the enclosing container stamps its *own* start line on
//!   it instead: `restart: null` on line 9 reported the `command:` line
//!   six lines above it. Only `deserialize_any`, whose `visit_unit` is
//!   reached from inside `fix_mark`'s reach, can report the real one.
//! - **Authored text.** `deserialize_any` resolves a *plain* scalar
//!   through `visit_untagged_scalar` and hands the visitor the resulting
//!   bool or number, not the characters: `env: {C: 0x10}` arrives as
//!   `16`, `name: +5` as `5`, `env: {E: True}` as `true`. `resolve_tests`'
//!   `load_preserves_authored_scalar_text_alongside_interpolation` pins
//!   the opposite, and it is right to: an environment variable must carry
//!   what the author wrote.
//!
//! So the two jobs are split across two walks of the same text. This scan
//! runs first and reads every node with `deserialize_any`, which sees
//! nulls and tags exactly and reports them from the right place — and
//! throws every value away, so its coercion costs nothing. The typed parse
//! then runs on a document already known to hold no `null`, and reads each
//! scalar as a string, so the authored text survives. Both walk the same
//! unmodified text, so neither can move a position or change a message.

use std::fmt;

use serde::de::{self, DeserializeSeed, EnumAccess, IgnoredAny, MapAccess, SeqAccess, VariantAccess, Visitor};

/// Refuse every explicit `null` in `yaml`, naming the first one found.
pub(super) fn reject_nulls(yaml: &str) -> Result<(), serde_yaml_ng::Error> {
    Scan(Slot::Document).deserialize(serde_yaml_ng::Deserializer::from_str(yaml))
}

/// Where in a manifest a node sits. It decides what a refusal says, and
/// nothing else — a slot this table does not know becomes [`Slot::Field`],
/// so a field added to `RawSpec` without a line here is still refused,
/// just with the generic wording.
#[derive(Clone, Copy)]
enum Slot {
    Document,
    Daemons,
    DaemonId,
    Daemon,
    Field,
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
    /// to refuse it. Each names what the author meant to write instead,
    /// because "omit the key" is the right remedy for a field with a
    /// default and the wrong one for a required key or a map entry.
    fn message(self) -> Option<&'static str> {
        Some(match self {
            // A `null` *document* is not this scan's to diagnose: the typed
            // parse answers it with `expected a mapping with a \`daemons\`
            // key`, which is more use than anything about `null` would be.
            Slot::Document => return None,
            Slot::Daemons => "an explicit `null` is not a set of daemons; write at least one `<id>:` entry under it",
            Slot::DaemonId => "an explicit `null` is not a daemon id; quote the key to use its literal text",
            Slot::Daemon => "an explicit `null` is not a daemon; give it at least a `command:`",
            Slot::Field => "an explicit `null` is not a way to unset this field; omit the key instead",
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
            Slot::User => "an explicit `null` is not a way to unset this field; omit the key instead",
            Slot::UserName => "an explicit `null` is not a username; omit the `user:` key instead",
            Slot::UserId => "an explicit `null` is not an account id; omit the `user:` key instead",
        })
    }

    /// The slot of the value stored under `key` in this mapping.
    fn value(self, key: &str) -> Slot {
        match (self, key) {
            (Slot::Document, "daemons") => Slot::Daemons,
            (Slot::Daemons, _) => Slot::Daemon,
            (Slot::Daemon, "env") => Slot::Env,
            (Slot::Daemon, "command") => Slot::Command,
            (Slot::Daemon, "user") => Slot::User,
            (Slot::Env, _) => Slot::EnvValue,
            (Slot::User, "name") => Slot::UserName,
            (Slot::User, "id") => Slot::UserId,
            _ => Slot::Field,
        }
    }

    /// The slot of a key of this mapping.
    fn key(self) -> Slot {
        match self {
            Slot::Daemons => Slot::DaemonId,
            Slot::Env => Slot::EnvKey,
            _ => Slot::Field,
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

/// One node of the scan. `deserialize_any` is the whole point: it is what
/// reaches `visit_unit` for a `null`, from inside the position the error
/// needs, and what resolves `!!null "null"` — `deserialize_option` decides
/// null-ness from the scalar's *style*, so it sees a tagged null only when
/// the scalar is unquoted.
struct Scan(Slot);

impl<'de> DeserializeSeed<'de> for Scan {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Scan {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a manifest value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        match self.0.message() {
            Some(message) => Err(E::custom(message)),
            None => Ok(()),
        }
    }

    /// An empty document, which `serde_yaml_ng` reports as `Event::Void`.
    /// The typed parse's `missing field \`daemons\`` says more about it
    /// than this scan could.
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
        while seq.next_element_seed(Scan(self.0.element()))?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key_seed(ScanKey(self.0.key()))? {
            map.next_value_seed(Scan(self.0.value(&key)))?;
        }
        Ok(())
    }

    /// A node carrying a local tag (`name: !mine text`) arrives as an
    /// enum, since that is how `serde_yaml_ng` offers `!Tag` syntax.
    /// Recursing into the content rather than ignoring it keeps
    /// `!mine null` refused.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<(), A::Error> {
        let (_tag, variant) = data.variant::<IgnoredAny>()?;
        variant.newtype_variant_seed(self)
    }
}

/// A mapping key. Same refusal as [`Scan`], plus the key's text, which
/// picks the slot of the value beside it. The text is used for that and
/// discarded, so the coercion `deserialize_any` performs on a plain
/// numeric key is invisible: `env`'s keys reach `RawSpec` from the typed
/// parse, not from here.
struct ScanKey(Slot);

impl<'de> DeserializeSeed<'de> for ScanKey {
    type Value = String;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ScanKey {
    type Value = String;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping key")
    }

    fn visit_unit<E: de::Error>(self) -> Result<String, E> {
        Scan(self.0).visit_unit().map(|()| String::new())
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
        Scan(self.0).visit_seq(seq).map(|()| String::new())
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<String, A::Error> {
        Scan(self.0).visit_map(map).map(|()| String::new())
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<String, A::Error> {
        Scan(self.0).visit_enum(data).map(|()| String::new())
    }
}
