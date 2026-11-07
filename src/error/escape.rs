//! Escaping what a message quotes, once, where text is written for a person.
//!
//! A value from a manifest, an environment file, the command line or a child's stderr can hold a
//! control character (a newline starts a line of its own, an escape sequence moves the cursor) or a
//! direction control (U+202E reverses the text after it). Messages are built from plain values; the
//! report and the CLI's other human sinks escape on the way out.

use std::fmt;

/// What is escaped: the controls (Cc: C0, DEL and C1), the line and paragraph separators, and the
/// bidirectional embeddings, overrides and isolates. The rest of the format characters (ZWNJ, ZWJ,
/// the soft hyphen, the direction marks) are text in some scripts and stay.
fn is_escaped(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Writes `text` with every escaped character as `\u{XXXX}`, never as `\n` or `\t`, so a backslash
/// and a letter in a Windows path (`C:\new`) cannot be mistaken for an escaped newline. Backslashes
/// stay as they are. Escaping twice changes nothing.
pub(super) fn write_escaped(out: &mut dyn fmt::Write, text: &str) -> fmt::Result {
    write_keeping(out, text, |_| false)
}

/// [`write_escaped`], except that a line feed and a tab are written as they are: the layout of text
/// that is multi-line by nature, a child's diagnostic.
pub(super) fn write_escaped_keeping_layout(out: &mut dyn fmt::Write, text: &str) -> fmt::Result {
    write_keeping(out, text, |c| matches!(c, '\n' | '\t'))
}

fn write_keeping(out: &mut dyn fmt::Write, text: &str, keep: fn(char) -> bool) -> fmt::Result {
    let mut plain = text;
    while let Some(at) = plain.find(|c| is_escaped(c) && !keep(c)) {
        let (before, rest) = plain.split_at(at);
        let c = rest.chars().next().expect("`find` stopped on a character");
        out.write_str(before)?;
        write!(out, "{}", c.escape_unicode())?;
        plain = &rest[c.len_utf8()..];
    }
    out.write_str(plain)
}

/// `text` as [`write_escaped`] writes it.
#[cfg(feature = "cli")]
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    write_escaped(&mut out, text).expect("writing to a String cannot fail");
    out
}

#[cfg(test)]
#[path = "escape_tests.rs"]
mod escape_tests;
