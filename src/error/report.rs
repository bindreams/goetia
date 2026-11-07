//! Printing an [`Error`]: its chain of causes, then each remedy found along it.

use std::fmt;

use super::Error;

/// What [`Error::report`] returns: the chain, formatted on `Display`.
pub(super) struct Report<'a>(pub(super) &'a Error);

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut remedies = Vec::new();
        write_chain(self.0, f, &mut remedies)?;
        write_remedies(f, &remedies)
    }
}

/// An error and its causes, without the remedies [`Report`] prints after them: how an error that
/// sits inside another's sentence is written. The outer [`write_chain`] collects the remedies of
/// what it embeds, so they still print once, last.
///
/// Only an [`Error`] embeds through [`Error::embedded`]. The other users, `Occupant::Unreadable` and
/// the SCM kill's `ReadAfterRefusedOpen`, hold `io::Error`s, which carry no remedy: a remedy lives
/// on [`Error::Undetermined`], and no goetia error is wrapped in an `io::Error`.
pub(crate) struct Chain<'a>(pub(crate) &'a (dyn std::error::Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_chain(self.0, f, &mut Vec::new())
    }
}

/// Writes `e` and its causes, collecting the remedies met on the way into `remedies`, outermost
/// first and each once: a [`Both`](Error::Both)'s halves can carry the same one. The remedies of
/// the errors `e` embeds in its own text (see [`embedded`]) are collected too, and not written.
pub(super) fn write_chain<'a>(
    e: &'a (dyn std::error::Error + 'static),
    out: &mut dyn fmt::Write,
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
    if let Some(inner) = ours.and_then(Error::embedded) {
        write_chain(inner, &mut Discard, remedies)?;
    }
    if let Some(source) = e.source() {
        out.write_str(": ")?;
        write_chain(source, out, remedies)?;
    }
    Ok(())
}

/// The first OS error code in `e`: its own, then its source's, then that of what it embeds. A
/// [`Both`](Error::Both) is searched first half first; [`Error::Kept`] is searched as the error it
/// holds.
pub(super) fn os_error_in(e: &(dyn std::error::Error + 'static)) -> Option<i32> {
    if let Some(code) = e
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
    {
        return Some(code);
    }
    let ours = e
        .downcast_ref::<Error>()
        .or_else(|| e.downcast_ref::<Box<Error>>().map(|b| &**b));
    match ours {
        Some(Error::Both { first, then }) => return os_error_in(first.as_ref()).or_else(|| os_error_in(then.as_ref())),
        Some(Error::Kept(kept)) => return os_error_in(kept.as_ref()),
        _ => {}
    }
    e.source()
        .and_then(os_error_in)
        .or_else(|| ours.and_then(Error::embedded).and_then(|inner| os_error_in(inner)))
}

/// Writes each of `remedies` as `. remedy`.
pub(super) fn write_remedies(out: &mut dyn fmt::Write, remedies: &[&str]) -> fmt::Result {
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
