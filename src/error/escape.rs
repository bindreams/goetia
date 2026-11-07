//! Escaping what a message quotes, once, where text is written for a person.
//!
//! A value from a manifest, an environment file, the command line or a child's stderr can hold a
//! control character (a newline starts a line of its own, an escape sequence moves the cursor) or a
//! direction control (U+202E reverses the text after it). Messages are built from plain values; the
//! report and the CLI's other human sinks escape on the way out.

use std::fmt;
#[cfg(feature = "cli")]
use std::io;

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
    write_keeping(out, text, false)
}

/// [`write_escaped`], except that a line feed, a tab and a carriage return that ends a line (CRLF,
/// as Windows tools write it) are written as they are: the layout of text that is multi-line by
/// nature, a child's diagnostic. A carriage return not followed by a line feed, the last character
/// of `text` included, is escaped.
pub(super) fn write_escaped_keeping_layout(out: &mut dyn fmt::Write, text: &str) -> fmt::Result {
    write_keeping(out, text, true)
}

fn write_keeping(out: &mut dyn fmt::Write, text: &str, keep_layout: bool) -> fmt::Result {
    let mut written = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next = chars.peek().map(|&(_, next)| next);
        let layout = matches!(c, '\n' | '\t') || (c == '\r' && next == Some('\n'));
        if !is_escaped(c) || (keep_layout && layout) {
            continue;
        }
        out.write_str(&text[written..at])?;
        write!(out, "{}", c.escape_unicode())?;
        written = at + c.len_utf8();
    }
    out.write_str(&text[written..])
}

/// `text` as [`write_escaped`] writes it.
#[cfg(feature = "cli")]
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    write_escaped(&mut out, text).expect("writing to a String cannot fail");
    out
}

/// An [`io::Write`] that writes everything to `inner` with the set escaped, keeping line feeds and
/// tabs, and a carriage return before a line feed ([`write_escaped_keeping_layout`]): the stream all of the CLI's output goes through, so a
/// sink that forgets to escape cannot put a control character on the terminal. A line feed passes,
/// so a quoted value in a one-line message could still start a line of its own: those messages are
/// escaped whole as well.
///
/// A write may end inside a multi-byte character, or on a carriage return that the next byte will
/// show to be the end of a CRLF, so that tail is held until the next write, across a flush. Bytes
/// that are not UTF-8 are written as U+FFFD; a tail still held when the stream is dropped is
/// written as U+FFFD, or escaped if it is the carriage return.
#[cfg(feature = "cli")]
pub(crate) struct EscapedStream<'a> {
    inner: &'a mut dyn io::Write,
    held: Vec<u8>,
}

#[cfg(feature = "cli")]
impl<'a> EscapedStream<'a> {
    pub(crate) fn new(inner: &'a mut dyn io::Write) -> Self {
        EscapedStream {
            inner,
            held: Vec::new(),
        }
    }

    /// Writes `rest`, holding what only the next write can decide.
    fn feed(&mut self, mut rest: &[u8]) -> io::Result<()> {
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    match text.strip_suffix('\r') {
                        Some(text) => {
                            self.put(text)?;
                            self.held = b"\r".to_vec();
                        }
                        None => self.put(text)?,
                    }
                    break;
                }
                Err(e) => {
                    let (valid, bad) = rest.split_at(e.valid_up_to());
                    self.put(std::str::from_utf8(valid).expect("`valid_up_to` is valid UTF-8"))?;
                    match e.error_len() {
                        None => {
                            self.held = bad.to_vec();
                            break;
                        }
                        Some(len) => {
                            self.put("\u{fffd}")?;
                            rest = &bad[len..];
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn put(&mut self, text: &str) -> io::Result<()> {
        let mut out = IoAsFmt {
            inner: &mut *self.inner,
            error: None,
        };
        write_escaped_keeping_layout(&mut out, text).map_err(|_| {
            out.error
                .take()
                .unwrap_or_else(|| io::Error::other("formatting failed"))
        })
    }
}

#[cfg(feature = "cli")]
impl io::Write for EscapedStream<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.held.is_empty() {
            self.feed(buf)?;
        } else {
            let mut joined = std::mem::take(&mut self.held);
            joined.extend_from_slice(buf);
            self.feed(&joined)?;
        }
        Ok(buf.len())
    }

    /// Flushes the inner stream. A held tail stays held: a flush does not say no byte is due, and
    /// a CRLF split by one must stay a CRLF.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(feature = "cli")]
impl Drop for EscapedStream<'_> {
    fn drop(&mut self) {
        if !self.held.is_empty() {
            let tail = if self.held == b"\r" { "\r" } else { "\u{fffd}" };
            let _ = self.put(tail);
        }
    }
}

/// An [`io::Write`] seen as a [`fmt::Write`], keeping the I/O error `fmt::Error` cannot carry.
#[cfg(feature = "cli")]
struct IoAsFmt<'a> {
    inner: &'a mut dyn io::Write,
    error: Option<io::Error>,
}

#[cfg(feature = "cli")]
impl fmt::Write for IoAsFmt<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.inner.write_all(text.as_bytes()).map_err(|e| {
            self.error = Some(e);
            fmt::Error
        })
    }
}

#[cfg(test)]
#[path = "escape_tests.rs"]
mod escape_tests;
