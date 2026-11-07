//! Escaping what a message quotes, once, where a message is written for a person.
//!
//! A value from a manifest, an environment file, the command line or a child's stderr can hold a
//! control character (a newline starts a line of its own, an escape sequence moves the cursor) or
//! a format character (U+202E reverses the text after it). Messages are built from plain values;
//! [`Error::report`](super::Error::report) and the CLI's warnings escape on the way out.

use std::fmt;

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Control characters (category Cc) and format characters (Cf, which includes the bidi overrides).
/// `char::is_control` is Cc only; the standard library has no test for Cf.
fn is_unprintable(c: char) -> bool {
    c.is_control() || c.general_category() == GeneralCategory::Format
}

/// `text` with every unprintable character written as `\u{XXXX}`, never as `\n` or `\t`, so a
/// backslash and a letter in a Windows path (`C:\new`) cannot be mistaken for an escaped newline.
/// Backslashes are left as they are. Escaping twice changes nothing.
#[cfg(feature = "cli")]
pub(crate) fn escape(text: &str) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    Escaping(&mut out)
        .write_str(text)
        .expect("writing to a String cannot fail");
    out
}

/// A [`fmt::Write`] that escapes what it is given before passing it on.
pub(super) struct Escaping<'a>(pub(super) &'a mut dyn fmt::Write);

impl fmt::Write for Escaping<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut plain = text;
        while let Some(at) = plain.find(is_unprintable) {
            let (before, rest) = plain.split_at(at);
            let c = rest.chars().next().expect("`find` stopped on a character");
            self.0.write_str(before)?;
            write!(self.0, "{}", c.escape_unicode())?;
            plain = &rest[c.len_utf8()..];
        }
        self.0.write_str(plain)
    }
}
