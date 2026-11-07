//! Printing an [`Error`]: its chain of causes, then each remedy found along it.

use std::fmt::{self, Write as _};

use super::Error;
use super::escape::{write_escaped, write_escaped_keeping_layout};

/// What [`Error::report`] and [`Error::report_unescaped`] return: the chain, formatted on `Display`.
pub(super) struct Report<'a> {
    pub(super) error: &'a Error,
    pub(super) escaped: bool,
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = Sink::new(f, self.escaped);
        let mut remedies = Vec::new();
        write_chain(self.error, &mut out, &mut remedies)?;
        write_remedies(&mut out, &remedies)
    }
}

const LAYOUT_INDENT: &str = "  ";

/// Where error text goes. The one place it is written for a person is [`write_chain`], so the one
/// place it is escaped: `escaped` is whether this destination is one. The text goes through
/// [`fmt::Write`], which escapes it whole; a child's stderr is written with
/// [`write_layout`](Sink::write_layout), which keeps its line feeds and tabs.
pub(super) struct Sink<'a> {
    out: &'a mut dyn fmt::Write,
    escaped: bool,
}

impl<'a> Sink<'a> {
    pub(super) fn new(out: &'a mut dyn fmt::Write, escaped: bool) -> Self {
        Sink { out, escaped }
    }

    /// Writes `text`, a child's diagnostic. For a person it keeps its line breaks and tabs, and
    /// indents every line after the first, so that no line of it starts where goetia's own lines do
    /// (a stderr of `error: ...` is not goetia saying it). A blank line has nothing to indent.
    fn write_layout(&mut self, text: &str) -> fmt::Result {
        if !self.escaped {
            return self.out.write_str(text);
        }
        for (i, line) in text.split_inclusive('\n').enumerate() {
            if i > 0 && !matches!(line, "\n" | "\r\n") {
                self.out.write_str(LAYOUT_INDENT)?;
            }
            write_escaped_keeping_layout(self.out, line)?;
        }
        Ok(())
    }
}

impl fmt::Write for Sink<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.escaped {
            write_escaped(self.out, text)
        } else {
            self.out.write_str(text)
        }
    }
}

/// An error and its causes, without the remedies [`Report`] prints after them: how an error that
/// sits inside another's sentence is written. The outer [`write_chain`] collects the remedies of
/// what it embeds, so they still print once, last.
///
/// Only an [`Error`] embeds through [`Error::embedded`]. The other users, `Occupant::Unreadable` and
/// the SCM kill's `ReadAfterRefusedOpen`, hold `io::Error`s, which carry no remedy: a remedy lives
/// on [`Error::Undetermined`], and no goetia error is wrapped in an `io::Error`.
///
/// Not escaped: the text lands in an error's `Display`, which the sink that prints it escapes.
pub(crate) struct Chain<'a>(pub(crate) &'a (dyn std::error::Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_chain(self.0, &mut Sink::new(f, false), &mut Vec::new())
    }
}

/// Writes `e` and its causes, collecting the remedies met on the way into `remedies`, outermost
/// first and each once: a [`Both`](Error::Both)'s halves can carry the same one. The remedies of
/// the errors `e` embeds in its own text (see [`embedded`]) are collected too, and not written.
pub(super) fn write_chain<'a>(
    e: &'a (dyn std::error::Error + 'static),
    out: &mut Sink,
    remedies: &mut Vec<&'a str>,
) -> fmt::Result {
    // A `#[source]` of type `Box<Error>` reaches here as the box.
    let ours = e
        .downcast_ref::<Error>()
        .or_else(|| e.downcast_ref::<Box<Error>>().map(|b| &**b));
    if let Some(Error::Both { first, then }) = ours {
        write_chain(first.as_ref(), out, remedies)?;
        out.write_str("; then: ")?;
        return write_chain(then.as_ref(), out, remedies);
    }
    if let Some(remedy) = ours.and_then(Error::remedy)
        && !remedies.contains(&remedy)
    {
        remedies.push(remedy);
    }
    write!(out, "{e}")?;
    if let Some(stderr) = ours.and_then(Error::stderr).filter(|stderr| !stderr.is_empty()) {
        out.write_str(": ")?;
        out.write_layout(stderr)?;
    }
    if let Some(inner) = ours.and_then(Error::embedded) {
        write_chain(inner, &mut Sink::new(&mut Discard, false), remedies)?;
    }
    if let Some(source) = e.source() {
        out.write_str(": ")?;
        write_chain(source, out, remedies)?;
    }
    Ok(())
}

/// Writes each of `remedies` as `. remedy`.
pub(super) fn write_remedies(out: &mut Sink, remedies: &[&str]) -> fmt::Result {
    for remedy in remedies {
        write!(out, ". {remedy}")?;
    }
    Ok(())
}

struct Discard;

impl fmt::Write for Discard {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        Ok(())
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;
