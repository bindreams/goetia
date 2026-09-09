//! Strict, non-shell `${VAR}` substitution for a parsed manifest.
//!
//! [`scalar`] substitutes one string; [`manifest`] and [`spec`] walk a
//! parsed [`RawManifest`] and call [`scalar`] on every string leaf, in
//! place, before that raw manifest is turned into a
//! [`super::DaemonSpec`] by `resolve_one`.
//!
//! # This must run before `resolve_one`, never after
//!
//! Substitution has to happen on the typed raw manifest, strictly before
//! `resolve_one` runs — never on an already-resolved `DaemonSpec`. This is
//! not a style preference: `resolve_one` is what routes every
//! user-supplied string through `reject_unemittable`/`reject_control_chars`,
//! the only gate standing between a string and a generated service
//! artifact. If substitution ran *after* that gate, a `.env` value could
//! carry a control character (a newline, say) straight into an emitted
//! systemd unit or launchd plist, having never passed the check that
//! exists precisely to catch it. No field in the raw manifest is exempt
//! from this ordering requirement today, and the ordering is the only
//! thing holding that property up — get the call order right.
//!
//! # Why hand-written rather than a crate
//!
//! The repo otherwise prefers a dependency to a reimplementation, but
//! neither of the two closest published crates was usable here:
//! `subst` and `shellexpand` both accept a bare `$VAR` (no braces) as a
//! reference, silently doing the same thing a brace form does, and
//! neither names the offending variable in the error when a reference
//! can't be resolved. This grammar deliberately rejects a bare `$` that
//! is not immediately followed by `{` or another `$` — a user who typed
//! `$HOME` meaning a reference gets an error naming the byte, not a
//! literal `$HOME` written into a unit file.
//!
//! # Grammar
//!
//! Scanned in one left-to-right pass, no regex:
//!
//! - `${NAME}` substitutes the value of `NAME` from `.env`. If `NAME` is
//!   not defined at all, this is an error naming `NAME`.
//! - `${NAME:-default}` substitutes `NAME`'s value if it is defined and
//!   non-empty; otherwise (undefined, or defined as the empty string) it
//!   substitutes `default`. `default` itself may not contain a `$` of any
//!   form — no nested substitution, no escape. This is checked
//!   unconditionally, even when `NAME` is set and `default` is never
//!   used, since grammar validity does not depend on which branch wins.
//! - `default` ends at the first `}`, with no escape for a literal `}`
//!   inside it: `${A:-{x}}` reads as `default = "{x"`, substitutes it,
//!   and leaves the second `}` to be copied through as ordinary text —
//!   so a default containing a closing brace cannot be written at all.
//!   Deliberate: fixing it needs either a seventh error message or new
//!   escape syntax, and this grammar's contract is exactly six.
//! - `NAME` matches `^[A-Za-z_][A-Za-z0-9_]*$`, the same charset `.env`
//!   names must match (see `vars.rs`), so a name that can be assigned can
//!   always be referenced and vice versa.
//! - `$$` is a literal `$`, and only in that pairing — a lone `$` not
//!   immediately followed by `{` or another `$` is an error rather than
//!   passed through, so a typo like `$HOME` cannot silently become the
//!   literal text `$HOME` in a generated artifact.
//! - The result of a substitution is never rescanned: a value that is
//!   itself the literal text `${OTHER}` is emitted as-is.
//! - Every other byte is copied through unchanged.
//!
//! # `would_substitution_change` deliberately over-approximates
//!
//! [`would_substitution_change`] answers "does this string contain any
//! `$` at all", not "does it contain a reference that will actually
//! substitute to something else". A string whose only `$` is half of a
//! `$$` escape still returns `true`, even though the *number* of `$`
//! characters changes but nothing "resolves" in the reference sense. A
//! later cross-backend check uses this to decide whether a value is safe
//! to hand to a parser *before* substitution has run, or must wait until
//! after. Tightening this to something like "contains `${`" would let
//! that check hand pre-substitution text (e.g. `a$$b`, which becomes
//! `a$b`) to a parser that will never see the text as it actually reads
//! post-substitution. Deferring a value that didn't strictly need
//! deferring costs one check running slightly later; parsing text that
//! substitution is about to change parses the wrong string outright. Do
//! not narrow this function to make that later check fire more often.

use super::raw::{RawManifest, RawSpec};
use super::user::{AccountId, RawUser};
use super::vars::Vars;
use crate::error::{Error, Result};

/// Substitute one scalar. `path` is the manifest field path used in
/// errors, e.g. `daemons.frpc.env["LOG"]`. See the module doc comment for
/// the grammar and for why this must run before `resolve_one`.
pub(crate) fn scalar(input: &str, path: &str, vars: &Vars) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }

        match chars.peek().map(|&(_, c)| c) {
            Some('{') => {
                chars.next();
                let value = scan_braced(&mut chars, path, vars)?;
                out.push_str(&value);
            }
            Some('$') => {
                chars.next();
                out.push('$');
            }
            _ => {
                return Err(interpolate_error(
                    path,
                    format!("`$` at byte {i} is not part of `${{...}}`; write `$$` for a literal `$`"),
                ));
            }
        }
    }

    Ok(out)
}

/// Whether substitution would change `s` — that is, whether `s` contains
/// any `$` at all. See the module doc comment: this is deliberately an
/// over-approximation and must stay one.
pub(crate) fn would_substitution_change(s: &str) -> bool {
    s.contains('$')
}

/// Scan the body of a `${...}` construct, with `${` already consumed.
/// `chars` is positioned just after the `{`.
fn scan_braced(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, path: &str, vars: &Vars) -> Result<String> {
    let mut name = String::new();

    loop {
        let Some((_, c)) = chars.next() else {
            return Err(interpolate_error(path, "unterminated `${`: no closing `}`".to_string()));
        };

        if name.is_empty() {
            if c == '}' {
                return Err(interpolate_error(path, "empty variable name in `${}`".to_string()));
            }
            // `:` here is legal syntax opening a `:-` default (`${:-fallback}`),
            // not an invalid character — the real defect is the empty name
            // that precedes it, so report that, not the `:`.
            if c == ':' && chars.peek().map(|&(_, c)| c) == Some('-') {
                return Err(interpolate_error(path, "empty variable name in `${}`".to_string()));
            }
            if c.is_ascii_alphabetic() || c == '_' {
                name.push(c);
                continue;
            }
            return Err(invalid_name_char(path, c));
        }

        if c == '}' {
            return resolve_name(path, vars, &name, None);
        }
        if c == ':' {
            match chars.peek().map(|&(_, c)| c) {
                Some('-') => {
                    chars.next();
                    let default = scan_default(chars, path)?;
                    return resolve_name(path, vars, &name, Some(default));
                }
                _ => return Err(invalid_name_char(path, c)),
            }
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
            continue;
        }
        return Err(invalid_name_char(path, c));
    }
}

/// Scan a `:-default` body, with `:-` already consumed. Returns the
/// default text with the closing `}` consumed.
fn scan_default(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, path: &str) -> Result<String> {
    let mut default = String::new();

    loop {
        let Some((_, c)) = chars.next() else {
            return Err(interpolate_error(path, "unterminated `${`: no closing `}`".to_string()));
        };

        match c {
            '}' => return Ok(default),
            '$' => {
                return Err(interpolate_error(
                    path,
                    "`$` is not allowed inside a `:-` default".to_string(),
                ));
            }
            other => default.push(other),
        }
    }
}

/// Resolve a fully-scanned `name` (with an optional `default`) against
/// `vars`.
fn resolve_name(path: &str, vars: &Vars, name: &str, default: Option<String>) -> Result<String> {
    match (vars.get(name), default) {
        (Some(value), _) if !value.is_empty() => Ok(value.to_string()),
        (_, Some(default)) => Ok(default),
        (Some(value), None) => Ok(value.to_string()),
        (None, None) => Err(interpolate_error(
            path,
            format!(
                "no value for `{name}`; define it in the `.env` file beside the manifest, or write `${{{name}:-default}}`"
            ),
        )),
    }
}

fn invalid_name_char(path: &str, c: char) -> Error {
    interpolate_error(
        path,
        format!("invalid character `{c}` in a variable name; names match [A-Za-z_][A-Za-z0-9_]*"),
    )
}

fn interpolate_error(path: &str, message: String) -> Error {
    Error::Interpolate {
        path: path.to_string(),
        message,
    }
}

// The manifest walk ===================================================================================================

/// The `user.id` rejection, byte-exact: a test pins it.
const USER_ID_MESSAGE: &str = "`user.id` cannot be interpolated: a substituted value is always a string, so it \
                               would be read as a Windows SID; write the uid literally, or use `user: <name>`";

/// Substitute every `String` leaf of `raw` in place.
pub(crate) fn manifest(raw: &mut RawManifest, vars: &Vars) -> Result<()> {
    let RawManifest { daemons } = raw;

    for (id, entry) in daemons.iter_mut() {
        let path = format!("daemons.{id}");
        if would_substitution_change(id) {
            return Err(interpolate_error(
                &path,
                "a daemon id cannot be interpolated: ids are map keys, so an interpolated one could collide with \
                 another daemon or change which installed service an artifact belongs to; write the id literally"
                    .to_string(),
            ));
        }
        spec(entry, &path, vars)?;
    }

    Ok(())
}

/// Substitute every `String` leaf of one daemon entry in place. `path` is
/// that entry's manifest path (`daemons.<id>`), prefixed onto every error.
pub(crate) fn spec(raw: &mut RawSpec, path: &str, vars: &Vars) -> Result<()> {
    // Destructured rather than field-accessed on purpose: a field added to
    // `RawSpec` stops compiling here until someone decides whether it
    // interpolates. A walk that reads fields by name would silently skip
    // the new one, and a silently skipped field is a literal `${VAR}` in a
    // generated service artifact.
    let RawSpec {
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
        // Task 5 turns this into `debug_assert!(backend_specific.is_empty(), …)`
        // once `merged_for` is the only thing that feeds `spec`/`spec_would_change`.
        // Writing that assert now would panic in every debug build until
        // then: `resolve` still calls this on the *unmerged* manifest, and
        // a manifest carrying a `backend-specific:` block is exactly what
        // this task adds the ability to write.
        backend_specific: _,
    } = raw;

    substitute_option(name, &format!("{path}.name"), vars)?;
    for (index, arg) in command.iter_mut().flatten().enumerate() {
        *arg = scalar(arg, &format!("{path}.command[{index}]"), vars)?;
    }
    substitute_option(cwd, &format!("{path}.cwd"), vars)?;
    substitute_option(logs, &format!("{path}.logs"), vars)?;

    for (key, value) in env.iter_mut() {
        let key_path = format!("{path}.env[{key:?}]");
        if would_substitution_change(key) {
            return Err(interpolate_error(
                &key_path,
                "an `env` name cannot be interpolated: names are map keys, so an interpolated one could silently \
                 overwrite another entry; write the name literally"
                    .to_string(),
            ));
        }
        *value = scalar(value, &key_path, vars)?;
    }

    substitute_user(user, path, vars)?;

    substitute_option(restart, &format!("{path}.restart"), vars)?;
    substitute_option(restart_delay, &format!("{path}.restart-delay"), vars)?;
    substitute_option(kind, &format!("{path}.type"), vars)?;

    Ok(())
}

/// Whether any substitutable leaf of `raw` contains a `$`. Deliberately an
/// over-approximation of "contains a reference", inherited from
/// [`would_substitution_change`] — see the module doc comment.
///
/// A `$` in a daemon id, an `env` name, or a `user.id` does *not* count: it
/// is an error [`manifest`] reports by itself, with a message naming the
/// reason, and answering `true` here would make `resolve` read `.env` first
/// and possibly replace that message with an unrelated IO failure.
pub(crate) fn manifest_would_change(raw: &RawManifest) -> bool {
    let RawManifest { daemons } = raw;
    daemons.values().any(spec_would_change)
}

fn spec_would_change(raw: &RawSpec) -> bool {
    // Exhaustive for the same reason `spec` is: a new field must be
    // considered here too, or `resolve` decides whether to read `.env` from
    // a stale view of the manifest.
    let RawSpec {
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
        // See the matching arm in `spec` above for why this is `_` here.
        backend_specific: _,
    } = raw;

    [name, cwd, logs, restart, restart_delay, kind]
        .iter()
        .any(|field| field.as_deref().is_some_and(would_substitution_change))
        || command.iter().flatten().any(|arg| would_substitution_change(arg))
        || env.values().any(|value| would_substitution_change(value))
        || user_would_change(user)
}

/// Substitute the authored `user:` text, whichever form it was written
/// in. No reserved-word mapping happens here: `resolve::resolve_user`
/// applies the `root` rule to the substituted text exactly as it does to
/// authored text, so `user: ${U}` with `U=root` means what `user: root`
/// means, and `{name: ${U}}` means what `{name: root}` means — a literal
/// account named `root`, which is that form's entire purpose.
fn substitute_user(user: &mut Option<RawUser>, path: &str, vars: &Vars) -> Result<()> {
    match user {
        // A `Uid` is a number the YAML parser already produced — nothing a
        // string could be substituted into.
        None | Some(RawUser::Id(AccountId::Uid(_))) => Ok(()),
        Some(RawUser::Id(AccountId::Sid(sid))) => {
            if would_substitution_change(sid) {
                return Err(interpolate_error(
                    &format!("{path}.user.id"),
                    USER_ID_MESSAGE.to_string(),
                ));
            }
            Ok(())
        }
        Some(RawUser::Scalar(text)) => {
            *text = scalar(text, &format!("{path}.user"), vars)?;
            Ok(())
        }
        Some(RawUser::Name(name)) => {
            *name = scalar(name, &format!("{path}.user.name"), vars)?;
            Ok(())
        }
    }
}

fn user_would_change(user: &Option<RawUser>) -> bool {
    match user {
        Some(RawUser::Scalar(text)) => would_substitution_change(text),
        Some(RawUser::Name(name)) => would_substitution_change(name),
        None | Some(RawUser::Id(_)) => false,
    }
}

/// Substitute an optional field in place; `None` has nothing to substitute.
fn substitute_option(field: &mut Option<String>, path: &str, vars: &Vars) -> Result<()> {
    if let Some(value) = field {
        *value = scalar(value, path, vars)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "interpolate_tests.rs"]
mod interpolate_tests;
