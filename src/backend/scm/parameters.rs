//! What `Services\<name>\Parameters` holds, as data.
//!
//! Not `#[cfg]`-gated: the registry reads that fill these types are Windows-only, but the types,
//! their classification of a raw entry and their rendering compile and test everywhere.
//!
//! goetia writes four string values there (see `generate`). Everything else is a [`Stray`]: kept
//! apart from the four so that no stray can be mistaken for one of them, and so that a stray the
//! registry API refuses to describe (a name that is not UTF-16 text, a type no API knows) is still
//! reported, and removable.

use std::collections::BTreeMap;
use std::fmt;

use crate::backend::scm::generate::field_named;

/// The registry's value type numbers (`winnt.h`), as plain constants so that this module needs no
/// Windows crate.
mod vtype {
    pub const REG_NONE: u32 = 0;
    pub const REG_SZ: u32 = 1;
    pub const REG_EXPAND_SZ: u32 = 2;
    pub const REG_BINARY: u32 = 3;
    pub const REG_DWORD: u32 = 4;
    pub const REG_DWORD_BIG_ENDIAN: u32 = 5;
    pub const REG_LINK: u32 = 6;
    pub const REG_MULTI_SZ: u32 = 7;
    pub const REG_RESOURCE_LIST: u32 = 8;
    pub const REG_FULL_RESOURCE_DESCRIPTOR: u32 = 9;
    pub const REG_RESOURCE_REQUIREMENTS_LIST: u32 = 10;
    pub const REG_QWORD: u32 = 11;
}

/// A `Parameters` entry that is not one of goetia's four fields stored as goetia stores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stray {
    /// A value, by its name as UTF-16 (the registry's names need not be valid text) and type
    /// number. Also one of goetia's own field names whose value is not exactly what goetia writes:
    /// of another type, or a string that is not plain NUL-terminated text.
    Value { name: Vec<u16>, vtype: u32 },
    /// A subkey, by its name as UTF-16.
    Subkey { name: Vec<u16> },
}

impl fmt::Display for Stray {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stray::Value { name, vtype } => {
                write!(out, "value {:?} of type {}", text_from_utf16(name), type_name(*vtype))
            }
            Stray::Subkey { name } => write!(out, "subkey {:?}", text_from_utf16(name)),
        }
    }
}

/// The contents of a `Parameters` key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parameters {
    /// goetia's own fields that are stored as goetia stores them, by the name as stored (its case
    /// may differ from goetia's: the registry ignores case).
    pub fields: BTreeMap<String, String>,
    pub strays: Vec<Stray>,
}

impl Parameters {
    /// Sort one value, as stored, into [`fields`](Self::fields) or [`strays`](Self::strays).
    pub fn add_value(&mut self, name: Vec<u16>, vtype: u32, data: &[u8]) {
        match classify_value(name, vtype, data) {
            Entry::Field(name, text) => {
                self.fields.insert(name, text);
            }
            Entry::Stray(stray) => self.strays.push(stray),
        }
    }

    pub fn add_subkey(&mut self, name: Vec<u16>) {
        self.strays.push(Stray::Subkey { name });
    }
}

/// How one value sorts.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Field(String, String),
    Stray(Stray),
}

/// A value is a field only when its name spells one of goetia's four in any case and its data is
/// what goetia writes for a string: a `REG_SZ` of valid UTF-16 text and one closing NUL. Anything
/// else under a field's name is a stray, so a hand edit never reads as equal to what goetia wants.
fn classify_value(name: Vec<u16>, vtype: u32, data: &[u8]) -> Entry {
    if vtype == vtype::REG_SZ
        && field_named_utf16(&name).is_some()
        && let Some(text) = exact_sz(data)
        && let Ok(name) = String::from_utf16(&name)
    {
        return Entry::Field(name, text);
    }
    Entry::Stray(Stray::Value { name, vtype })
}

/// Goetia's spelling of the field `name` spells in any ASCII case; `None` for any other name,
/// including one that is not valid UTF-16.
pub(crate) fn field_named_utf16(name: &[u16]) -> Option<&'static str> {
    String::from_utf16(name).ok().and_then(|name| field_named(&name))
}

/// The text of a `REG_SZ` laid out as goetia writes one: UTF-16 text, one closing NUL, nothing
/// else. `None` for an odd byte count, a missing or doubled terminator, a NUL inside, or text that
/// is not valid UTF-16.
fn exact_sz(data: &[u8]) -> Option<String> {
    let (pairs, remainder) = data.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    match units.as_slice() {
        [body @ .., 0] if !body.contains(&0) => String::from_utf16(body).ok(),
        _ => None,
    }
}

/// `units` as text, exactly. Units that are not valid UTF-16 show as text that says so, with each
/// unpaired surrogate and every `=` escaped.
pub(crate) fn text_from_utf16(units: &[u16]) -> String {
    let decoded: Vec<_> = char::decode_utf16(units.iter().copied()).collect();
    if decoded.iter().all(|c| c.is_ok()) {
        return decoded.into_iter().flatten().collect();
    }
    let mut shown = String::from("(not valid UTF-16: ");
    for c in decoded {
        match c {
            Ok('=') => shown.push_str(r"\x3D"),
            Ok(c) => shown.extend(c.escape_debug()),
            Err(e) => shown.push_str(&format!(r"\u{{{:04X}}}", e.unpaired_surrogate())),
        }
    }
    shown.push(')');
    shown
}

/// The registry's own name for a value type, or its number when it has none.
pub(crate) fn type_name(vtype: u32) -> String {
    match vtype {
        vtype::REG_NONE => "REG_NONE",
        vtype::REG_SZ => "REG_SZ",
        vtype::REG_EXPAND_SZ => "REG_EXPAND_SZ",
        vtype::REG_BINARY => "REG_BINARY",
        vtype::REG_DWORD => "REG_DWORD",
        vtype::REG_DWORD_BIG_ENDIAN => "REG_DWORD_BIG_ENDIAN",
        vtype::REG_LINK => "REG_LINK",
        vtype::REG_MULTI_SZ => "REG_MULTI_SZ",
        vtype::REG_RESOURCE_LIST => "REG_RESOURCE_LIST",
        vtype::REG_FULL_RESOURCE_DESCRIPTOR => "REG_FULL_RESOURCE_DESCRIPTOR",
        vtype::REG_RESOURCE_REQUIREMENTS_LIST => "REG_RESOURCE_REQUIREMENTS_LIST",
        vtype::REG_QWORD => "REG_QWORD",
        other => return other.to_string(),
    }
    .to_string()
}

#[cfg(test)]
#[path = "parameters_tests.rs"]
mod parameters_tests;
