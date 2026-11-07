//! Strict, non-shell `${VAR}` substitution for a parsed manifest.
//!
//! [`scalar`] substitutes one string against `.env`; [`check_grammar`]
//! checks the same grammar without resolving anything. [`spec`] and
//! [`check_spec_grammar`] walk one [`RawSpec`]'s string leaves, calling one
//! or the other on each. `resolve` (`resolve.rs`) is what calls all four;
//! see "This must run before `resolve_shape`, never after" below.
//!
//! # This must run before `resolve_shape`, never after
//!
//! Substitution has to happen on the typed raw manifest, strictly before
//! `resolve_shape` runs, never on a resolved `DaemonSpec`: `resolve_shape`
//! routes every user-supplied string through `reject_control_chars`, the only gate between a
//! string and a generated artifact. Substituting after it would let a `.env` value
//! carry a newline straight into a systemd unit or launchd plist. No field is
//! exempt, and the call order is all that holds this up.
//!
//! # Why hand-written rather than a crate
//!
//! `subst` and `shellexpand` both accept a bare `$VAR` as a reference, and
//! neither names the offending variable when a reference cannot be resolved.
//! This grammar rejects a bare `$` not followed by `{` or `$`, so a user who
//! typed `$HOME` gets an error naming the byte, not a literal `$HOME` in a
//! unit file.
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
//! - `default` ends at the first `}`, with no escape for a literal `}`:
//!   `${A:-{x}}` reads as `default = "{x"` followed by ordinary text `}`, so a
//!   default containing a closing brace cannot be written. Deliberate: fixing
//!   it needs a seventh error message or new escape syntax, and this
//!   grammar's contract is exactly six.
//! - `NAME` matches `^[A-Za-z_][A-Za-z0-9_]*$`, the same charset `.env`
//!   names must match (see `vars.rs`), so a name that can be assigned can
//!   always be referenced and vice versa.
//! - `$$` is a literal `$`. A lone `$` not followed by `{` or `$` is an
//!   error, never passed through.
//! - The result of a substitution is never rescanned: a value that is
//!   itself the literal text `${OTHER}` is emitted as-is.
//! - Every other byte is copied through unchanged.
//!
//! # `would_substitution_change` deliberately over-approximates
//!
//! [`would_substitution_change`] answers "does this string contain any `$`
//! at all", so a string whose only `$` is half of a `$$` escape still
//! returns `true`. A cross-backend check uses it to decide whether a value
//! can be parsed *before* substitution or must wait. Narrowing it to
//! "contains `${`" would hand `a$$b` (which becomes `a$b`) to a parser
//! before substitution, parsing the wrong string; deferring unnecessarily
//! only runs a check slightly later. Do not narrow it.

use super::raw::RawSpec;
use super::user::{AccountId, RawUser};
use super::vars::Vars;
use crate::error::{Error, Result};

/// Which job [`scan`] is doing. Both entry points share one scanner so the
/// two grammars cannot drift; only [`resolve_name`] consults `vars`.
#[derive(Clone, Copy)]
enum Mode<'a> {
    /// Resolve each reference against `.env`.
    Substitute(&'a Vars),
    /// Check the grammar only. [`resolve_name`] returns `Ok(String::new())`
    /// without touching `vars`, so an undefined name is not an error.
    CheckOnly,
}

/// Substitute one scalar. `path` is the manifest field path used in
/// errors, e.g. `env["LOG"]`. See the module doc for the grammar and for why
/// this must run before `resolve_shape`.
pub(crate) fn scalar(input: &str, path: &str, vars: &Vars) -> Result<String> {
    scan(input, path, Mode::Substitute(vars))
}

/// Every `${...}` reference in `input` is well-formed. Grammar only: an undefined name is *not*
/// an error here (see [`Mode::CheckOnly`]), unlike `scalar(input, path, &Vars::empty())`, which
/// would demand a variable on a host that may not be the one substitution runs on. See
/// `resolve.rs`'s "Why the grammar check exists".
pub(crate) fn check_grammar(input: &str, path: &str) -> Result<()> {
    scan(input, path, Mode::CheckOnly).map(drop)
}

fn scan(input: &str, path: &str, mode: Mode<'_>) -> Result<String> {
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
                let value = scan_braced(&mut chars, path, mode)?;
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

/// Whether `s` contains any `$` at all. Deliberately an over-approximation of "substitution
/// would change `s`"; see the module doc.
pub(crate) fn would_substitution_change(s: &str) -> bool {
    s.contains('$')
}

/// Scan the body of a `${...}` construct, with `${` already consumed.
/// `chars` is positioned just after the `{`.
fn scan_braced(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    path: &str,
    mode: Mode<'_>,
) -> Result<String> {
    let mut name = String::new();

    loop {
        let Some((_, c)) = chars.next() else {
            return Err(interpolate_error(path, "unterminated `${`: no closing `}`".to_string()));
        };

        if name.is_empty() {
            if c == '}' {
                return Err(interpolate_error(path, "empty variable name in `${}`".to_string()));
            }
            // `${:-x}`: the defect is the empty name, not the `:`.
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
            return resolve_name(path, mode, &name, None);
        }
        if c == ':' {
            match chars.peek().map(|&(_, c)| c) {
                Some('-') => {
                    chars.next();
                    let default = scan_default(chars, path)?;
                    return resolve_name(path, mode, &name, Some(default));
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
/// `mode`.
fn resolve_name(path: &str, mode: Mode<'_>, name: &str, default: Option<String>) -> Result<String> {
    let vars = match mode {
        Mode::CheckOnly => return Ok(String::new()),
        Mode::Substitute(vars) => vars,
    };
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

// The per-field walk ==================================================================================================

/// The `user.id` rejection, byte-exact: a test pins it. `pub(super)` so `resolve_shape` raises
/// the identical message for a non-native backend.
pub(super) const USER_ID_MESSAGE: &str = "`user.id` cannot be interpolated: a substituted value is always a string, so it \
                               would be read as a Windows SID; write the uid literally, or use `user: <name>`";

/// The `env` name rejection, byte-exact; `pub(super)` for the same reason as [`USER_ID_MESSAGE`].
pub(super) const ENV_NAME_MESSAGE: &str = "an `env` name cannot be interpolated: names are map keys, so an \
                                            interpolated one could silently overwrite another entry; write the \
                                            name literally";

/// The daemon-id rejection, byte-exact.
const DAEMON_ID_MESSAGE: &str = "a daemon id cannot be interpolated: it names the installed service, so an \
                                  interpolated one could change which service this manifest installs; write the \
                                  id literally";

/// The no-interpolation rule for the daemon id. `id` is the raw `id:` text; this must run
/// *before* the [`super::is_valid_id`] check, whose charset excludes `$` and would answer first
/// with a more generic message. The id is never overridable, so unlike the `env`-name and
/// `user.id` rules there is no per-backend echo.
pub(crate) fn reject_interpolated_id(id: &str) -> Result<()> {
    if would_substitution_change(id) {
        return Err(interpolate_error("id", DAEMON_ID_MESSAGE.to_string()));
    }
    Ok(())
}

/// Substitute every `String` leaf of the manifest in place. Runs only on a
/// spec carrying no overrides: `resolve` calls it on the native backend's
/// `merged_for` result, which becomes the returned spec.
pub(super) fn spec(raw: &mut RawSpec, vars: &Vars) -> Result<()> {
    // Destructured so a field added to `RawSpec` stops compiling here until someone decides
    // whether it interpolates; a skipped field is a literal `${VAR}` in a generated artifact.
    let RawSpec {
        // Checked by `reject_interpolated_id` before this ever runs.
        id: _,
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
        backend_specific,
    } = raw;
    debug_assert!(backend_specific.is_empty(), "interpolation runs on merged specs only");

    substitute_option(name, "name", vars)?;
    for (index, arg) in command.iter_mut().flatten().enumerate() {
        *arg = scalar(arg, &format!("command[{index}]"), vars)?;
    }
    substitute_option(cwd, "cwd", vars)?;
    substitute_option(logs, "logs", vars)?;

    for (key, value) in env.iter_mut() {
        let key_path = format!("env[{key:?}]");
        if would_substitution_change(key) {
            return Err(interpolate_error(&key_path, ENV_NAME_MESSAGE.to_string()));
        }
        *value = scalar(value, &key_path, vars)?;
    }

    substitute_user(user, vars)?;

    substitute_option(restart, "restart", vars)?;
    substitute_option(restart_delay, "restart-delay", vars)?;
    substitute_option(kind, "type", vars)?;

    Ok(())
}

/// Every substitutable `String` leaf of `raw` passes [`check_grammar`].
/// Same walk and field paths as [`spec`], with no [`Vars`], so it can run before any `.env` is
/// loaded. The daemon id, `env` names and `user.id` need no check here: a `$` in them is
/// rejected outright (`reject_interpolated_id`, `resolve_shape`).
pub(super) fn check_spec_grammar(raw: &RawSpec) -> Result<()> {
    let RawSpec {
        id: _,
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
        backend_specific,
    } = raw;
    debug_assert!(
        backend_specific.is_empty(),
        "check_spec_grammar runs on merged specs only"
    );

    if let Some(name) = name {
        check_grammar(name, "name")?;
    }
    for (index, arg) in command.iter().flatten().enumerate() {
        check_grammar(arg, &format!("command[{index}]"))?;
    }
    if let Some(cwd) = cwd {
        check_grammar(cwd, "cwd")?;
    }
    if let Some(logs) = logs {
        check_grammar(logs, "logs")?;
    }
    for (key, value) in env {
        check_grammar(value, &format!("env[{key:?}]"))?;
    }
    check_user_grammar(user)?;
    if let Some(restart) = restart {
        check_grammar(restart, "restart")?;
    }
    if let Some(restart_delay) = restart_delay {
        check_grammar(restart_delay, "restart-delay")?;
    }
    if let Some(kind) = kind {
        check_grammar(kind, "type")?;
    }
    Ok(())
}

fn check_user_grammar(user: &Option<RawUser>) -> Result<()> {
    match user {
        // `Id`: a `Uid` is a number, and a `$` in a `Sid` is rejected by `resolve_shape` first.
        None | Some(RawUser::Id(_)) => Ok(()),
        Some(RawUser::Scalar(text)) => check_grammar(text, "user"),
        Some(RawUser::Name(name)) => check_grammar(name, "user.name"),
    }
}

/// Whether any substitutable leaf of `raw` contains a `$`; an over-approximation inherited from
/// [`would_substitution_change`].
///
/// A `$` in an `env` name or a `user.id` does *not* count: [`spec`] (or `resolve_shape`) reports
/// it with its own message, and answering `true` would make `resolve` read `.env` first and
/// possibly replace that message with an unrelated IO failure. The daemon id is checked earlier
/// (`resolve`'s step 1).
pub(super) fn spec_would_change(raw: &RawSpec) -> bool {
    // Exhaustive for the same reason as `spec`.
    let RawSpec {
        id: _,
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
        backend_specific: _,
    } = raw;

    [name, cwd, logs, restart, restart_delay, kind]
        .iter()
        .any(|field| field.as_deref().is_some_and(would_substitution_change))
        || command.iter().flatten().any(|arg| would_substitution_change(arg))
        || env.values().any(|value| would_substitution_change(value))
        || user_would_change(user)
}

/// Substitute the authored `user:` text, whichever form it was written in. The `root` rule is
/// left to `resolve::resolve_user`, which applies it to substituted text as to typed text: `user:
/// ${U}` with `U=root` means `user: root`, and `{name: ${U}}` means `{name: root}`.
fn substitute_user(user: &mut Option<RawUser>, vars: &Vars) -> Result<()> {
    match user {
        // A `Uid` is already a number.
        None | Some(RawUser::Id(AccountId::Uid(_))) => Ok(()),
        Some(RawUser::Id(AccountId::Sid(sid))) => {
            if would_substitution_change(sid) {
                return Err(interpolate_error("user.id", USER_ID_MESSAGE.to_string()));
            }
            Ok(())
        }
        Some(RawUser::Scalar(text)) => {
            *text = scalar(text, "user", vars)?;
            Ok(())
        }
        Some(RawUser::Name(name)) => {
            *name = scalar(name, "user.name", vars)?;
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
