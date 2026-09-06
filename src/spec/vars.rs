//! `Vars`: a literal reader for the `.env` file beside a `goetia.yaml`
//! manifest.
//!
//! Every value `Vars` produces is exactly the bytes written after `=` in
//! the file, quoting aside — nothing is expanded and the process
//! environment never participates, in either direction. That is not a
//! feature gap; it is the whole point. `goetia` stores a resolved spec
//! inside each installed artifact and re-derives it later, from the same
//! manifest and `.env`, to detect drift. `install` runs elevated and
//! `diff` runs unelevated, and the two processes' ambient environments
//! are not guaranteed to agree even when nothing about the daemon has
//! changed. A variable source that consulted the process environment
//! would make drift detection depend on who ran the command, reporting a
//! spurious difference on an artifact nobody touched.
//!
//! # Why hand-written rather than a crate
//!
//! The repo otherwise prefers a dependency to a reimplementation. That is
//! deliberately not followed here, because no published crate provides
//! the required semantics, and the two closest were measured against
//! this file's grammar rather than assumed:
//!
//! - `dotenvy` 0.15.7 performs its own `$`/`${}` expansion, *consulting
//!   the process environment* — precisely the input this design
//!   excludes — and substitutes an empty string for an unset name
//!   (`LOG=$MISSING/app.log` reads back `/app.log`, `B=${NOPE}` reads
//!   back `""`, `A=$$` reads back `""`). It also scans a variable name
//!   with `char::is_alphanumeric()`, so `_` terminates one where this
//!   grammar's `^[A-Za-z_][A-Za-z0-9_]*$` continues it, and it joins
//!   physical lines inside a quoted value, so a raw-text pre-guard
//!   against multi-line values is bypassable. Its `remove_bom` helper is
//!   private and unreachable from its own non-mutating iterator API.
//! - `dotenv-parser` 0.1.3 gets the literal semantics right — `$MISSING`,
//!   `${NOPE}`, and `$$` all stay literal — but turns a BOM-prefixed file
//!   into a hard error instead of stripping it, silently drops a
//!   duplicate key instead of rejecting it, silently declines to
//!   unescape `\n` where docker-compose and python-dotenv both do, and
//!   surfaces `pest`'s internal `ParsingError { positives: [...], ... }`
//!   as its public error type.
//!
//! This module is the input path to a tool that writes privileged system
//! services, so both gaps are disqualifying rather than cosmetic. See
//! `raw.rs` for the same rationale applied to a hand-written
//! `Deserialize`.
//!
//! # Grammar
//!
//! A deliberate strict subset of docker-compose's `.env` grammar. Every
//! rule below is pinned by a test in `vars_tests.rs`:
//!
//! - The file is read as bytes first. A leading UTF-8 BOM (`EF BB BF`) is
//!   stripped. A UTF-16 or UTF-32 BOM (`FF FE`, `FE FF`, `FF FE 00 00`, or
//!   `00 00 FE FF`) is rejected with a dedicated error naming the
//!   encoding, because `String::from_utf8` would otherwise report only
//!   that the bytes are not valid UTF-8, naming neither the file's real
//!   problem nor the fix. Anything else is decoded as UTF-8, and a decode
//!   failure produces that same dedicated error rather than a bare
//!   `Error::Io`.
//! - Line terminators are `\n` and `\r\n`.
//! - A line that is entirely whitespace, or whose first non-whitespace
//!   character is `#`, is ignored.
//! - Leading whitespace before a name is allowed and trimmed, matching
//!   compose and python-dotenv.
//! - An optional `export` prefix is recognised only when followed by
//!   whitespace. `export=1` therefore assigns the variable named
//!   `export`, exactly as a shell would.
//! - `NAME=VALUE`, where `NAME` matches `^[A-Za-z_][A-Za-z0-9_]*$` — the
//!   same charset a manifest `${VAR}` reference can name, so an
//!   unreferenceable name is an error rather than silent dead weight.
//! - Whitespace around `=` is an error, not trimmed. python-dotenv
//!   accepts `A = 1` and a shell does not, so accepting it would let one
//!   file mean two different things to two readers of it. This rule and
//!   "an unquoted value has its trailing whitespace trimmed" collide on
//!   exactly one input: `A= ` (an `=` followed only by whitespace to end
//!   of line). There, trimming wins: the line's trailing whitespace is
//!   stripped before the adjacency check runs, so `A= ` means `A=` — an
//!   empty value, not an error — while `A= 1`, `A =1`, and `A = 1` still
//!   error, since none of their offending whitespace is at the end of the
//!   line.
//! - `VALUE` is one of: single-quoted (literal up to the closing `'`, no
//!   escapes); double-quoted (only `\\` and `\"` are decoded); or
//!   unquoted (trailing whitespace trimmed; a `#` preceded by whitespace
//!   starts a comment, a `#` not preceded by whitespace is part of the
//!   value, matching compose).
//! - `\n`, `\r`, `\t`, and every other escape in a double-quoted value
//!   are errors. Decoding one would only ever produce a control
//!   character, which `resolve::reject_control_chars` rejects far
//!   downstream with no `.env` line number; rejecting it here is the
//!   same refusal with a better message, and it keeps a compose-authored
//!   file from silently meaning something different. A literal
//!   backslash-then-`n` is written `"a\\nb"`.
//! - A quoted value must close on the same line; multi-line values are
//!   not supported, for the same reason.
//! - Anything other than whitespace or a `#` comment after a closing
//!   quote is an error.
//! - No expansion: `$`, `${...}`, and `$$` are literal characters in a
//!   value.
//! - A duplicate `NAME` is an error naming the line it first appeared
//!   on. This diverges from compose's last-wins: goetia already rejects
//!   a duplicate daemon id rather than silently keeping the second, and
//!   a repeated key in a hand-edited `.env` is a mistake, not an
//!   override.
//! - A missing file is not an error and yields `Vars::empty()`. Only
//!   `io::ErrorKind::NotFound` means "absent"; every other IO failure
//!   (for example a directory sitting at the `.env` path) propagates as
//!   `Error::Io`, so a real failure is never mistaken for a benign
//!   absence.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{Error, Result};

/// The file name `Vars::load` looks for beside a manifest.
pub(crate) const ENV_FILE_NAME: &str = ".env";

/// A `.env` file's variables, fully resolved: every value is exactly the
/// literal text after `=` (quoting aside), with no `$` expansion and no
/// contribution from or to the process environment. See the module doc
/// comment for the full grammar and the rationale for both properties.
#[derive(Debug, Clone)]
pub(crate) struct Vars(BTreeMap<String, String>);

impl Vars {
    /// Read `<base_dir>/.env`. A missing file yields an empty set; any
    /// other read failure is `Error::Io`; a file that exists but does not
    /// follow the grammar in the module doc comment is `Error::EnvFile`.
    pub(crate) fn load(base_dir: &Path) -> Result<Self> {
        let path = base_dir.join(ENV_FILE_NAME);

        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(source) => return Err(Error::Io { path, source }),
        };

        let bytes = strip_bom(&bytes, &path)?;
        let text =
            std::str::from_utf8(bytes).map_err(|_| env_error(&path, 1, "file is not valid UTF-8".to_string()))?;

        // Keyed by name to `(line it was first defined on, its value)`, so
        // a duplicate can be reported without a second map tracking the
        // same keys.
        let mut vars: BTreeMap<String, (usize, String)> = BTreeMap::new();
        for (index, raw_line) in text.split('\n').enumerate() {
            let line = index + 1;
            let Some((name, value)) = parse_line(raw_line, line, &path)? else {
                continue;
            };
            if let Some((first, _)) = vars.get(&name) {
                return Err(env_error(
                    &path,
                    line,
                    format!("duplicate variable `{name}` (first defined on line {first})"),
                ));
            }
            vars.insert(name, (line, value));
        }

        Ok(Self(vars.into_iter().map(|(name, (_, value))| (name, value)).collect()))
    }

    /// A `.env`-less set: what `load` returns for a missing file, and what
    /// callers use as a base when no manifest-adjacent `.env` applies.
    pub(crate) fn empty() -> Self {
        Self(BTreeMap::new())
    }

    /// The literal value of `name`, if `.env` defined it.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

#[cfg(test)]
impl Vars {
    /// Build a `Vars` directly from pairs, bypassing the parser, for tests
    /// that exercise a consumer of `Vars` rather than `Vars` itself.
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }
}

// BOM and decoding ====================================================================================================

/// Strip a UTF-8 BOM, or reject a UTF-16/UTF-32 BOM by name. `line` 1 is
/// used for every error here since the problem is with the file's
/// encoding, not with any one line of text within it.
///
/// The 4-byte UTF-32LE BOM (`FF FE 00 00`) starts with the same two bytes
/// as the UTF-16LE BOM (`FF FE`), so the 4-byte patterns are checked
/// first — otherwise a UTF-32LE file would be misreported as UTF-16LE.
fn strip_bom<'a>(bytes: &'a [u8], path: &Path) -> Result<&'a [u8]> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Ok(rest)
    } else if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        Err(env_error(
            path,
            1,
            "file starts with a UTF-32LE byte-order mark; goetia .env files must be UTF-8".to_string(),
        ))
    } else if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        Err(env_error(
            path,
            1,
            "file starts with a UTF-32BE byte-order mark; goetia .env files must be UTF-8".to_string(),
        ))
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Err(env_error(
            path,
            1,
            "file starts with a UTF-16LE byte-order mark; goetia .env files must be UTF-8".to_string(),
        ))
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Err(env_error(
            path,
            1,
            "file starts with a UTF-16BE byte-order mark; goetia .env files must be UTF-8".to_string(),
        ))
    } else {
        Ok(bytes)
    }
}

// Line parsing ========================================================================================================

/// Whitespace, for every purpose in this grammar: leading trim, the
/// `export`-prefix boundary, and the ban on whitespace around `=`. ASCII
/// only, matching a shell and docker-compose.
fn is_ws(c: char) -> bool {
    c.is_ascii_whitespace()
}

/// Parse one physical line (already split on `\n`, `\r` not yet
/// stripped). Returns `None` for a blank or comment-only line, `Some`
/// name/value otherwise.
fn parse_line(raw_line: &str, line: usize, path: &Path) -> Result<Option<(String, String)>> {
    let line_text = raw_line.strip_suffix('\r').unwrap_or(raw_line);

    let trimmed = line_text.trim_start_matches(is_ws);
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }

    let rest = strip_export_prefix(trimmed);
    // An unquoted value's trailing whitespace is trimmed regardless, so
    // strip it before the whitespace-around-`=` check runs: `A= ` must
    // mean `A=`, not an error. A quoted value is unaffected, since any
    // whitespace inside its quotes is never at the very end of the line.
    let rest = rest.trim_end_matches(is_ws);

    let Some(eq) = rest.find('=') else {
        return Err(env_error(path, line, format!("expected `NAME=VALUE`, found `{rest}`")));
    };

    let name = &rest[..eq];
    let value_text = &rest[eq + 1..];
    if name.ends_with(is_ws) || value_text.starts_with(is_ws) {
        return Err(env_error(
            path,
            line,
            "whitespace is not allowed around `=`; write `A=1`, not `A = 1`".to_string(),
        ));
    }
    if !is_valid_name(name) {
        return Err(env_error(
            path,
            line,
            format!("`{name}` is not a valid variable name (expected ^[A-Za-z_][A-Za-z0-9_]*$)"),
        ));
    }

    let value = parse_value(value_text, path, line)?;
    Ok(Some((name.to_string(), value)))
}

/// Strip a leading `export` from `s`, but only when it is followed by
/// whitespace — `export=1` must fall through untouched, naming the
/// variable `export`. Any further whitespace before the name is trimmed
/// too, same as leading whitespace at the start of the line.
fn strip_export_prefix(s: &str) -> &str {
    match s.strip_prefix("export") {
        Some(after) if after.starts_with(is_ws) => after.trim_start_matches(is_ws),
        _ => s,
    }
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`.
fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// Value parsing =======================================================================================================

/// Parse the text after `=` to the end of the line.
fn parse_value(s: &str, path: &Path, line: usize) -> Result<String> {
    match s.as_bytes().first() {
        Some(b'\'') => parse_single_quoted(s, path, line),
        Some(b'"') => parse_double_quoted(s, path, line),
        _ => Ok(parse_unquoted(s)),
    }
}

/// `'...'`: literal up to the closing quote, no escapes of any kind.
fn parse_single_quoted(s: &str, path: &Path, line: usize) -> Result<String> {
    let inner = &s[1..];
    let Some(end) = inner.find('\'') else {
        return Err(env_error(path, line, "unterminated single-quoted value".to_string()));
    };
    check_trailing(&inner[end + 1..], path, line)?;
    Ok(inner[..end].to_string())
}

/// `"..."`: only `\\` and `\"` decode; every other escape is an error,
/// named with the offending character rather than deferred to
/// `resolve::reject_control_chars`.
fn parse_double_quoted(s: &str, path: &Path, line: usize) -> Result<String> {
    let inner = &s[1..];
    let mut value = String::new();
    let mut chars = inner.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                check_trailing(&inner[i + 1..], path, line)?;
                return Ok(value);
            }
            '\\' => match chars.next() {
                Some((_, '\\')) => value.push('\\'),
                Some((_, '"')) => value.push('"'),
                Some((_, other)) => {
                    return Err(env_error(
                        path,
                        line,
                        format!(
                            "invalid escape `\\{other}` in double-quoted value (only `\\\\` and `\\\"` are supported)"
                        ),
                    ));
                }
                None => return Err(env_error(path, line, "unterminated double-quoted value".to_string())),
            },
            _ => value.push(c),
        }
    }
    Err(env_error(path, line, "unterminated double-quoted value".to_string()))
}

/// Anything but whitespace or a `#` comment after a closing quote is an
/// error.
fn check_trailing(after: &str, path: &Path, line: usize) -> Result<()> {
    let after = after.trim_start_matches(is_ws);
    if after.is_empty() || after.starts_with('#') {
        Ok(())
    } else {
        Err(env_error(
            path,
            line,
            format!("unexpected text after closing quote: `{after}`"),
        ))
    }
}

/// Unquoted: trailing whitespace trimmed; a `#` preceded by whitespace
/// starts a comment, a `#` not preceded by whitespace is part of the
/// value, matching compose.
fn parse_unquoted(s: &str) -> String {
    let mut prev_ws = false;
    let mut comment_start = None;
    for (i, c) in s.char_indices() {
        if c == '#' && prev_ws {
            comment_start = Some(i);
            break;
        }
        prev_ws = is_ws(c);
    }
    let value = match comment_start {
        Some(i) => &s[..i],
        None => s,
    };
    value.trim_end_matches(is_ws).to_string()
}

fn env_error(path: &Path, line: usize, message: String) -> Error {
    Error::EnvFile {
        path: path.to_path_buf(),
        line,
        message,
    }
}

#[cfg(test)]
#[path = "vars_tests.rs"]
mod vars_tests;
