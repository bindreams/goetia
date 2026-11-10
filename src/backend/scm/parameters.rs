//! What `Services\<name>\Parameters` holds, as data. Not `#[cfg]`-gated: the registry reads that
//! fill these types are Windows-only, but the types and their classification compile and test
//! everywhere. goetia writes four string values there (see `generate`), kept as fields; everything
//! else, and a field stored other than as goetia writes it, is a [`Stray`], apart from the fields so
//! that none is mistaken for one, and reported and removable even when the registry API refuses to
//! describe it. [`RegistryReadOutcome`] is what a read of a service's registry values returns.

use std::collections::BTreeMap;
use std::fmt;

use crate::backend::scm::generate::{FIELD_MARKER, field_named};
use crate::blob;

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
    /// number, that is not one of goetia's fields: another name, or a field that holds no one
    /// string of text.
    Value { name: Vec<u16>, vtype: u32 },
    /// One of goetia's fields whose text reads, stored other than as goetia writes it: another
    /// string type, a terminator other than one NUL, or (a `Marker`) another case. The field is in
    /// [`Parameters::fields`].
    Layout { name: &'static str, vtype: u32 },
    /// A subkey, by its name as UTF-16.
    Subkey { name: Vec<u16> },
}

impl fmt::Display for Stray {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stray::Value { name, vtype } => {
                write!(out, "value {} of type {}", shown_name(name), type_name(*vtype))
            }
            Stray::Layout { name, vtype } => write!(
                out,
                "value \"{name}\" stored as {}, not exactly as goetia writes it",
                type_name(*vtype)
            ),
            Stray::Subkey { name } => write!(out, "subkey {}", shown_name(name)),
        }
    }
}

/// `name` in double quotes, quoted as `ScmRegistration` quotes every other value it reads back
/// (`{:?}`), so it stays on one line and cannot close its own quotes. A name that is not valid
/// UTF-16 is quoted the same way with each unpaired unit as `\u{XXXX}`, which no valid name spells.
pub(crate) fn shown_name(name: &[u16]) -> String {
    if let Ok(text) = String::from_utf16(name) {
        return format!("{text:?}");
    }
    let mut shown = String::from("\"");
    let mut run = String::new();
    let flush = |run: &mut String, shown: &mut String| {
        let quoted = format!("{run:?}");
        shown.push_str(&quoted[1..quoted.len() - 1]);
        run.clear();
    };
    for decoded in char::decode_utf16(name.iter().copied()) {
        match decoded {
            Ok(c) => run.push(c),
            Err(e) => {
                flush(&mut run, &mut shown);
                shown.push_str(&format!(r"\u{{{:04X}}}", e.unpaired_surrogate()));
            }
        }
    }
    flush(&mut run, &mut shown);
    shown.push('"');
    shown
}

/// What a read of a service's registry values found.
pub enum RegistryReadOutcome<T> {
    /// The registry says the service no longer exists. That is its word, not proof: a caller that
    /// must not take a live service for gone asks SCM too (the sweep does).
    ServiceGone,
    Read(T),
}

/// The contents of a `Parameters` key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parameters {
    /// goetia's fields that hold a string, under goetia's spelling of the name, with the text read.
    pub fields: BTreeMap<String, String>,
    /// Every other value and subkey, and each field stored other than as goetia writes it.
    pub strays: Vec<Stray>,
}

impl Parameters {
    /// Sort one value, as stored, into [`fields`](Self::fields) or [`strays`](Self::strays).
    pub fn add_value(&mut self, name: Vec<u16>, vtype: u32, data: &[u8]) {
        match classify_value(name, vtype, data) {
            Entry::Field { name, text, layout } => {
                debug_assert!(
                    !self.fields.contains_key(name),
                    "the registry holds no two value names that fold to one: {name} repeats"
                );
                self.fields.insert(name.to_string(), text);
                self.strays.extend(layout);
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
    /// `layout` is the [`Stray::Layout`] when the value is not stored as goetia stores it.
    Field {
        name: &'static str,
        text: String,
        layout: Option<Stray>,
    },
    Stray(Stray),
}

/// A value is a field when its name spells one of goetia's four in any case and it holds one string
/// of text in any string type. A `Marker` is goetia's only when its text, less its closing NULs, is
/// goetia's marker string in any case, in any layout; it is then the field `Marker` with goetia's
/// own text, so ownership is decided here and nowhere else. A third-party service may hold a
/// `Marker` of its own, and must stay foreign. Anything else under a field's name is a stray, so a
/// hand edit never reads as equal to what goetia wants. A field not stored exactly as goetia writes
/// it, in layout or (a `Marker`) in case, comes with a [`Stray::Layout`].
fn classify_value(name: Vec<u16>, vtype: u32, data: &[u8]) -> Entry {
    if let Some(field) = field_named_utf16(&name)
        && let Some((text, exact)) = string_text(vtype, data)
        && (field != FIELD_MARKER || text.eq_ignore_ascii_case(blob::MARKER))
    {
        let (text, exact) = if field == FIELD_MARKER {
            let exact = exact && text == blob::MARKER;
            (blob::MARKER.to_string(), exact)
        } else {
            (text, exact)
        };
        let layout = (!exact).then_some(Stray::Layout { name: field, vtype });
        return Entry::Field {
            name: field,
            text,
            layout,
        };
    }
    Entry::Stray(Stray::Value { name, vtype })
}

/// Goetia's spelling of the field `name` spells in any ASCII case; `None` for any other name,
/// including one that is not valid UTF-16.
pub(crate) fn field_named_utf16(name: &[u16]) -> Option<&'static str> {
    String::from_utf16(name).ok().and_then(|name| field_named(&name))
}

/// Whether `vtype` is a string type: `REG_SZ`, `REG_EXPAND_SZ` or `REG_MULTI_SZ`.
pub(crate) fn is_string_type(vtype: u32) -> bool {
    [vtype::REG_SZ, vtype::REG_EXPAND_SZ, vtype::REG_MULTI_SZ].contains(&vtype)
}

/// The text of a string value of any string type (`REG_SZ`, `REG_EXPAND_SZ`, `REG_MULTI_SZ`), with
/// the NULs that close it removed, and whether it is laid out as goetia writes one: a `REG_SZ` with
/// one closing NUL. `None` for another type, an odd byte count, a NUL inside the text, or text that
/// is not valid UTF-16.
fn string_text(vtype: u32, data: &[u8]) -> Option<(String, bool)> {
    if !is_string_type(vtype) {
        return None;
    }
    let (pairs, remainder) = data.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    let body = &units[..units.iter().rposition(|unit| *unit != 0).map_or(0, |last| last + 1)];
    if body.contains(&0) {
        return None;
    }
    let text = String::from_utf16(body).ok()?;
    Some((text, vtype == vtype::REG_SZ && units.len() == body.len() + 1))
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
