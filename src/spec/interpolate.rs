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
//! routes every user-supplied string through
//! `reject_unemittable`/`reject_control_chars`, the only gate between a string
//! and a generated artifact. Substituting after it would let a `.env` value
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

use std::collections::BTreeSet;

use super::field::Field;
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
    /// Check the grammar only. [`resolve_name`] returns empty text
    /// without touching `vars`, so an undefined name is not an error.
    CheckOnly,
}

/// A scanned string, and whether `.env` supplied any of it.
#[derive(Debug)]
pub(crate) struct Scanned {
    pub(crate) text: String,
    /// Some reference was resolved from `.env`. Not a default, and not a `$$`: those are the
    /// manifest's own text.
    pub(crate) from_env: bool,
}

/// Substitute one scalar. `path` is the manifest field path used in
/// errors, e.g. `env["LOG"]`. See the module doc for the grammar and for why
/// this must run before `resolve_shape`.
pub(crate) fn scalar(input: &str, path: &str, vars: &Vars) -> Result<Scanned> {
    scan(input, path, Mode::Substitute(vars))
}

/// Every `${...}` reference in `input` is well-formed. Grammar only: an undefined name is *not*
/// an error here (see [`Mode::CheckOnly`]), unlike `scalar(input, path, &Vars::empty())`, which
/// would demand a variable on a host that may not be the one substitution runs on. See
/// `resolve.rs`'s "Why the grammar check exists".
pub(crate) fn check_grammar(input: &str, path: &str) -> Result<()> {
    scan(input, path, Mode::CheckOnly).map(drop)
}

fn scan(input: &str, path: &str, mode: Mode<'_>) -> Result<Scanned> {
    let mut out = String::with_capacity(input.len());
    let mut from_env = false;
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
                out.push_str(&value.text);
                from_env |= value.from_env;
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

    Ok(Scanned { text: out, from_env })
}

/// Whether `s` contains any `$` at all. Deliberately an over-approximation of "substitution
/// would change `s`"; see the module doc.
pub(crate) fn would_substitution_change(s: &str) -> bool {
    s.contains('$')
}

/// Whether the text a shape-phase judgement is handed may still hold `${...}` references.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Substitution {
    /// As authored (the base pass and the all-backends sweep). A value with a `$` may become
    /// something else, so it is judged only after substitution.
    Pending,
    /// Substituted (the native pass). A `$` left is ordinary text (a `$$` escape, or one a `.env`
    /// value carried), so every value is judged as it stands.
    Done,
}

impl Substitution {
    /// Whether a judgement of `s` waits for a later pass: only while [`Pending`](Self::Pending),
    /// and only for a value [`would_substitution_change`] flags.
    pub(super) fn defers(self, s: &str) -> bool {
        self == Substitution::Pending && would_substitution_change(s)
    }
}

/// Scan the body of a `${...}` construct, with `${` already consumed.
/// `chars` is positioned just after the `{`.
fn scan_braced(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    path: &str,
    mode: Mode<'_>,
) -> Result<Scanned> {
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
fn resolve_name(path: &str, mode: Mode<'_>, name: &str, default: Option<String>) -> Result<Scanned> {
    let vars = match mode {
        Mode::CheckOnly => {
            return Ok(Scanned {
                text: String::new(),
                from_env: false,
            });
        }
        Mode::Substitute(vars) => vars,
    };
    let from_env = |text: &str| Scanned {
        text: text.to_string(),
        from_env: true,
    };
    match (vars.get(name), default) {
        (Some(value), _) if !value.is_empty() => Ok(from_env(value)),
        (_, Some(default)) => Ok(Scanned {
            text: default,
            from_env: false,
        }),
        (Some(value), None) => Ok(from_env(value)),
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
///
/// Returns the fields in which some reference was resolved from `.env` ([`Scanned::from_env`]),
/// so `resolve` can say whose text an error about one is.
pub(super) fn spec(raw: &mut RawSpec, vars: &Vars) -> Result<BTreeSet<Field>> {
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

    let mut from_env = BTreeSet::new();

    substitute_option(name, Field::Name, vars, &mut from_env)?;
    for (index, arg) in command.iter_mut().flatten().enumerate() {
        substitute(arg, Field::Command(index), vars, &mut from_env)?;
    }
    substitute_option(cwd, Field::Cwd, vars, &mut from_env)?;
    substitute_option(logs, Field::Logs, vars, &mut from_env)?;

    for (key, value) in env.iter_mut() {
        let field = Field::EnvValue(key.clone());
        if would_substitution_change(key) {
            return Err(interpolate_error(&field.path(), ENV_NAME_MESSAGE.to_string()));
        }
        substitute(value, field, vars, &mut from_env)?;
    }

    substitute_user(user, vars, &mut from_env)?;

    substitute_option(restart, Field::Restart, vars, &mut from_env)?;
    substitute_option(restart_delay, Field::RestartDelay, vars, &mut from_env)?;
    substitute_option(kind, Field::Kind, vars, &mut from_env)?;

    Ok(from_env)
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
fn substitute_user(user: &mut Option<RawUser>, vars: &Vars, from_env: &mut BTreeSet<Field>) -> Result<()> {
    match user {
        // A `Uid` is already a number.
        None | Some(RawUser::Id(AccountId::Uid(_))) => Ok(()),
        Some(RawUser::Id(AccountId::Sid(sid))) => {
            if would_substitution_change(sid) {
                return Err(interpolate_error("user.id", USER_ID_MESSAGE.to_string()));
            }
            Ok(())
        }
        Some(RawUser::Scalar(text)) => substitute(text, Field::User, vars, from_env),
        Some(RawUser::Name(name)) => substitute_at(name, Field::User, "user.name", vars, from_env),
    }
}

fn user_would_change(user: &Option<RawUser>) -> bool {
    match user {
        Some(RawUser::Scalar(text)) => would_substitution_change(text),
        Some(RawUser::Name(name)) => would_substitution_change(name),
        None | Some(RawUser::Id(_)) => false,
    }
}

/// Substitute `text` in place, noting `field` in `from_env` if `.env` supplied any of it. Errors
/// name [`Field::path`].
fn substitute(text: &mut String, field: Field, vars: &Vars, from_env: &mut BTreeSet<Field>) -> Result<()> {
    let path = field.path();
    substitute_at(text, field, &path, vars, from_env)
}

/// [`substitute`] with the error path spelled by the caller: `user.name`, which shares
/// [`Field::User`] with `user`.
fn substitute_at(
    text: &mut String,
    field: Field,
    path: &str,
    vars: &Vars,
    from_env: &mut BTreeSet<Field>,
) -> Result<()> {
    let scanned = scalar(text, path, vars)?;
    *text = scanned.text;
    if scanned.from_env {
        from_env.insert(field);
    }
    Ok(())
}

/// [`substitute`] for an optional field; `None` has nothing to substitute.
fn substitute_option(
    text: &mut Option<String>,
    field: Field,
    vars: &Vars,
    from_env: &mut BTreeSet<Field>,
) -> Result<()> {
    match text {
        Some(text) => substitute(text, field, vars, from_env),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "interpolate_tests.rs"]
mod interpolate_tests;
