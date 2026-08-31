//! Pure generation of the plist from a resolved spec, and extraction of
//! the embedded metadata blob back out of one.
//!
//! Not `#[cfg]`-gated: this compiles and its tests run on every platform.
//!
//! **Enablement is not a plist key.** `launchd.plist(5)`: "The use of
//! `KeepAlive` implicitly implies `RunAtLoad`, causing launchd to
//! speculatively launch the job" — so any job carrying a restart policy
//! runs the instant it is loaded, and `Disabled: true` fares no better:
//! `launchctl bootstrap` refuses such a plist outright. Enrollment is
//! therefore expressed by the plist's *directory*, never by its content,
//! and this module emits `Disabled` under no circumstance. That is also
//! why the generated text is byte-identical whether or not the daemon is
//! enabled, and the drift-detection invariant needs no macOS special case.
//!
//! Metadata lives in an XML comment right after the DOCTYPE, delimited by
//! a `goetia:begin` / `goetia:end` sentinel pair unlikely to appear in any
//! human-written comment. It is emitted by hand rather than through the
//! `plist` crate's serializer, which would not preserve a comment at all.
//! The `Spec` field is base64, whose alphabet (`A-Za-z0-9+/=`) cannot
//! contain the `--` that would otherwise let a value break out of an XML
//! comment early.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::backend::Identity;
use crate::blob::{self, Blob};
use crate::error::Error;
use crate::spec::{DaemonSpec, Restart};

// Metadata comment ====================================================================================================

const COMMENT_BEGIN: &str = "<!-- goetia:begin";
const COMMENT_END: &str = "goetia:end -->";

/// Render the metadata comment carrying `Marker`, `Schema`, `Version`, and
/// `Spec` — byte-exact field names, per the crate's global metadata
/// convention (shared with the systemd `[X-Goetia]` section).
fn metadata_comment(spec: &DaemonSpec) -> String {
    format!(
        "{COMMENT_BEGIN}\nMarker: {marker}\nSchema: {schema}\nVersion: {version}\nSpec: {encoded}\n{COMMENT_END}\n",
        marker = blob::MARKER,
        schema = blob::SCHEMA,
        version = crate::version(),
        encoded = blob::encode(spec),
    )
}

/// Locate every `goetia:begin` / `goetia:end` comment in `text` and return
/// each one's inner content, with exactly the line break on each side of
/// the sentinels trimmed. An unterminated `goetia:begin` (a sentinel with
/// no matching `goetia:end`) is a malformed metadata comment, not a
/// foreign one, so it errors rather than being silently skipped.
///
/// The sentinel constants deliberately exclude the line break that
/// follows/precedes them: matching on the literal byte `\n` would miss a
/// plist whose line endings became `\r\n` (e.g. saved by a Windows
/// editor), silently reporting a fully intact metadata comment as absent
/// instead of extracting it.
fn find_metadata_blocks(text: &str) -> Result<Vec<&str>, Error> {
    let mut blocks = Vec::new();
    let mut cursor = 0;

    while let Some(begin_rel) = text[cursor..].find(COMMENT_BEGIN) {
        let content_start = cursor + begin_rel + COMMENT_BEGIN.len();
        let Some(end_rel) = text[content_start..].find(COMMENT_END) else {
            return Err(Error::Blob(
                "goetia metadata comment has a `goetia:begin` with no matching `goetia:end`".to_string(),
            ));
        };
        let content_end = content_start + end_rel;
        blocks.push(trim_one_line_break(&text[content_start..content_end]));
        cursor = content_end + COMMENT_END.len();
    }

    Ok(blocks)
}

/// Trim exactly one leading and one trailing line break (`\r\n` or `\n`)
/// from `s`, leaving any interior line breaks untouched.
fn trim_one_line_break(s: &str) -> &str {
    let s = s.strip_prefix("\r\n").or_else(|| s.strip_prefix('\n')).unwrap_or(s);
    s.strip_suffix("\r\n").or_else(|| s.strip_suffix('\n')).unwrap_or(s)
}

/// Parse one metadata comment's content (`Marker`/`Schema`/`Version`/`Spec`,
/// one per line, in that order) and decode the embedded blob. `Schema` and
/// `Version` are read and discarded here — `blob::decode` re-derives both,
/// authoritatively, from the `Spec` payload itself; the plain-text copies
/// exist only so a human reading the artifact does not have to decode
/// base64 to see them.
fn parse_metadata_block(content: &str) -> Result<Blob, Error> {
    let mut lines = content.lines();

    let marker = read_field(&mut lines, "Marker")?;
    let _schema = read_field(&mut lines, "Schema")?;
    let _version = read_field(&mut lines, "Version")?;
    let spec = read_field(&mut lines, "Spec")?;

    if lines.next().is_some() {
        // A `Spec` value can only reach here already broken: ours never
        // contains whitespace, so an embedded newline (or anything else
        // that produced a further line) is a hand-edit or bit-rot. Reject
        // it outright rather than silently decoding a truncated prefix.
        return Err(Error::Blob(
            "goetia metadata comment has content after `Spec`".to_string(),
        ));
    }

    if marker != blob::MARKER {
        return Err(Error::Blob(format!(
            "goetia metadata comment names marker `{marker}`, expected `{}`",
            blob::MARKER
        )));
    }

    blob::decode(spec)
}

fn read_field<'a>(lines: &mut std::str::Lines<'a>, name: &str) -> Result<&'a str, Error> {
    let line = lines
        .next()
        .ok_or_else(|| Error::Blob(format!("goetia metadata comment is missing `{name}`")))?;
    let prefix = format!("{name}: ");
    line.strip_prefix(prefix.as_str())
        .ok_or_else(|| Error::Blob(format!("goetia metadata comment: expected `{name}: ...`, got `{line}`")))
}

/// Extract the metadata blob embedded in a generated plist, if any.
///
/// - No `goetia:begin`/`goetia:end` comment at all: `Ok(None)` — a
///   foreign plist, not one of ours.
/// - Exactly one, but its `Spec` payload does not decode (or the comment
///   is otherwise malformed): `Err`. A marker without a usable blob is
///   never silently treated as absent.
/// - More than one: `Err`. A forged second comment must not be able to
///   choose which blob wins.
pub fn extract(plist_text: &str) -> Result<Option<Blob>, Error> {
    let blocks = find_metadata_blocks(plist_text)?;
    match blocks.as_slice() {
        [] => Ok(None),
        [only] => parse_metadata_block(only).map(Some),
        _ => Err(Error::Blob(format!(
            "plist carries {} goetia metadata comments, expected at most one",
            blocks.len()
        ))),
    }
}

// XML rendering =======================================================================================================

/// Escape the three characters XML element content cannot contain
/// unescaped. Applied to every emitted value — argv entries, paths,
/// environment keys and values, and the resolved account name — since
/// `spec::resolve` only rejects control characters, not `&`/`<`/`>`.
///
/// XML 1.0's `Char` production also excludes the noncharacters U+FFFE and
/// U+FFFF entirely — no escape can represent them — and `spec::resolve`'s
/// gate (`char::is_control()`) does not currently reject them. Closing
/// that gate belongs in `spec::resolve`, out of this module's file scope;
/// until it is closed, this panics loudly on the two codepoints rather
/// than silently emit XML no parser can load.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        assert!(
            !matches!(c, '\u{FFFE}' | '\u{FFFF}'),
            "value contains {c:?} (U+FFFE/U+FFFF), which XML 1.0 cannot represent at all; \
             spec::resolve must reject this upstream, before it ever reaches a generator"
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

/// `DaemonSpec`'s `cwd`/`logs` are only ever constructed from `String`
/// sources — `spec::resolve` parses YAML text, `blob::decode` parses JSON
/// text — so a `DaemonSpec` reaching this generator always carries valid
/// UTF-8 paths. Panic loudly on a violation of that contract rather than
/// let `to_string_lossy` silently substitute U+FFFD and emit a plist that
/// names a path that does not exist.
fn path_str(p: &Path) -> &str {
    p.to_str()
        .expect("DaemonSpec paths are always valid UTF-8 (see spec::resolve and blob::decode)")
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

/// `restart: never` omits `KeepAlive` (and, with it, `RunAtLoad`) entirely,
/// so such a job never starts on its own — not at load, not at boot once
/// enabled. This is deliberate, not an oversight: the plan's settled
/// launchd semantics name `launchctl kickstart` as the mechanism for
/// starting "a job with no `KeepAlive`/`RunAtLoad`", which only makes
/// sense if `restart: never` is exactly that job shape. `on-failure`
/// emits a dict with `SuccessfulExit: false`; `always` emits the bare
/// `true` form. Both non-`never` variants also emit `RunAtLoad: true`
/// explicitly: harmless alongside the implication `KeepAlive` already
/// carries, and it keeps the plist self-documenting about a property the
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

/// launchd's `ThrottleInterval` is integer seconds. Rounding up (never
/// down) is mandatory, not cosmetic: `500ms` truncated to `0` would
/// *disable* throttling and yield an unbounded respawn storm. An exact
/// `0` the caller authored is left alone — that is a deliberate opt-out,
/// not a value in need of rounding. `saturating_add`, not `+`: `as_secs()`
/// can legitimately be `u64::MAX` (e.g. `Duration::MAX`), and a plain add
/// would overflow — wrapping to `0` in release, which is exactly the
/// "disables throttling" failure this rounding exists to prevent.
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
/// Never emits `Disabled` — see the module doc comment. Emits `Label`,
/// `ProgramArguments`, `WorkingDirectory` (if `cwd`), `EnvironmentVariables`
/// (if `env` is non-empty), `UserName`, `KeepAlive`/`RunAtLoad` (per
/// `restart`), `ThrottleInterval` (if `restart != never` and
/// `restart_delay` is set), and `StandardOutPath`/`StandardErrorPath` (if
/// `logs`, both pointed at the same file — the spec has one log path, not
/// separate stdout/stderr ones).
pub fn plist(spec: &DaemonSpec, id: &Identity) -> String {
    const INDENT: &str = "\t";
    let mut dict = String::new();

    push_key_string(&mut dict, INDENT, "Label", spec.id.as_str());
    push_key_string_array(&mut dict, INDENT, "ProgramArguments", &spec.command);

    if let Some(cwd) = &spec.cwd {
        push_key_string(&mut dict, INDENT, "WorkingDirectory", path_str(cwd));
    }

    if !spec.env.is_empty() {
        push_key_string_dict(&mut dict, INDENT, "EnvironmentVariables", &spec.env);
    }

    push_key_string(&mut dict, INDENT, "UserName", &id.user);

    push_keep_alive(&mut dict, INDENT, spec.restart);

    if spec.restart != Restart::Never {
        if let Some(delay) = spec.restart_delay {
            push_key_integer(&mut dict, INDENT, "ThrottleInterval", round_up_seconds(delay));
        }
    }

    if let Some(logs) = &spec.logs {
        let path = path_str(logs);
        push_key_string(&mut dict, INDENT, "StandardOutPath", path);
        push_key_string(&mut dict, INDENT, "StandardErrorPath", path);
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         {comment}\
         <plist version=\"1.0\">\n\
         <dict>\n\
         {dict}\
         </dict>\n\
         </plist>\n",
        comment = metadata_comment(spec),
    )
}

#[cfg(test)]
#[path = "generate_tests.rs"]
mod generate_tests;
