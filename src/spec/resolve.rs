//! `RawSpec -> DaemonSpec`: defaults, absolute-path resolution, and the
//! injection validation gate.
//!
//! This is the crate's parse-don't-validate boundary — there is no
//! separate `validate()` anywhere. systemd's `Description=` and
//! `Environment=` have no escaping and a literal newline terminates a
//! directive, so an unvalidated `name` could inject a second `ExecStart=`
//! using systemd's own reset-then-reassign idiom; XML 1.0 separately
//! cannot represent control characters at all, not even as entities. Every
//! user-supplied string that ends up in a generated artifact — `name`,
//! `command`, `cwd`, `logs`, every `env` key and value, and `user`'s
//! `name`/`id` — is rejected here if it contains one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::raw::RawManifest;
use super::user::{AccountId, User};
use super::{DaemonSpec, Id, Kind, RawSpec, Restart, Warning};
use crate::error::Error;

const MANIFEST_FILE_NAME: &str = "goetia.yaml";

/// Turn a parsed manifest into resolved daemon specs.
///
/// Relative `command[0]`, `cwd`, and `logs` paths are resolved against
/// `base_dir` and written back absolute. Fails on the first invalid
/// daemon; a valid manifest may still produce `Warning`s for properties
/// that are accepted but cannot be faithfully honored on every platform.
pub fn resolve(raw: RawManifest, base_dir: &Path) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    // `DaemonSpec` documents every path as absolute and `blob::decode`
    // enforces it, so a relative `base_dir` would produce an artifact whose
    // own embedded blob cannot be decoded — breaking the drift invariant for
    // the entirely ordinary `goetia daemon install -f .`. Anchor it here, at
    // the one place that makes the guarantee, rather than trusting every
    // caller to pass an absolute path.
    let base_dir = absolutize(base_dir)?;
    let base_dir = base_dir.as_path();

    let mut specs = Vec::with_capacity(raw.daemons.len());
    let mut warnings = Vec::new();

    for (key, entry) in raw.daemons {
        let id = Id::try_from(key)?;
        let spec = resolve_one(id, entry, base_dir, &mut warnings)?;
        specs.push(spec);
    }

    Ok((specs, warnings))
}

/// Read and resolve a manifest from `path`. `path` may name the manifest
/// file itself, or a directory containing `goetia.yaml`.
pub fn load(path: &Path) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    let (file_path, base_dir) = if path.is_dir() {
        (path.join(MANIFEST_FILE_NAME), path.to_path_buf())
    } else {
        let base = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        (path.to_path_buf(), base)
    };

    let text = fs::read_to_string(&file_path).map_err(|source| Error::Io {
        path: file_path.clone(),
        source,
    })?;
    let raw: RawManifest = serde_yaml_ng::from_str(&text)?;
    resolve(raw, &base_dir)
}

fn resolve_one(id: Id, raw: RawSpec, base_dir: &Path, warnings: &mut Vec<Warning>) -> Result<DaemonSpec, Error> {
    let name = raw.name.unwrap_or_else(|| id.as_str().to_string());
    reject_unemittable(&id, "name", &name)?;

    reject_empty_command(&id, &raw.command)?;
    let mut command = raw.command;
    for arg in &command {
        reject_unemittable(&id, "command", arg)?;
    }
    if let Some(first) = command.first_mut() {
        *first = resolve_path_string(&id, "command", first, base_dir)?;
    }

    let cwd = resolve_optional_path(&id, "cwd", raw.cwd, base_dir)?;
    let logs = resolve_optional_path(&id, "logs", raw.logs, base_dir)?;

    let mut env = BTreeMap::new();
    for (key, value) in raw.env {
        reject_env_key_with_equals(&id, &key)?;
        reject_unemittable(&id, "env key", &key)?;
        reject_unemittable(&id, &format!("env[{key}]"), &value)?;
        env.insert(key, value);
    }

    let user = raw.user.unwrap_or(User::Root);
    match &user {
        User::Root => {}
        User::Name(n) => reject_unemittable(&id, "user.name", n)?,
        User::Id(AccountId::Sid(s)) => reject_unemittable(&id, "user.id", s)?,
        User::Id(AccountId::Uid(_)) => {}
    }

    let restart = raw.restart.unwrap_or(Restart::Never);
    warn_on_sub_second_restart_delay(&id, raw.restart_delay, warnings);

    let kind = raw.kind.unwrap_or(Kind::Simple);
    warn_on_windows_divergences(&id, kind, cwd.is_some(), logs.is_some(), restart, warnings);

    // The absoluteness guarantee has exactly one runtime enforcement point
    // today, inside `blob::decode` — which only fires when re-reading an
    // artifact that was already written. On the direct resolve -> generate
    // path that `install`/`show` take, a regression here would be baked into
    // a unit file before anything noticed. Catch it at the source instead.
    debug_assert!(
        Path::new(&command[0]).is_absolute()
            && cwd.as_deref().is_none_or(Path::is_absolute)
            && logs.as_deref().is_none_or(Path::is_absolute),
        "resolve must return absolute paths; got command[0]={:?} cwd={:?} logs={:?}",
        command[0],
        cwd,
        logs,
    );

    Ok(DaemonSpec {
        id,
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay: raw.restart_delay,
        logs,
        kind,
    })
}

fn resolve_optional_path(id: &Id, field: &str, raw: Option<String>, base_dir: &Path) -> Result<Option<PathBuf>, Error> {
    match raw {
        Some(s) => {
            reject_unemittable(id, field, &s)?;
            Ok(Some(PathBuf::from(resolve_path_string(id, field, &s, base_dir)?)))
        }
        None => Ok(None),
    }
}

/// Join a relative path against `base_dir`; leave an absolute path as-is.
/// `Path::is_absolute` (not a string prefix check) so this behaves
/// correctly on both a POSIX host (`/opt/rt`) and a Windows host
/// (`C:\base`), whose absoluteness rules differ.
fn resolve_path_string(id: &Id, field: &str, raw: &str, base_dir: &Path) -> Result<String, Error> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        normalize(&base_dir.join(p))
    };
    // Assert the post-condition rather than assume the join achieved it.
    // A Windows drive-relative path (`C:bin/frpc.exe`) is neither absolute
    // nor joinable: `PathBuf::push` truncates the buffer whenever the pushed
    // path carries a prefix, so `base_dir.join("C:bin")` silently discards
    // `base_dir` and yields `C:bin` — still relative. Resolving it for real
    // would mean reading the per-drive current directory, which Rust exposes
    // no portable way to do, and which would anchor the artifact to whatever
    // that happened to be in the *installing* shell. Reject it instead.
    if !joined.is_absolute() {
        return Err(invalid(
            id,
            &format!(
                "field `{field}` is `{raw}`, which cannot be resolved to an absolute path; drive-relative paths like `C:dir` are not supported - write a full path"
            ),
        ));
    }
    Ok(joined.to_string_lossy().into_owned())
}

/// Make `dir` absolute against the process's working directory.
///
/// Deliberately not `canonicalize()`: that requires the path to exist and
/// resolves symlinks, neither of which is wanted here. A manifest may name a
/// `cwd` or `logs` directory the installer is about to create, and resolving
/// a symlink would bake the target into the artifact rather than the path the
/// user wrote.
fn absolutize(dir: &Path) -> Result<PathBuf, Error> {
    if dir.is_absolute() {
        return Ok(normalize(dir));
    }
    // Not `Error::Io`: that variant means "reading `path` from disk failed",
    // and nothing was read from `dir` here. Reporting `failed to read
    // <manifest dir>` would send someone chasing permissions on a directory
    // that is fine, when the real fault is the process's own cwd being gone.
    let cwd = std::env::current_dir().map_err(|source| {
        Error::Other(format!(
            "failed to read the current directory while resolving `{}`: {source}",
            dir.display()
        ))
    })?;
    let joined = normalize(&cwd.join(dir));
    if !joined.is_absolute() {
        return Err(Error::Other(format!(
            "manifest directory `{}` cannot be resolved to an absolute path; drive-relative paths like `C:dir` are not supported — pass a full path to `-f`",
            dir.display()
        )));
    }
    Ok(joined)
}

/// Drop `.` components so a manifest loaded from `.` yields `<cwd>` rather
/// than `<cwd>/.`, and joined segments read as one consistent path. `..` is
/// left alone: removing it lexically is wrong in the presence of symlinks.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// systemd's `Description=`/`Environment=`, launchd's `UserName`, and every
/// other emitted directive have no escaping, and a literal control
/// character — most importantly a newline — can terminate one directive
/// and start another. XML 1.0 cannot represent a control character at all,
/// not even as an entity, so this same check also keeps the launchd
/// generator from emitting an unparseable plist.
///
/// `pub(crate)`: also called by `blob::decode`, which must re-run this
/// same check against a spec deserialized from an untrusted artifact
/// rather than trust that `DaemonSpec`'s `pub` fields still satisfy it.
/// Every gate a user-supplied string must pass before it can be emitted
/// into any artifact. One entry point rather than two, so a field added
/// later cannot pick up half the checks: each of these was a real defect
/// found in review, and both are silent when they fail.
///
/// `pub(crate)`: also called by `blob::decode`, which re-runs these against
/// a spec deserialized from an untrusted artifact rather than trusting that
/// `DaemonSpec`'s `pub` fields still satisfy them.
pub(crate) fn reject_unemittable(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    reject_control_chars(id, field, value)?;
    reject_trailing_backslash(id, field, value)?;
    Ok(())
}

pub(crate) fn reject_control_chars(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    if value.chars().any(|c| c.is_control()) {
        return Err(invalid(
            id,
            &format!("field `{field}` contains a control character, which is forbidden"),
        ));
    }
    if let Some(c) = value.chars().find(|c| is_xml_noncharacter(*c)) {
        // Not control characters, so `char::is_control()` misses them, but
        // XML 1.0 cannot represent a noncharacter at all — not even as a
        // numeric entity — so one reaching the launchd generator produces an
        // unparseable plist and a daemon that refuses to load.
        return Err(invalid(
            id,
            &format!(
                "field `{field}` contains U+{:04X}, a Unicode noncharacter that XML cannot represent",
                c as u32
            ),
        ));
    }
    Ok(())
}

/// The Unicode noncharacters: `U+FDD0..=U+FDEF`, and the last two code
/// points of every plane.
fn is_xml_noncharacter(c: char) -> bool {
    let n = c as u32;
    (0xFDD0..=0xFDEF).contains(&n) || (n & 0xFFFE) == 0xFFFE
}

/// A value ending in a backslash is a line continuation to systemd, which
/// merges its directive with the following line. That is not a formatting
/// nuisance but a privilege boundary: a `name` ending in `\` swallows
/// whatever comes next, which can be the `[Service]` section header or the
/// `User=` directive — and a swallowed `User=` silently runs the daemon as
/// root instead of the requested account. Interior backslashes are fine and
/// must stay allowed, or ordinary Windows paths become unexpressible.
///
/// `pub(crate)`: see `reject_control_chars`.
pub(crate) fn reject_trailing_backslash(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    let trailing = value.len() - value.trim_end_matches('\\').len();
    if trailing % 2 == 1 {
        return Err(invalid(
            id,
            &format!("field `{field}` ends in a backslash, which systemd reads as a line continuation"),
        ));
    }
    Ok(())
}

/// `command` must name at least one argv entry. `pub(crate)`: see
/// `reject_control_chars`.
pub(crate) fn reject_empty_command(id: &Id, command: &[String]) -> Result<(), Error> {
    if command.is_empty() {
        return Err(invalid(id, "command must not be empty"));
    }
    Ok(())
}

/// An env key containing `=` would make a `KEY=VALUE` env-file line or a
/// systemd `Environment=` directive ambiguous about where the key ends.
/// `pub(crate)`: see `reject_control_chars`.
pub(crate) fn reject_env_key_with_equals(id: &Id, key: &str) -> Result<(), Error> {
    if key.contains('=') {
        return Err(invalid(id, &format!("env key `{key}` must not contain `=`")));
    }
    Ok(())
}

/// `cwd` and `logs` are always resolved to absolute paths here (joined
/// against `base_dir` when relative), so a `DaemonSpec` field carrying a
/// relative path can only be an already-corrupt one. `pub(crate)`: see
/// `reject_control_chars`.
pub(crate) fn reject_relative_path(id: &Id, field: &str, path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(invalid(
            id,
            &format!("field `{field}` must be an absolute path, got `{}`", path.display()),
        ));
    }
    Ok(())
}

fn invalid(id: &Id, message: &str) -> Error {
    Error::Invalid {
        daemon: id.as_str().to_string(),
        message: message.to_string(),
    }
}

/// `restart-delay` is stored as authored so the metadata blob stays
/// deterministic across platforms, but launchd's `ThrottleInterval` is
/// integer seconds: `500ms` would truncate to `0`, which *disables*
/// throttling and yields an unbounded respawn storm. Warn here so the
/// rounding is not a silent surprise at install time.
fn warn_on_sub_second_restart_delay(id: &Id, restart_delay: Option<std::time::Duration>, warnings: &mut Vec<Warning>) {
    let Some(delay) = restart_delay else { return };
    if delay.subsec_nanos() == 0 {
        return;
    }
    let rounded = delay.as_secs().saturating_add(1);
    warnings.push(Warning {
        id: id.clone(),
        message: format!(
            "restart-delay {delay:?} is not a whole number of seconds; launchd's ThrottleInterval \
             will round it up to {rounded}s"
        ),
    });
}

/// `type: managed` on Windows has no working-directory or stdout-capture
/// field, and SCM recovery actions never fire after a clean exit — so
/// `cwd`/`logs` and `restart: always` are accepted, not rejected, but
/// silently unavailable there. See the design spec's accepted divergences
/// (a) and (b).
fn warn_on_windows_divergences(
    id: &Id,
    kind: Kind,
    has_cwd: bool,
    has_logs: bool,
    restart: Restart,
    warnings: &mut Vec<Warning>,
) {
    if kind != Kind::Managed {
        return;
    }
    if has_cwd || has_logs {
        warnings.push(Warning {
            id: id.clone(),
            message: "type: managed has no working-directory or stdout-capture field on Windows SCM; \
                      `cwd`/`logs` are silently unavailable there"
                .to_string(),
        });
    }
    if restart == Restart::Always {
        warnings.push(Warning {
            id: id.clone(),
            message: "restart: always is not faithfully expressible for type: managed on Windows: SCM \
                      recovery actions only fire on failure, never after a clean exit"
                .to_string(),
        });
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod resolve_tests;
