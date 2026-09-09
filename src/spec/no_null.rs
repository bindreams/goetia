//! A scan that refuses an explicit YAML `null` anywhere in a manifest the
//! typed parse has already accepted.
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
//! So the two jobs are split across two walks of the same text. The typed
//! parse reads each scalar as a string, so the authored text survives;
//! this scan reads every node with `deserialize_any`, which sees a null
//! exactly and reports it from the right place — and throws every value
//! away, so its coercion costs nothing. Both walk the same unmodified
//! text, so neither can move a position or change a message.
//!
//! # Why it runs second, and may only ever report a `null`
//!
//! The scan is schema-blind: it walks whatever YAML is written and knows
//! nothing about which keys are fields. Running it first therefore handed
//! it errors that are not its business. `bogus: null` was refused as a
//! null rather than as ``unknown field `bogus` ``, and the author's actual
//! mistake was never named; so were a wrong container type and a duplicate
//! key. And `env: {V: !!int abc}`, which the typed parse accepts as the
//! text `abc`, became a hard parse error, because `deserialize_any` checks
//! a standard tag's content against the tag.
//!
//! So the typed parse goes first and owns unknown fields, duplicate keys
//! and type errors, with the positions and messages it always had, and
//! only a structurally valid document reaches the scan. Anything the scan
//! can still object to other than a `null` is therefore something the
//! typed parse has already accepted, and is discarded — the tag check
//! happens inside `serde_yaml_ng` before the visitor is reached, so the
//! scan cannot decline to see it, only decline to report it. Discarding
//! means *keep walking*: a `null` sitting after a tag the scan could not
//! read must still be found, so each container skips the node it could not
//! read and carries on with the next one.
//!
//! One consequence is deliberate. A `null` in a position the typed parse
//! rejects for its own reasons now reports that reason — `command: null`
//! is `invalid type: unit value, expected a sequence` — which names the
//! field and is what `goetia` said before this rule existed. The positions
//! the typed parse *accepts* are the ones this module answers: every
//! `Option` field, `env` keys and values, `command` elements, a daemon id,
//! `user`, `user.name`, and a container written empty rather than `null`
//! (`daemons:` with no body deserializes to an empty mapping).

use std::cell::Cell;
use std::fmt;

use serde::de::{self, DeserializeSeed, EnumAccess, IgnoredAny, MapAccess, SeqAccess, VariantAccess, Visitor};

/// Refuse every explicit `null` in `yaml`, naming the first one found.
/// `yaml` must be a document the typed parse has accepted: every other
/// complaint this walk can raise belongs to that parse and is discarded
/// here — see the module doc comment.
pub(super) fn reject_nulls(yaml: &str) -> Result<(), serde_yaml_ng::Error> {
    let found = Cell::new(false);
    let scan = Scan(Slot::Document, &found);
    match scan.deserialize(serde_yaml_ng::Deserializer::from_str(yaml)) {
        Err(error) if found.get() => Err(error),
        _ => Ok(()),
    }
}

/// What a `null` `user.id` is refused with. It is `user`'s own
/// `Deserialize` that raises it, not this scan: the typed parse rejects a
/// `null` there, so the scan never sees one — see [`AccountId`](super::user::AccountId).
pub(super) const NULL_ACCOUNT_ID: &str = "an explicit `null` is not an account id; omit the `user:` key instead";

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
            Slot::BackendSpecific => {
                "an explicit `null` is not a set of backend overrides; write at least one `<backend>:` entry under it"
            }
            Slot::Override => {
                "an explicit `null` is not a backend override; give it at least one field to override"
            }
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
            (Slot::Document, "daemons") => Slot::Daemons,
            (Slot::Daemons, _) => Slot::Daemon,
            (Slot::Daemon, "backend-specific") => Slot::BackendSpecific,
            (Slot::BackendSpecific, _) => Slot::Override,
            (Slot::Daemon | Slot::Override, "env") => Slot::Env,
            (Slot::Daemon | Slot::Override, "command") => Slot::Command,
            (Slot::Daemon | Slot::Override, "user") => Slot::User,
            (Slot::Env, _) => Slot::EnvValue,
            (Slot::User, "name") => Slot::UserName,
            (Slot::User, "id") => Slot::UserId,
            _ => Slot::Field,
        }
    }

    /// The slot of a key of this mapping. Only `daemons` and `env` name
    /// their keys; everywhere else a key is a field name, and a `null` one
    /// has no field to unset.
    fn key(self) -> Slot {
        match self {
            Slot::Daemons => Slot::DaemonId,
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

/// One node of the scan. `deserialize_any` is the whole point: it is what
/// reaches `visit_unit` for a `null`, from inside the position the error
/// needs, and what resolves `!!null "null"` — `deserialize_option` decides
/// null-ness from the scalar's *style*, so it sees a tagged null only when
/// the scalar is unquoted.
///
/// The flag says whether the error currently travelling up the stack is
/// this scan's own refusal. Any other one belongs to the typed parse,
/// which has already accepted this document, so it is dropped and the walk
/// carries on — see the module doc comment.
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
                // The key is consumed even when it cannot be read, so
                // the value beside it must still be taken, or the walk
                // falls out of step and starts reading values as keys.
                // It is taken as a *value*, not skipped: the typed parse
                // read that key as a field, so a `null` under it is part
                // of the manifest. Only the key's own text is lost, and
                // with it the slot that text would have chosen.
                Err(_) => String::new(),
            };
            match map.next_value_seed(Scan(self.0.value(&key), self.1)) {
                Ok(()) => {}
                Err(error) if self.refused_a_null() => return Err(error),
                Err(_) => {}
            }
        }
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

    /// A key carrying a local tag (`!mine env:`) arrives as an enum. The
    /// typed parse resolves the tag away and honours it as the field it
    /// names, so this has to hand back the same text: dropping it puts the
    /// value beside it in the wrong slot, and the refusal then contradicts
    /// its own key path.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<String, A::Error> {
        let (_tag, variant) = data.variant::<IgnoredAny>()?;
        variant.newtype_variant_seed(self)
    }
}

#[cfg(test)]
#[path = "no_null_tests.rs"]
mod no_null_tests;
