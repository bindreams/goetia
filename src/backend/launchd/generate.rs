//! Pure generation of the plist from a resolved spec, and extraction of the embedded metadata blob
//! back out of one. Not `#[cfg]`-gated: it compiles and its tests run everywhere.
//!
//! **Enablement is not a plist key.** `KeepAlive` implies `RunAtLoad`, so a job with a restart
//! policy runs the instant it is loaded, and `launchctl bootstrap` refuses a `Disabled: true`
//! plist. Enrollment is the plist's *directory*, and this module never emits `Disabled`. The
//! generated text is therefore byte-identical whether or not the daemon is enabled, and drift
//! detection needs no macOS special case.
//!
//! Metadata lives in an XML comment right after the DOCTYPE, delimited by a `goetia:begin` /
//! `goetia:end` sentinel pair. It is emitted by hand because the `plist` crate's serializer would
//! not preserve a comment. The `Spec` field is base64, whose alphabet (`A-Za-z0-9+/=`) cannot
//! contain the `--` that would end an XML comment early.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::backend::{Identity, cannot_run_tasks};
use crate::blob::{self, Blob};
use crate::error::Error;
use crate::spec::{DaemonSpec, Restart};

// Metadata comment ====================================================================================================

const COMMENT_BEGIN: &str = "<!-- goetia:begin";
const COMMENT_END: &str = "goetia:end -->";

/// Render the metadata comment carrying `Marker`, `Schema`, `Version`, and `Spec`: byte-exact field
/// names, per the crate's metadata convention (shared with the systemd `[X-Goetia]` section).
fn metadata_comment(spec: &DaemonSpec) -> String {
    format!(
        "{COMMENT_BEGIN}\nMarker: {marker}\nSchema: {schema}\nVersion: {version}\nSpec: {encoded}\n{COMMENT_END}\n",
        marker = blob::MARKER,
        schema = blob::SCHEMA,
        version = crate::version(),
        encoded = blob::encode(spec),
    )
}

/// Locate every `goetia:begin` / `goetia:end` comment in `text` and return each one's inner
/// content, with one line break trimmed on each side of the sentinels. A `goetia:begin` with no
/// matching `goetia:end` is a malformed metadata comment, not a foreign one, so it errors.
///
/// The sentinel constants exclude the adjacent line break: matching a literal `\n` would miss a plist
/// whose line endings became `\r\n` (saved by a Windows editor) and report an intact comment absent.
fn find_metadata_blocks(text: &str) -> Result<Vec<&str>, Error> {
    let mut blocks = Vec::new();
    let mut cursor = 0;

    while let Some(begin_rel) = text[cursor..].find(COMMENT_BEGIN) {
        let content_start = cursor + begin_rel + COMMENT_BEGIN.len();
        let Some(end_rel) = text[content_start..].find(COMMENT_END) else {
            return Err(Error::blob(
                "goetia metadata comment has a `goetia:begin` with no matching `goetia:end`".to_string(),
            ));
        };
        let content_end = content_start + end_rel;
        blocks.push(trim_one_line_break(&text[content_start..content_end]));
        cursor = content_end + COMMENT_END.len();
    }

    Ok(blocks)
}

/// Trim one leading and one trailing line break (`\r\n` or `\n`) from `s`.
fn trim_one_line_break(s: &str) -> &str {
    let s = s.strip_prefix("\r\n").or_else(|| s.strip_prefix('\n')).unwrap_or(s);
    s.strip_suffix("\r\n").or_else(|| s.strip_suffix('\n')).unwrap_or(s)
}

/// Parse one metadata comment's content (`Marker`/`Schema`/`Version`/`Spec`, one per line, in that
/// order) and decode the embedded blob. `Schema` and `Version` are discarded: `blob::decode`
/// re-derives both from `Spec`, and the plain-text copies are for a human reading the artifact.
fn parse_metadata_block(content: &str) -> Result<Blob, Error> {
    let mut lines = content.lines();

    let marker = read_field(&mut lines, "Marker")?;
    let _schema = read_field(&mut lines, "Schema")?;
    let _version = read_field(&mut lines, "Version")?;
    let spec = read_field(&mut lines, "Spec")?;

    if lines.next().is_some() {
        // Our `Spec` never contains whitespace, so a further line is a hand-edit or bit-rot. Reject
        // it rather than decode a truncated prefix.
        return Err(Error::blob(
            "goetia metadata comment has content after `Spec`".to_string(),
        ));
    }

    if marker != blob::MARKER {
        return Err(Error::blob(format!(
            "goetia metadata comment names marker `{marker}`, expected `{}`",
            blob::MARKER
        )));
    }

    blob::decode(spec)
}

fn read_field<'a>(lines: &mut std::str::Lines<'a>, name: &str) -> Result<&'a str, Error> {
    let line = lines
        .next()
        .ok_or_else(|| Error::blob(format!("goetia metadata comment is missing `{name}`")))?;
    let prefix = format!("{name}: ");
    line.strip_prefix(prefix.as_str())
        .ok_or_else(|| Error::blob(format!("goetia metadata comment: expected `{name}: ...`, got `{line}`")))
}

/// Extract the metadata blob embedded in a generated plist, if any.
///
/// - No `goetia:begin`/`goetia:end` comment: `Ok(None)`, a foreign plist.
/// - Exactly one, but its `Spec` does not decode or the comment is otherwise malformed: `Err`. A
///   marker without a usable blob is never treated as absent.
/// - More than one: `Err`, so a forged second comment cannot choose which blob wins.
pub fn extract(plist_text: &str) -> Result<Option<Blob>, Error> {
    let blocks = find_metadata_blocks(plist_text)?;
    match blocks.as_slice() {
        [] => Ok(None),
        [only] => parse_metadata_block(only).map(Some),
        _ => Err(Error::blob(format!(
            "plist carries {} goetia metadata comments, expected at most one",
            blocks.len()
        ))),
    }
}

/// `text` without its metadata comments, each with the line break after it: what is left is the part
/// of a plist launchd reads. The comment carries the spec and the goetia version, which change
/// without the rest changing. An unterminated `goetia:begin` is left as it is, with everything after.
pub(crate) fn without_metadata(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(begin) = rest.find(COMMENT_BEGIN) {
        let after_begin = begin + COMMENT_BEGIN.len();
        let Some(end_rel) = rest[after_begin..].find(COMMENT_END) else {
            break;
        };
        out.push_str(&rest[..begin]);
        let end = after_begin + end_rel + COMMENT_END.len();
        rest = &rest[end..];
        rest = rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
            .unwrap_or(rest);
    }
    out.push_str(rest);
    out
}

// XML rendering =======================================================================================================

/// Escape the three characters XML element content cannot contain unescaped. Applied to every
/// emitted value (argv, paths, environment keys and values, the account name), since
/// `ServiceSpec::new` does not reject `&`/`<`/`>`.
///
/// XML 1.0 excludes U+FFFE and U+FFFF entirely, so no escape can represent them. `ServiceSpec::new`
/// rejects them (and the other Unicode noncharacters); this panics on them as a backstop rather
/// than emit XML no parser can load.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        assert!(
            !matches!(c, '\u{FFFE}' | '\u{FFFF}'),
            "value contains {c:?} (U+FFFE/U+FFFF), which XML 1.0 cannot represent at all; \
             ServiceSpec::new must reject this upstream, before it ever reaches a generator"
        );
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// `ServiceSpec::new` refuses a `cwd` or `logs` that is not valid UTF-8, so a `DaemonSpec`'s are
/// valid. Panic on a violation rather than let `to_string_lossy` substitute U+FFFD and emit a
/// path that does not exist.
fn path_str(p: &Path) -> &str {
    p.to_str()
        .expect("DaemonSpec paths are always valid UTF-8 (see ServiceSpec::new)")
}

fn push_key_string(out: &mut String, indent: &str, key: &str, value: &str) {
    out.push_str(indent);
    out.push_str("<key>");
    out.push_str(key);
    out.push_str("</key>\n");
    out.push_str(indent);
    out.push_str("<string>");
    out.push_str(&xml_escape(value));
    out.push_str("</string>\n");
}

fn push_key_string_array(out: &mut String, indent: &str, key: &str, values: &[String]) {
    out.push_str(indent);
    out.push_str("<key>");
    out.push_str(key);
    out.push_str("</key>\n");
    out.push_str(indent);
    out.push_str("<array>\n");
    let inner = format!("{indent}\t");
    for v in values {
        out.push_str(&inner);
        out.push_str("<string>");
        out.push_str(&xml_escape(v));
        out.push_str("</string>\n");
    }
    out.push_str(indent);
    out.push_str("</array>\n");
}

fn push_key_string_dict(out: &mut String, indent: &str, key: &str, values: &BTreeMap<String, String>) {
    out.push_str(indent);
    out.push_str("<key>");
    out.push_str(key);
    out.push_str("</key>\n");
    out.push_str(indent);
    out.push_str("<dict>\n");
    let inner = format!("{indent}\t");
    for (k, v) in values {
        out.push_str(&inner);
        out.push_str("<key>");
        out.push_str(&xml_escape(k));
        out.push_str("</key>\n");
        out.push_str(&inner);
        out.push_str("<string>");
        out.push_str(&xml_escape(v));
        out.push_str("</string>\n");
    }
    out.push_str(indent);
    out.push_str("</dict>\n");
}

fn push_key_integer(out: &mut String, indent: &str, key: &str, value: u64) {
    out.push_str(indent);
    out.push_str("<key>");
    out.push_str(key);
    out.push_str("</key>\n");
    out.push_str(indent);
    out.push_str("<integer>");
    out.push_str(&value.to_string());
    out.push_str("</integer>\n");
}

fn push_run_at_load(out: &mut String, indent: &str) {
    out.push_str(indent);
    out.push_str("<key>RunAtLoad</key>\n");
    out.push_str(indent);
    out.push_str("<true/>\n");
}

/// `restart: never` omits `KeepAlive` and `RunAtLoad`, so the job never starts on its own, at load
/// or at boot. That is deliberate: `start` launches it with `launchctl kickstart`. `on-failure`
/// emits a dict with `SuccessfulExit: false`; `always` emits the bare `true`. Both also emit
/// `RunAtLoad: true`, redundant with what `KeepAlive` implies, so the plist states the property the
/// design leans on.
fn push_keep_alive(out: &mut String, indent: &str, restart: Restart) {
    match restart {
        Restart::Never => {}
        Restart::OnFailure => {
            out.push_str(indent);
            out.push_str("<key>KeepAlive</key>\n");
            out.push_str(indent);
            out.push_str("<dict>\n");
            let inner = format!("{indent}\t");
            out.push_str(&inner);
            out.push_str("<key>SuccessfulExit</key>\n");
            out.push_str(&inner);
            out.push_str("<false/>\n");
            out.push_str(indent);
            out.push_str("</dict>\n");
            push_run_at_load(out, indent);
        }
        Restart::Always => {
            out.push_str(indent);
            out.push_str("<key>KeepAlive</key>\n");
            out.push_str(indent);
            out.push_str("<true/>\n");
            push_run_at_load(out, indent);
        }
    }
}

/// launchd's `ThrottleInterval` is integer seconds. Round up, never down: `500ms` truncated to `0`
/// would disable throttling and allow an unbounded respawn storm. An authored `0` is left alone, as
/// a deliberate opt-out. `saturating_add`, not `+`: `as_secs()` can be `u64::MAX`
/// (`Duration::MAX`), and a plain add would wrap to `0` in release.
fn round_up_seconds(d: Duration) -> u64 {
    if d.subsec_nanos() == 0 {
        d.as_secs()
    } else {
        d.as_secs().saturating_add(1)
    }
}

// Generation ==========================================================================================================

/// Render `spec` as a launchd plist for the already-resolved account `id`.
///
/// Never emits `Disabled`; see the module doc. Emits `Label`, `ProgramArguments`, `WorkingDirectory`
/// (if `cwd`), `EnvironmentVariables` (if `env` is non-empty), `UserName`, `KeepAlive`/`RunAtLoad`
/// (per `restart`), `ThrottleInterval` (if `restart != never` and `restart_delay` is set), and
/// `StandardOutPath`/`StandardErrorPath` (if `logs`; both name the one log file).
pub fn plist(spec: &DaemonSpec, id: &Identity) -> Result<String, Error> {
    let DaemonSpec::Service(s) = spec else {
        return Err(cannot_run_tasks("launchd", &spec.common().id));
    };
    const INDENT: &str = "\t";
    let mut dict = String::new();

    push_key_string(&mut dict, INDENT, "Label", s.common.id.as_str());
    push_key_string_array(&mut dict, INDENT, "ProgramArguments", &s.common.command);

    if let Some(cwd) = &s.common.cwd {
        push_key_string(&mut dict, INDENT, "WorkingDirectory", path_str(cwd));
    }

    if !s.common.env.is_empty() {
        push_key_string_dict(&mut dict, INDENT, "EnvironmentVariables", &s.common.env);
    }

    push_key_string(&mut dict, INDENT, "UserName", &id.user);

    push_keep_alive(&mut dict, INDENT, s.restart);

    if s.restart != Restart::Never
        && let Some(delay) = s.restart_delay
    {
        push_key_integer(&mut dict, INDENT, "ThrottleInterval", round_up_seconds(delay));
    }

    if let Some(logs) = &s.common.logs {
        let path = path_str(logs);
        push_key_string(&mut dict, INDENT, "StandardOutPath", path);
        push_key_string(&mut dict, INDENT, "StandardErrorPath", path);
    }

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         {comment}\
         <plist version=\"1.0\">\n\
         <dict>\n\
         {dict}\
         </dict>\n\
         </plist>\n",
        comment = metadata_comment(spec),
    ))
}

#[cfg(test)]
#[path = "generate_tests.rs"]
mod generate_tests;
