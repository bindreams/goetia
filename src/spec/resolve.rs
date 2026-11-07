//! `RawSpec -> DaemonSpec`: defaults, absolute-path resolution, and the
//! injection validation gate.
//!
//! This is the crate's parse-don't-validate boundary — there is no
//! separate `validate()`. systemd's `Description=` and `Environment=` have
//! no escaping and a newline terminates a directive, so an unvalidated
//! `name` could inject a second `ExecStart=`; XML 1.0 cannot represent
//! most C0 control characters at all, not even as entities. Every user-supplied
//! string that ends up in a generated artifact — `name`, `command`, `cwd`,
//! `logs`, every `env` key and value, and `user`'s `name`/`id` — is
//! rejected here if it contains one, by the `check_*` rules that `DaemonSpec::new` also runs.
//!
//! **Why `command[0]` is absolutized here and `command[1..]` is not.**
//! No launcher resolves a path *inside* an argument, so a relative path in
//! `command[1..]` is the launched program's to resolve, against its working
//! directory. `command[0]` has a defined role, and systemd, launchd and
//! Windows services disagree about a relative value (systemd rejects
//! `bin/frpc`; launchd silently resolves it against the job's working
//! directory, the default one when `cwd` is unset; Windows against
//! `System32`). Absolutizing it against the manifest's directory before
//! any backend sees it gives one rule that does not depend on where the
//! daemon starts.
//!
//! **The dragon is in the default.** A relative path passed as an
//! *argument* (`command: ["bin/frpc", "-c", "host/frpc.toml"]`) with no
//! `cwd` resolves against the platform's default working directory; for a
//! systemd system instance that is `/`, so the daemon cannot find its
//! config and exits with no hint that the working directory was the problem.
//! Set `cwd`, or write the argument absolute.
//!
//! # Why the grammar check exists, and why it is not just `scalar` with an
//! empty `Vars`
//!
//! `interpolate::scalar` rejects a `$` outside `${...}` or `$$`, an
//! unterminated `${`, an empty or invalid name, and a `$` inside a `:-`
//! default. All of that is grammar over authored text, which is shape. But
//! `interpolate::spec` **never runs on a non-native override**, so without a
//! separate check `scm: {name: "$HOME"}` would resolve cleanly on Linux and
//! fail only at install on Windows, and a bad `$` in a base value that the
//! native override replaces would never be diagnosed. `scalar` with
//! `Vars::empty()` is not the check: it fails on `${VAR}` for a missing
//! variable, which a non-native backend is not entitled to decide.
//! `interpolate::check_grammar`/`check_spec_grammar` are the separate check.
//!
//! `check_spec_grammar` runs on authored text only: a `.env` value with a
//! literal `$` (`SECRET=abc$def`) is legal and would fail a grammar check
//! applied to substituted text.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::backend::{BackendKind, Shaped, ShapedSpec};
use super::interpolate;
use super::overrides::Supplied;
use super::raw::RawSpec;
use super::user::{AccountId, RawUser, User};
use super::vars::Vars;
use super::{DaemonFields, DaemonSpec, Id, Kind, Restart, Warning};
use crate::error::Error;

const MANIFEST_FILE_NAME: &str = "goetia.yaml";

/// Turn a parsed manifest into a resolved daemon spec.
///
/// Substitution happens inside `resolve`, on the merged native spec (step 5), so no `${VAR}`
/// survives into a returned spec; systemd would apply its *own* `${...}` expansion to
/// `ExecStart=` and turn an unsubstituted reference into an empty string in a privileged unit.
/// A non-native backend's override is validated as written (uninterpolated) and then discarded.
///
/// A host judges a non-native backend only by what the manifest wrote under
/// that backend's own `backend-specific:` key (steps 3-4), and warns only
/// about its native backend (step 5). Only the native backend's merged spec —
/// substituted, shaped, and completed — becomes the returned `DaemonSpec`.
/// Relative `command[0]`, `cwd`, and `logs` paths are resolved against
/// `base_dir` and written back absolute. A valid manifest may still produce
/// `Warning`s for properties that are accepted but cannot be faithfully
/// honored by the native backend.
fn resolve(raw: RawSpec, base_dir: &Path) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    resolve_as(raw, base_dir, BackendKind::native())
}

/// [`resolve`], with the native backend named rather than detected.
///
/// Production's only caller is [`resolve`], with [`BackendKind::native`]. Tests pass any of the
/// three, since the native arm of steps 4 and 5 is otherwise reachable only from one platform.
fn resolve_as(raw: RawSpec, base_dir: &Path, native: Option<BackendKind>) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    // Before `absolutize`, which can push a manifest-level advisory (a drive-relative `-f`).
    let mut warnings = Vec::new();

    // `blob::decode` requires absolute paths, so a relative `base_dir` (`goetia daemon install
    // -f .`) would produce an artifact whose own blob cannot be decoded. Anchor it here, the one
    // place that makes the guarantee.
    let base_dir = absolutize(base_dir, &mut warnings)?;
    let base_dir = base_dir.as_path();

    // Step 1: the id. Before `Id::try_from`, whose charset excludes `$` and would answer first
    // with a generic message.
    interpolate::reject_interpolated_id(&raw.id)?;
    let id = Id::try_from(raw.id.clone())?;

    // Step 2: the `.env` gate, computed from the merged native spec of step 5, the one
    // substitution runs on. Anything else either misses a `${VAR}` written only in the native
    // override, or lets an unrelated or unreadable `.env` break a host whose native backend has
    // no `$`.
    let (mut native_merged, native_supplied) = match native {
        Some(b) => raw.merged_for(b),
        None => (raw.without_overrides(), Supplied::NONE),
    };
    let vars = if interpolate::spec_would_change(&native_merged) {
        Vars::load(base_dir)?
    } else {
        Vars::empty()
    };

    // Step 3: the base pass. Reported verbatim with no backend attribution, so base errors win
    // on precedence. No `BackendKind` verdict: the base spec is nobody's override.
    let base = raw.without_overrides();
    interpolate::check_spec_grammar(&base)?;
    resolve_shape(&id, base, base_dir)?;

    // Step 4: the all-backends sweep, errors only: every backend's merged spec is checked as
    // authored, and `BackendKind::error` judges the fields its override supplied. A non-native
    // backend's advisories are not this host's to give, so its shape warnings are dropped. The
    // native arm runs the grammar and shape checks but no verdict (step 5 runs `error` on
    // substituted text). It is still needed: it makes step 5's failures provably substitution's
    // fault, and catches a *literal* control character in the native override.
    for backend in BackendKind::ALL {
        let (merged, supplied) = raw.merged_for(backend);
        let has_entry = raw.backend_specific.contains_key(&backend);
        let sweep: Result<(), Error> = (|| {
            interpolate::check_spec_grammar(&merged)?;
            let (shaped, _dropped_warnings) = resolve_shape(&id, merged, base_dir)?;
            if Some(backend) == native {
                return Ok(());
            }
            backend.error(&shaped, supplied)
        })();
        sweep.map_err(|e| annotate_sweep_error(e, backend, has_entry))?;
    }

    // Step 5: the native pass, the substitution whose result is kept. Authoritative: the
    // injection gate inspects final values here, and its `ShapedSpec` becomes the `DaemonSpec`.
    // No `check_spec_grammar`: a substituted `.env` value may legally carry a literal `$`.
    let native_entry = native.filter(|b| raw.backend_specific.contains_key(b));
    let substituted: Result<(ShapedSpec, Vec<Warning>), Error> = (|| {
        interpolate::spec(&mut native_merged, &vars)?;
        resolve_shape(&id, native_merged, base_dir)
    })();
    let (shaped, mut daemon_warnings) = substituted.map_err(|e| annotate_step5_error(e, native_entry))?;

    // `error` reads only `native_supplied` fields, so attribute to the backend, not `.env`.
    if let Some(b) = native {
        b.error(&shaped, native_supplied).map_err(|e| attribute_to(e, b))?;
        b.warn(&shaped, &mut daemon_warnings);
    }

    // Step 6: complete.
    let spec = require_complete(shaped, native)?;
    warnings.extend(daemon_warnings);

    Ok((spec, warnings))
}

/// Read and resolve a manifest from `path`. `path` may name the manifest
/// file itself, or a directory containing `goetia.yaml`.
pub fn load(path: &Path) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    let (file_path, base_dir) = if path.is_dir() {
        (path.join(MANIFEST_FILE_NAME), path.to_path_buf())
    } else {
        let base = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        (path.to_path_buf(), base)
    };

    let text = fs::read_to_string(&file_path).map_err(|source| Error::os_at("read", &file_path, source))?;
    // The only place a manifest is read, so every positioned diagnostic comes from the file as
    // written; `resolve` interpolates after, on the typed result.
    let raw = RawSpec::parse(&text)?;

    resolve(raw, &base_dir)
}

/// Backend annotation for step 4. `Error::Invalid` always gains the `backend-specific.<backend>: `
/// prefix: the base pass already validated the base form, so a failure here is attributable to
/// this backend's override. `Error::Interpolate` gets a path suffix instead, gated explicitly on
/// `has_entry`.
fn annotate_sweep_error(err: Error, backend: BackendKind, has_entry: bool) -> Error {
    match err {
        Error::Interpolate { path, message } if has_entry => Error::Interpolate {
            path: format!("{path} (backend-specific.{backend})"),
            message,
        },
        other => attribute_to(other, backend),
    }
}

/// `backend-specific.<backend>: ` on an `Error::Invalid`, and nothing on any other variant.
/// Shared by step 4's sweep and step 5's native verdict.
fn attribute_to(err: Error, backend: BackendKind) -> Error {
    match err {
        Error::Invalid {
            daemon,
            message,
            source,
        } => Error::Invalid {
            daemon,
            message: format!("backend-specific.{backend}: {message}"),
            source,
        },
        other => other,
    }
}

/// Backend annotation for step 5. `Error::Interpolate` gains a "merged with" path suffix, gated
/// on `native_entry` (was this backend mentioned at all), not on `Supplied` (which fields did it
/// write): `scm: {}` is an entry whose `Supplied` equals a missing entry's. `Error::Invalid`
/// gains an "after substituting from .env" prefix: step 4's native arm already passed
/// `resolve_shape` on the uninterpolated spec, so substitution is what changed.
///
/// Applied to the substitution and shape half of step 5 only; the native `BackendKind::error`
/// verdict is annotated by `attribute_to` at its call site, since step 4 never ran it.
fn annotate_step5_error(err: Error, native_entry: Option<BackendKind>) -> Error {
    match err {
        Error::Interpolate { path, message } => {
            let path = match native_entry {
                Some(b) => format!("{path} (merged with backend-specific.{b})"),
                None => path,
            };
            Error::Interpolate { path, message }
        }
        Error::Invalid {
            daemon,
            message,
            source,
        } => Error::Invalid {
            daemon,
            message: format!("after substituting from .env: {message}"),
            source,
        },
        other => other,
    }
}

/// Shape only: injection gate, path resolution, the three `$`-aware
/// parses. Requires no particular field to be present — completeness is
/// [`require_complete`]'s job.
fn resolve_shape(id: &Id, raw: RawSpec, base_dir: &Path) -> Result<(ShapedSpec, Vec<Warning>), Error> {
    debug_assert!(
        raw.backend_specific.is_empty(),
        "resolve_shape must be handed a merged spec, never one still carrying overrides"
    );

    let mut warnings = Vec::new();

    let name = raw.name.unwrap_or_else(|| id.as_str().to_string());
    let user = resolve_user(raw.user);

    // The field rules run on the authored values: the same function `DaemonSpec::new` runs on
    // the final ones. Every backend's merged spec passes through this, including the non-native
    // ones that never become a `DaemonSpec`: a newline in `scm`'s env must be caught from Linux
    // too. An absent or empty `command` is incomplete, not invalid: a native override may supply
    // it, and `require_complete` reports the miss with the backend named.
    //
    // Each path is resolved right after its own checks, so a resolution error falls in field
    // order, and the id is already valid when one names it.
    let mut command0 = None;
    let mut cwd = None;
    let mut logs = None;
    check_field_values(
        id,
        &FieldValues {
            name: &name,
            command: raw.command.as_deref(),
            cwd: raw.cwd.as_deref().map(Path::new),
            logs: raw.logs.as_deref().map(Path::new),
            env: &raw.env,
            user: &user,
        },
        Phase::Authored,
        |field| {
            match field {
                PathField::Command => {
                    if let Some(first) = raw.command.as_ref().and_then(|c| c.first()) {
                        command0 = Some(resolve_path_string(id, "command", first, base_dir, &mut warnings)?);
                    }
                }
                PathField::Cwd => {
                    cwd = resolve_optional_path(id, "cwd", raw.cwd.as_deref(), base_dir, &mut warnings)?;
                }
                PathField::Logs => {
                    logs = resolve_optional_path(id, "logs", raw.logs.as_deref(), base_dir, &mut warnings)?;
                }
            }
            Ok(())
        },
    )?;

    let mut command = raw.command;
    if let Some((first, resolved)) = command.as_mut().and_then(|c| c.first_mut()).zip(command0) {
        *first = resolved;
    }
    let env = raw.env;

    let restart = shape(id, raw.restart, parse_restart)?;
    let restart_delay = shape(id, raw.restart_delay, parse_restart_delay)?;
    let kind = shape(id, raw.kind, parse_kind)?;

    Ok((
        ShapedSpec {
            id: id.clone(),
            name,
            command,
            cwd,
            env,
            user,
            restart,
            restart_delay,
            logs,
            kind,
        },
        warnings,
    ))
}

/// Keep the parse in the shape phase, deferring exactly the values a substitution would change.
/// `would_substitution_change` is deliberately `contains('$')`, so a `$$` escape is deferred
/// too; see its doc.
fn shape<T>(id: &Id, raw: Option<String>, parse: fn(&Id, &str) -> Result<T, Error>) -> Result<Shaped<T>, Error> {
    match raw {
        None => Ok(Shaped::Absent),
        Some(text) if interpolate::would_substitution_change(&text) => Ok(Shaped::Deferred(text)),
        Some(text) => Ok(Shaped::Parsed(parse(id, &text)?)),
    }
}

/// Completeness: every field the installed spec must actually have, plus
/// the deferred parses and the defaults. Errors name `backend` — except a
/// deferred parse that fails here, which carries post-substitution text and
/// is not provably anyone's fault in particular, so it is reported
/// verbatim.
fn require_complete(shaped: ShapedSpec, backend: Option<BackendKind>) -> Result<DaemonSpec, Error> {
    let ShapedSpec {
        id,
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
    } = shaped;

    // Absent and empty share one check, so they cannot diverge in message.
    let command = command.unwrap_or_default();
    reject_empty_command(&id, &command).map_err(|_| missing_command_error(&id, backend))?;

    // `command[0]` is absolutized in `resolve_shape`, where a `command` first exists; the field
    // rules do not test it for absoluteness.
    debug_assert!(
        Path::new(&command[0]).is_absolute(),
        "resolve must return an absolute command[0]; got {:?}",
        command[0],
    );

    let restart = match restart {
        Shaped::Parsed(r) => r,
        Shaped::Deferred(text) => parse_restart(&id, &text)?,
        Shaped::Absent => Restart::Never,
    };
    let restart_delay = match restart_delay {
        Shaped::Parsed(d) => Some(d),
        Shaped::Deferred(text) => Some(parse_restart_delay(&id, &text)?),
        Shaped::Absent => None,
    };
    let kind = match kind {
        Shaped::Parsed(k) => k,
        Shaped::Deferred(text) => parse_kind(&id, &text)?,
        Shaped::Absent => Kind::Simple,
    };

    // `resolve_shape` ran the field rules on the authored values and on every path it resolved, so
    // this refuses nothing a manifest can cause; it is what makes the result a `DaemonSpec`.
    DaemonSpec::new(DaemonFields {
        id,
        name,
        command,
        cwd,
        env,
        user,
        restart,
        restart_delay,
        logs,
        kind,
    })
}

fn missing_command_error(id: &Id, backend: Option<BackendKind>) -> Error {
    match backend {
        Some(b) => invalid(
            id,
            &format!("no command is set for `{b}`: set the top-level `command`, or `backend-specific.{b}.command`"),
        ),
        None => invalid(id, "command must not be empty: set the top-level `command`"),
    }
}

/// Apply the `root` reserved word to an authored [`RawUser`], and default
/// an absent `user:` to [`User::Root`].
///
/// The rule is the bare string form's alone: `user: root` is the superuser, while `{name: root}`
/// is the literal account called `root`. Applying it here rather than in `RawUser`'s visitor
/// makes `user: ${U}` with `U=root` mean what a typed `user: root` means, without `{name: ${U}}`
/// also collapsing to the superuser; a `User::Name` records no syntax of origin.
pub(super) fn resolve_user(raw: Option<RawUser>) -> User {
    match raw {
        None => User::Root,
        Some(RawUser::Scalar(s)) if s == "root" => User::Root,
        Some(RawUser::Scalar(s)) => User::Name(s),
        Some(RawUser::Name(s)) => User::Name(s),
        Some(RawUser::Id(id)) => User::Id(id),
    }
}

/// Parse an authored `restart:` string into a [`Restart`]. `pub(crate)` because two-phase
/// resolution calls it from whichever phase can run it. Parsed from a `String`, not a derived
/// `Deserialize`, so `restart: ${R}` is interpolated first.
pub(crate) fn parse_restart(id: &Id, raw: &str) -> Result<Restart, Error> {
    match raw {
        "never" => Ok(Restart::Never),
        "on-failure" => Ok(Restart::OnFailure),
        "always" => Ok(Restart::Always),
        other => Err(invalid(
            id,
            &format!("field `restart` is `{other}`; expected one of `never`, `on-failure`, `always`"),
        )),
    }
}

/// Parse an authored `type:` string into a [`Kind`]. `pub(crate)`: see
/// [`parse_restart`].
pub(crate) fn parse_kind(id: &Id, raw: &str) -> Result<Kind, Error> {
    match raw {
        "simple" => Ok(Kind::Simple),
        "managed" => Ok(Kind::Managed),
        other => Err(invalid(
            id,
            &format!("field `type` is `{other}`; expected one of `simple`, `managed`"),
        )),
    }
}

/// Parse an authored `restart-delay:` string into a [`Duration`] with `humantime`.
/// `pub(crate)`: see [`parse_restart`].
pub(crate) fn parse_restart_delay(id: &Id, raw: &str) -> Result<Duration, Error> {
    humantime::parse_duration(raw).map_err(|source| {
        invalid_from(
            id,
            &format!("field `restart-delay` is `{raw}`, which is not a duration (e.g. `30s`, `1m 30s`)"),
            source,
        )
    })
}

fn resolve_optional_path(
    id: &Id,
    field: &str,
    raw: Option<&str>,
    base_dir: &Path,
    warnings: &mut Vec<Warning>,
) -> Result<Option<PathBuf>, Error> {
    raw.map(|s| resolve_path_string(id, field, s, base_dir, warnings).map(PathBuf::from))
        .transpose()
}

/// Join a relative path against `base_dir`; leave an absolute path as-is.
/// Uses `Path::is_absolute`, not a string prefix check, since POSIX and Windows differ.
///
/// `base_dir` is checked (see [`check_base_dir`]) only when a path is joined to it, so a manifest
/// directory that no field uses is never refused. The result is checked too ([`check_resolved`]).
///
/// Not a pure function of `(value, base_dir)`: `std::path::absolute`'s drive-relative fallback
/// reads the per-drive current directory. But every pass `resolve` makes runs in one process
/// against one unchanging process state, so identical values get identical verdicts, which makes
/// "the base pass passed, so this failure is the override's" sound.
fn resolve_path_string(
    id: &Id,
    field: &str,
    raw: &str,
    base_dir: &Path,
    warnings: &mut Vec<Warning>,
) -> Result<String, Error> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        // A path with its own prefix (`C:bin`) replaces `base_dir` in the join, so it is not used.
        if !matches!(p.components().next(), Some(std::path::Component::Prefix(_))) {
            check_base_dir(id, field, base_dir)?;
        }
        normalize(&base_dir.join(p))
    };
    // With an absolute `base_dir` a join stays relative only for a Windows drive-relative path
    // (`C:bin/frpc.exe`): `PathBuf::push` replaces the buffer when the pushed path carries a
    // prefix, so `base_dir.join("C:bin")` yields `C:bin`.
    let joined = if joined.is_absolute() {
        joined
    } else {
        // `std::path::absolute`, not `fs::canonicalize` (which needs the path to exist, resolves
        // symlinks and returns a `\\?\` path on Windows): resolves the way `GetFullPathNameW`
        // does, against the per-drive current directory of *this* process.
        let resolved = std::path::absolute(&joined).map_err(|source| {
            invalid_from(
                id,
                &format!("field `{field}` is `{raw}`, which cannot be resolved to an absolute path"),
                source,
            )
        })?;
        if resolved.is_absolute() {
            // Resolved against process state, so `install` and `diff` run from different
            // directories can disagree and report drift on an untouched artifact.
            warnings.push(Warning {
                id: Some(id.clone()),
                message: format!(
                    "field `{field}` is `{raw}`, a drive-relative path; it is anchored to the \
                     current directory on that drive in whichever process runs goetia, not to \
                     the manifest directory. Write a full path (e.g. `C:\\bin\\frpc.exe`) or a \
                     manifest-relative one (e.g. `.\\bin\\frpc.exe`) instead."
                ),
            });
        }
        resolved
    };
    debug_assert!(joined.is_absolute(), "`std::path::absolute` returned a relative path");
    check_resolved(id, field, raw, &joined)?;
    Ok(joined.to_string_lossy().into_owned())
}

/// The field rules for a resolved path. `base_dir` and `raw` have passed them, so only a
/// drive-relative path can fail here, anchored to a per-drive current directory that carries a
/// defect the authored text does not. The message says the field *resolved* to the bad value, since
/// the user did not write it.
fn check_resolved(id: &Id, field: &str, raw: &str, resolved: &Path) -> Result<(), Error> {
    let defect = match resolved.to_str() {
        None => Some("is not valid UTF-8".to_string()),
        Some(text) => unemittable_defect(text),
    };
    match defect {
        None => Ok(()),
        Some(defect) => Err(invalid(
            id,
            &format!(
                "field `{field}` is `{raw}`, which resolves to `{}`, which {defect}",
                resolved.display()
            ),
        )),
    }
}

/// Make `dir` absolute against the process's working directory.
///
/// **Deduced values may be canonicalised; authored ones are honoured as
/// written.** `dir` is the manifest directory the user passed to `-f`, so it
/// is not `canonicalize()`d: that requires the path to exist and resolves
/// symlinks, which would bake a symlinked deployment path's target into the
/// artifact, so a service would stop following a repointed symlink. Hence
/// `-f .` cannot preserve a symlinked spelling (`getcwd` already resolved
/// it); pass it explicitly to `-f`.
///
/// The process's own working directory, read below when `dir` is relative,
/// is the opposite case: nobody typed it and it always exists, so it *is*
/// canonicalised.
fn absolutize(dir: &Path, warnings: &mut Vec<Warning>) -> Result<PathBuf, Error> {
    if dir.is_absolute() {
        return Ok(normalize(dir));
    }
    // Not an `Error::Os` at `dir`: nothing was read from it, and naming it would misdirect a
    // reader when the process's own cwd is what is gone.
    let cwd = std::env::current_dir().map_err(|source| {
        Error::os(
            format!("read the current directory while resolving `{}`", dir.display()),
            source,
        )
    })?;
    let cwd = canonicalize_cwd(&cwd)?;
    let joined = normalize(&cwd.join(dir));
    // The fallback below resolves `..` lexically, on this path only: `normalize` leaves `..`
    // alone (wrong with symlinks), but `GetFullPathNameW` collapses it lexically, and goetia must
    // agree with other Windows tools.
    let joined = if joined.is_absolute() {
        joined
    } else {
        let resolved = std::path::absolute(&joined).map_err(|source| {
            Error::os(
                format!("resolve manifest directory `{}` to an absolute path", dir.display()),
                source,
            )
        })?;
        if resolved.is_absolute() {
            warnings.push(Warning {
                id: None,
                message: format!(
                    "manifest directory `-f {}` is a drive-relative path; it is anchored to the \
                     current directory on that drive in whichever process runs goetia, not to a \
                     fixed location. Pass a full path (e.g. `C:\\repo`) or a `.`-relative one to \
                     `-f` instead.",
                    dir.display()
                ),
            });
        }
        resolved
    };
    debug_assert!(joined.is_absolute(), "`std::path::absolute` returned a relative path");
    Ok(joined)
}

/// The manifest directory, for the field `field` that is joined to it, so the join cannot carry a
/// defect the field rules would blame on text the manifest wrote cleanly: it must be valid UTF-8,
/// hold no control character, and not end in a backslash. The error is attributed to `field`, so
/// it falls in field order.
///
/// The backslash rule is the field rule unchanged, which is an open owner question: it refuses a
/// Windows drive or UNC root (`C:\`, `\\srv\share\`), the only form `normalize` leaves ending
/// in one.
fn check_base_dir(id: &Id, field: &str, dir: &Path) -> Result<(), Error> {
    let defect = match dir.to_str() {
        None => Some("is not valid UTF-8".to_string()),
        Some(text) => unemittable_defect(text),
    };
    match defect {
        None => Ok(()),
        Some(defect) => Err(invalid(
            id,
            &format!("field `{field}`: the manifest directory {} {defect}", dir.display()),
        )),
    }
}

/// Canonicalise the process's current working directory; see [`absolutize`] for why this value,
/// unlike an authored one, may be.
///
/// On Unix this gains almost nothing. On Windows it gains canonical capitalisation, so
/// installing from `C:\App` and diffing from `C:\app` do not report phantom drift.
///
/// When this fails, every subcommand that loads a manifest by a relative `-f` (including the
/// default `-f .`) fails from that directory. That is an accepted limitation, decided by the
/// owner: the reachable causes are narrow, and a fallback to the uncanonicalised directory would
/// trade a loud refusal for silent phantom drift on Windows. Revisit if one of them shows up in use.
///
/// The failure causes (an unreadable path component on Unix; a refused open or a volume with no
/// DOS path on Windows) are the ones the message names. A
/// **deleted** working directory is not among them: it fails earlier, in
/// `std::env::current_dir`, so do not put it in the message below.
fn canonicalize_cwd(cwd: &Path) -> Result<PathBuf, Error> {
    let canonical = fs::canonicalize(cwd).map_err(|source| Error::Step {
        what: "the current directory could not be canonicalised; it may be unreadable, or on a volume with no drive \
               letter or mount point. Pass `-f` with an absolute path to skip resolving it"
            .to_string(),
        source: Box::new(Error::os_at("canonicalise", cwd, source)),
    })?;
    Ok(strip_verbatim_prefix(canonical))
}

/// Strip Windows' `\\?\` verbatim prefix from a `canonicalize`d path, so it
/// compares equal to every other spelling goetia or SCM produces for the
/// same directory. `canonicalize` only returns the two shapes matched below;
/// any other prefix is left untouched. Stripped unconditionally, even past
/// `MAX_PATH` (260): a `\\?\` string in the metadata blob would cause
/// permanent phantom drift, worse than the length limit that already applies
/// to every authored absolute path. A no-op on other platforms.
#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                let mut out = PathBuf::from(format!("{}:\\", letter as char));
                out.extend(path.components().skip(1));
                out
            }
            // Not `UNC\server\share`, which dropping the leading `\\?\` would produce.
            Prefix::VerbatimUNC(server, share) => {
                let mut out = PathBuf::from(format!("\\\\{}\\{}", server.to_string_lossy(), share.to_string_lossy()));
                out.extend(path.components().skip(1));
                out
            }
            _ => path,
        },
        _ => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
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

/// The field rules over a finished daemon: the one place they run for a value that becomes a
/// `DaemonSpec`, called by [`DaemonSpec::new`]. [`resolve_shape`] runs the same rules, through
/// [`check_field_values`], on the authored values.
pub(super) fn check_fields(fields: &DaemonFields) -> Result<(), Error> {
    check_field_values(
        &fields.id,
        &FieldValues {
            name: &fields.name,
            command: Some(&fields.command),
            cwd: fields.cwd.as_deref(),
            logs: fields.logs.as_deref(),
            env: &fields.env,
            user: &fields.user,
        },
        Phase::Final,
        |_| Ok(()),
    )
}

/// The values the field rules read, as the caller holds them: authored text, or a finished
/// daemon's fields. Paths are [`Path`]s so one function takes both.
struct FieldValues<'a> {
    name: &'a str,
    /// `None` only while authored: the manifest may leave `command` to a native override.
    command: Option<&'a [String]>,
    cwd: Option<&'a Path>,
    logs: Option<&'a Path>,
    env: &'a BTreeMap<String, String>,
    user: &'a User,
}

/// Which values [`check_field_values`] is reading. Three rules depend on it, each because the
/// other phase cannot hold the value it tests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// What the manifest wrote, or what `.env` substitution made of it (step 5 runs this phase on
    /// the substituted values), before any join or path resolution. Before substitution it may
    /// carry `${...}` references; either way it may hold a relative path or no `command`.
    Authored,
    /// A finished daemon's fields, after substitution and resolution: `command` is non-empty and
    /// the paths are absolute, and `$` is ordinary text.
    Final,
}

/// A field that [`resolve_shape`] resolves to an absolute path.
#[derive(Clone, Copy)]
enum PathField {
    Command,
    Cwd,
    Logs,
}

impl PathField {
    fn name(self) -> &'static str {
        match self {
            PathField::Command => "command",
            PathField::Cwd => "cwd",
            PathField::Logs => "logs",
        }
    }
}

/// Every field rule, in the order a manifest's defects are reported: `name`, `command`, `cwd`,
/// `logs`, each `env` entry, `user`. The one list; [`resolve_shape`] runs it on the authored
/// values and [`DaemonSpec::new`] on the final ones.
///
/// `resolve` is called once for each of `command`, `cwd` and `logs` after that field's own rules
/// pass (for `command`, only if it is present), and an error from it stops the walk. It lets the
/// caller resolve a path at its place in the order.
///
/// Beyond the rules both phases share, [`Phase::Authored`] refuses a `${...}` reference where
/// substitution must not produce one (an `env` key, a `user.id`), and [`Phase::Final`] refuses an
/// empty `command` and a relative `cwd` or `logs`. A path is checked as text; a non-UTF-8 one is
/// refused first, so the message is exact.
fn check_field_values(
    id: &Id,
    values: &FieldValues,
    phase: Phase,
    mut resolve: impl FnMut(PathField) -> Result<(), Error>,
) -> Result<(), Error> {
    check_name(id, values.name)?;

    if let Some(command) = values.command {
        if phase == Phase::Final {
            reject_empty_command(id, command)?;
        }
        check_command_args(id, command)?;
        if let Some(first) = command.first() {
            reject_empty(id, "command[0]", first)?;
        }
        resolve(PathField::Command)?;
    }

    for (field, path) in [(PathField::Cwd, values.cwd), (PathField::Logs, values.logs)] {
        let name = field.name();
        if let Some(path) = path {
            let Some(text) = path.to_str() else {
                return Err(invalid(
                    id,
                    &format!(
                        "field `{name}` is `{}`, which is not valid UTF-8",
                        path.to_string_lossy()
                    ),
                ));
            };
            check_path_text(id, name, text)?;
            if phase == Phase::Final {
                reject_relative_path(id, name, path)?;
            }
            resolve(field)?;
        }
    }

    for (key, value) in values.env {
        // Checked for every backend: `scm: {env: {"${K}": v}}` must be caught from Linux too.
        // `interpolate::spec` keeps its own copy, so it does not depend on this running first.
        if phase == Phase::Authored && interpolate::would_substitution_change(key) {
            return Err(invalid(id, interpolate::ENV_NAME_MESSAGE));
        }
        check_env_entry(id, key, value)?;
    }

    if phase == Phase::Authored {
        if let User::Id(AccountId::Sid(s)) = values.user {
            // Same reason as the `env`-name rule above. `${S}` trims to `${S}`, never empty, so
            // this and `check_user`'s blank check can never both fire on one value.
            if interpolate::would_substitution_change(s) {
                return Err(invalid(id, interpolate::USER_ID_MESSAGE));
            }
        }
    }
    check_user(id, values.user)
}

fn check_name(id: &Id, name: &str) -> Result<(), Error> {
    reject_unemittable(id, "name", name)
}

/// Every `command` element, `command[0]` included. Its emptiness is separate: see [`reject_empty`].
fn check_command_args(id: &Id, command: &[String]) -> Result<(), Error> {
    for arg in command {
        reject_unemittable(id, "command", arg)?;
    }
    Ok(())
}

/// A `cwd` or `logs`, as the text of the path.
fn check_path_text(id: &Id, field: &str, text: &str) -> Result<(), Error> {
    reject_unemittable(id, field, text)?;
    reject_empty(id, field, text)
}

fn check_env_entry(id: &Id, key: &str, value: &str) -> Result<(), Error> {
    reject_env_key_with_equals(id, key)?;
    reject_unemittable(id, "env key", key)?;
    reject_empty(id, "env key", key)?;
    reject_unemittable(id, &format!("env[{key}]"), value)
}

fn check_user(id: &Id, user: &User) -> Result<(), Error> {
    match user {
        User::Root | User::Id(AccountId::Uid(_)) => Ok(()),
        User::Name(n) => {
            reject_unemittable(id, "user.name", n)?;
            reject_blank(id, "user.name", n)
        }
        User::Id(AccountId::Sid(s)) => {
            reject_unemittable(id, "user.id", s)?;
            reject_blank(id, "user.id", s)
        }
    }
}

/// Every gate a user-supplied string must pass before it can be emitted into any artifact. One
/// entry point, so a field added later cannot pick up half the checks. Each defect they catch
/// would be silent if no gate did.
///
/// systemd's `Description=`/`Environment=`, launchd's `UserName` and every other emitted
/// directive have no escaping, and a control character (above all a newline) can terminate one
/// directive and start another. XML 1.0 cannot represent most C0 control characters at all, so
/// this also keeps the launchd generator from emitting an unparseable plist.
fn reject_unemittable(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    reject_control_chars(id, field, value)?;
    reject_trailing_backslash(id, field, value)?;
    Ok(())
}

/// How `value` breaks [`reject_unemittable`], as a predicate to follow the name of what was
/// checked; `None` if it does not. For a caller that names the value differently (a directory, a
/// resolved path).
fn unemittable_defect(value: &str) -> Option<String> {
    control_defect(value).or_else(|| trailing_backslash_defect(value).map(str::to_string))
}

fn reject_control_chars(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    match control_defect(value) {
        Some(defect) => Err(invalid(id, &format!("field `{field}` {defect}"))),
        None => Ok(()),
    }
}

/// How `value` breaks the control-character rule, as a predicate to follow the name of what was
/// checked; `None` if it does not.
fn control_defect(value: &str) -> Option<String> {
    if value.chars().any(|c| c.is_control()) {
        return Some("contains a control character, which is forbidden".to_string());
    }
    // `char::is_control()` misses these, but XML 1.0 cannot represent a noncharacter even as a
    // numeric entity, so one would make the launchd plist unparseable.
    value.chars().find(|c| is_xml_noncharacter(*c)).map(|c| {
        format!(
            "contains U+{:04X}, a Unicode noncharacter that XML cannot represent",
            c as u32
        )
    })
}

/// The Unicode noncharacters: `U+FDD0..=U+FDEF`, and the last two code
/// points of every plane.
fn is_xml_noncharacter(c: char) -> bool {
    let n = c as u32;
    (0xFDD0..=0xFDEF).contains(&n) || (n & 0xFFFE) == 0xFFFE
}

/// A value ending in a backslash is a line continuation to systemd, which
/// merges its directive with the following line. That is a privilege
/// boundary: a `name` ending in `\` can swallow the `User=` directive and
/// silently run the daemon as root. Interior backslashes must stay allowed,
/// or ordinary Windows paths become unexpressible.
fn reject_trailing_backslash(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    match trailing_backslash_defect(value) {
        Some(defect) => Err(invalid(id, &format!("field `{field}` {defect}"))),
        None => Ok(()),
    }
}

/// How `value` breaks the trailing-backslash rule, as a predicate to follow the name of what was
/// checked; `None` if it does not.
fn trailing_backslash_defect(value: &str) -> Option<&'static str> {
    let trailing = value.len() - value.trim_end_matches('\\').len();
    (trailing % 2 == 1).then_some("ends in a backslash, which systemd reads as a line continuation")
}

/// `command` must name at least one argv entry.
fn reject_empty_command(id: &Id, command: &[String]) -> Result<(), Error> {
    if command.is_empty() {
        return Err(invalid(id, "command must not be empty"));
    }
    Ok(())
}

/// Fields where the empty string is never meaningful and is silently
/// dangerous.
///
/// Applied to `command[0]`, `cwd` and `logs` when present, and every `env` key: an empty one
/// fails to resolve or, for `command[0]`, would silently resolve to the manifest directory.
/// `user.name` and `user.id` need the stronger [`reject_blank`]. Deliberately **not** applied
/// to `name` (an empty `Description=` is harmless), `env` values (`FOO=` is a normal assignment)
/// or `command[1..]` (an empty argv element is legitimate and every generator quotes it).
fn reject_empty(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(invalid(id, &format!("field `{field}` must not be empty")));
    }
    Ok(())
}

/// Like [`reject_empty`], but also rejects a value that is nothing but whitespace. Applied only
/// to `user.name` and `user.id` (the `Sid` arm), never to a path or an `env` key. The asymmetry
/// is deliberate:
///
/// - systemd rejects neither a whitespace-only `User=` nor an empty one (`systemd-analyze verify`
///   accepts both alike), so goetia is the only gate. An empty `User=` resets to root; a
///   whitespace-only value reaching the unit either does the same or names an account that
///   cannot exist.
/// - A path has no such default: a file named `" "` is legal on Unix, and an all-whitespace
///   `cwd`/`logs`/`command[0]` simply fails to resolve.
/// - Whether an `env` key may contain a space at all is a separate question; only its
///   *emptiness* is checked, via [`reject_empty`].
///
/// An installed artifact whose blob carries an empty or whitespace-only `user.name` decodes as
/// `Installed::OursUnusable`, not silently as root.
///
/// This is the only gate between an authored empty name and `canonical_account("")`, which maps
/// it to `LocalSystem` on Windows; `windows_builtin`'s `""` row is the other half (see there).
fn reject_blank(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(invalid(id, &format!("field `{field}` must not be empty")));
    }
    Ok(())
}

/// An env key containing `=` would make a `KEY=VALUE` env-file line or a
/// systemd `Environment=` directive ambiguous about where the key ends.
fn reject_env_key_with_equals(id: &Id, key: &str) -> Result<(), Error> {
    if key.contains('=') {
        return Err(invalid(id, &format!("env key `{key}` must not contain `=`")));
    }
    Ok(())
}

/// `cwd` and `logs` are always resolved to absolute paths here, so a relative one in a
/// `DaemonSpec` is already corrupt.
fn reject_relative_path(id: &Id, field: &str, path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(invalid(
            id,
            &format!("field `{field}` must be an absolute path, got `{}`", path.display()),
        ));
    }
    Ok(())
}

/// `pub(super)`: `spec::backend` builds the same `Error::Invalid` for `BackendKind::error`.
pub(super) fn invalid(id: &Id, message: &str) -> Error {
    Error::Invalid {
        daemon: id.as_str().to_string(),
        message: message.to_string(),
        source: None,
    }
}

/// [`invalid`], for a rejection that is `source` failing to parse or resolve what the manifest
/// holds.
fn invalid_from(id: &Id, message: &str, source: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Invalid {
        daemon: id.as_str().to_string(),
        message: message.to_string(),
        source: Some(std::sync::Arc::new(source)),
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod resolve_tests;
