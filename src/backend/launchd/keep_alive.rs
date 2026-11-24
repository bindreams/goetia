//! Whether launchd keeps a job alive, read from the job's own plist. Pure, so it compiles and its tests
//! run on every platform, unlike `manager`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::backend::launchd::generate;

/// Why a plist's `KeepAlive` could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unreadable {
    #[error("the plist is not well-formed XML")]
    Xml(#[source] roxmltree::Error),
    #[error("the plist's root is not a dict")]
    NoDict,
    #[error("the plist's dict has a `KeepAlive` key with no value")]
    NoValue,
    #[error("the plist's dict has a `KeepAlive` key more than once")]
    Twice,
    #[error("the plist's `KeepAlive` is a <{0}>, which is neither a boolean nor a dict")]
    Type(String),
}

/// Whether launchd keeps the job whose plist is `plist` alive: restarts it from its own memory after the
/// process ends, whatever is later done to the file (measured on macOS 26.7.2: a removed plist changes
/// nothing for a loaded job).
///
/// A job's top-level `KeepAlive` decides: absent or `false` is not kept alive, `true` is, and a dict is,
/// since launchd restarts the job whenever one of its conditions holds and goetia writes one
/// (`SuccessfulExit: false`, for `restart: on-failure`). A plist that is not a job's dict, or whose key
/// has another value, is [`Unreadable`]. goetia's metadata comment is not read: the encoded spec in it
/// may hold `--`, which XML forbids in a comment, and launchd does not use it.
pub(crate) fn keeps_alive(plist: &str) -> Result<bool, Unreadable> {
    let text = generate::without_metadata(plist);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let document = roxmltree::Document::parse_with_options(&text, options).map_err(Unreadable::Xml)?;
    let dict = document
        .root_element()
        .children()
        .find(roxmltree::Node::is_element)
        .filter(|node| node.has_tag_name("dict"))
        .ok_or(Unreadable::NoDict)?;
    let mut elements = dict.children().filter(roxmltree::Node::is_element);
    let mut value = None;
    while let Some(key) = elements.next() {
        let paired = elements.next();
        if key.has_tag_name("key") && key.text().map(str::trim) == Some("KeepAlive") {
            if value.is_some() {
                return Err(Unreadable::Twice);
            }
            value = Some(paired.ok_or(Unreadable::NoValue)?);
        }
    }
    match value {
        None => Ok(false),
        Some(node) => match node.tag_name().name() {
            "true" | "dict" => Ok(true),
            "false" => Ok(false),
            other => Err(Unreadable::Type(other.to_string())),
        },
    }
}

#[cfg(test)]
#[path = "keep_alive_tests.rs"]
mod keep_alive_tests;
