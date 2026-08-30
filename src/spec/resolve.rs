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
    reject_control_chars(&id, "name", &name)?;

    if raw.command.is_empty() {
        return Err(invalid(&id, "command must not be empty"));
    }
    let mut command = raw.command;
    for arg in &command {
        reject_control_chars(&id, "command", arg)?;
    }
    if let Some(first) = command.first_mut() {
        *first = resolve_path_string(first, base_dir);
    }

    let cwd = resolve_optional_path(&id, "cwd", raw.cwd, base_dir)?;
    let logs = resolve_optional_path(&id, "logs", raw.logs, base_dir)?;

    let mut env = BTreeMap::new();
    for (key, value) in raw.env {
        if key.contains('=') {
            return Err(invalid(&id, &format!("env key `{key}` must not contain `=`")));
        }
        reject_control_chars(&id, "env key", &key)?;
        reject_control_chars(&id, &format!("env[{key}]"), &value)?;
        env.insert(key, value);
    }

    let user = raw.user.unwrap_or(User::Root);
    match &user {
        User::Root => {}
        User::Name(n) => reject_control_chars(&id, "user.name", n)?,
        User::Id(AccountId::Sid(s)) => reject_control_chars(&id, "user.id", s)?,
        User::Id(AccountId::Uid(_)) => {}
    }

    let restart = raw.restart.unwrap_or(Restart::Never);
    warn_on_sub_second_restart_delay(&id, raw.restart_delay, warnings);

    let kind = raw.kind.unwrap_or(Kind::Simple);
    warn_on_windows_divergences(&id, kind, cwd.is_some(), logs.is_some(), restart, warnings);

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
            reject_control_chars(id, field, &s)?;
            Ok(Some(PathBuf::from(resolve_path_string(&s, base_dir))))
        }
        None => Ok(None),
    }
}

/// Join a relative path against `base_dir`; leave an absolute path as-is.
/// `Path::is_absolute` (not a string prefix check) so this behaves
/// correctly on both a POSIX host (`/opt/rt`) and a Windows host
/// (`C:\base`), whose absoluteness rules differ.
fn resolve_path_string(raw: &str, base_dir: &Path) -> String {
    let p = Path::new(raw);
    if p.is_absolute() {
        raw.to_string()
    } else {
        base_dir.join(p).to_string_lossy().into_owned()
    }
}

/// systemd's `Description=`/`Environment=`, launchd's `UserName`, and every
/// other emitted directive have no escaping, and a literal control
/// character — most importantly a newline — can terminate one directive
/// and start another. XML 1.0 cannot represent a control character at all,
/// not even as an entity, so this same check also keeps the launchd
/// generator from emitting an unparseable plist.
fn reject_control_chars(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    if value.chars().any(|c| c.is_control()) {
        return Err(invalid(
            id,
            &format!("field `{field}` contains a control character, which is forbidden"),
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
    let rounded = delay.as_secs() + 1;
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
