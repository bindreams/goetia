//! `RawSpec -> DaemonSpec`: defaults, absolute-path resolution, and the
//! injection validation gate.
//!
//! This is the crate's parse-don't-validate boundary. systemd's `Description=` and `Environment=` have
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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::backend::{BackendKind, Shaped, ShapedSpec};
use super::field::Field;
use super::interpolate::{self, Substitution};
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
    resolve_shape(&id, base, base_dir, Substitution::Pending, None)?;

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
            let (shaped, _dropped_warnings) = resolve_shape(&id, merged, base_dir, Substitution::Pending, None)?;
            if Some(backend) == native {
                return Ok(());
            }
            backend.error(&shaped, &supplied, Substitution::Pending)
        })();
        sweep.map_err(|e| annotate_sweep_error(e, backend, has_entry))?;
    }

    // Step 5: the native pass, the substitution whose result is kept. Authoritative: the
    // injection gate inspects final values here, and its `ShapedSpec` becomes the `DaemonSpec`.
    // No `check_spec_grammar`: a substituted `.env` value may legally carry a literal `$`.
    let native_entry = native.filter(|b| raw.backend_specific.contains_key(b));
    let from_env = interpolate::spec(&mut native_merged, &vars).map_err(|e| annotate_step5_error(e, native_entry))?;
    // The one place an error of this pass is attributed: to `.env` by the field it is about, and to
    // the native override when it wrote that field's value.
    let blame = |refused: Refused| {
        let by_override = refused
            .field
            .as_ref()
            .is_some_and(|field| native_supplied.supplies(field));
        let err = refused.attributed(&from_env);
        match native {
            Some(b) if by_override => attribute_to(err, b),
            _ => err,
        }
    };
    let (shaped, mut daemon_warnings) =
        resolve_shape(&id, native_merged, base_dir, Substitution::Done, native).map_err(blame)?;
    // `error` reads only `native_supplied` fields, and only `user`: it is the backend's verdict on
    // what the override wrote, so it names the backend too, ahead of the `.env` attribution.
    if let Some(b) = native {
        b.error(&shaped, &native_supplied, Substitution::Done)
            .map_err(|e| attribute_to(Refused::at(Field::User, e).attributed(&from_env), b))?;
        b.refuse(&shaped).map_err(blame)?;
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

/// Annotation for an error of step 5's substitution, which is always an `Error::Interpolate`: it
/// gains a "merged with" path suffix, gated on `native_entry` (was this backend mentioned at all),
/// not on `Supplied` (which fields did it write): `scm: {}` is an entry whose `Supplied` equals a
/// missing entry's. The native `BackendKind::error` verdict is annotated by `attribute_to` at its
/// call site, since step 4 never ran it.
fn annotate_step5_error(err: Error, native_entry: Option<BackendKind>) -> Error {
    match err {
        Error::Interpolate { path, message } => {
            let path = match native_entry {
                Some(b) => format!("{path} (merged with backend-specific.{b})"),
                None => path,
            };
            Error::Interpolate { path, message }
        }
        other => other,
    }
}

/// An error about one field's value. In step 5 it is attributed to `.env` by [`Refused::attributed`],
/// the one place that happens.
#[derive(Debug)]
pub(super) struct Refused {
    /// The field the error is about. `None` for a defect no field's text is to blame for: the
    /// manifest directory's.
    pub(super) field: Option<Field>,
    err: Error,
}

impl Refused {
    pub(super) fn at(field: Field, err: Error) -> Self {
        Refused {
            field: Some(field),
            err,
        }
    }

    /// An error no field's text is to blame for: the manifest directory's (`-f`).
    pub(super) fn of_manifest_dir(err: Error) -> Self {
        Refused { field: None, err }
    }

    /// The error, prefixed "after substituting from .env" iff `.env` supplied some of the text of
    /// the field it is about, so `from_env` ([`interpolate::spec`]'s result) holds it.
    fn attributed(self, from_env: &BTreeSet<Field>) -> Error {
        match (self.field, self.err) {
            (
                Some(field),
                Error::Invalid {
                    daemon,
                    message,
                    source,
                },
            ) if from_env.contains(&field) => Error::Invalid {
                daemon,
                message: format!("after substituting from .env: {message}"),
                source,
            },
            (_, err) => err,
        }
    }
}

impl From<Refused> for Error {
    fn from(refused: Refused) -> Self {
        refused.err
    }
}

/// Shape only: injection gate, path resolution, the three `$`-aware
/// parses. Requires no particular field to be present — completeness is
/// [`require_complete`]'s job.
///
/// `substitution` says whether the values may still hold references, which decides what is judged
/// now. `native` is the backend whose rules refuse a path as it is joined
/// ([`BackendKind::refuse_path`]): only the pass whose result is kept names one. An error says which
/// field it is about ([`Refused`]); attributing it is the caller's.
fn resolve_shape(
    id: &Id,
    raw: RawSpec,
    base_dir: &Path,
    substitution: Substitution,
    native: Option<BackendKind>,
) -> Result<(ShapedSpec, Vec<Warning>), Refused> {
    debug_assert!(
        raw.backend_specific.is_empty(),
        "resolve_shape must be handed a merged spec, never one still carrying overrides"
    );

    let mut warnings = Vec::new();

    let name = raw.name.unwrap_or_else(|| id.as_str().to_string());
    let user = resolve_user(raw.user);

    // The rules run on every backend's merged spec, the non-native ones that never become a
    // `DaemonSpec` included: a newline in `scm`'s env must be caught from Linux too. An absent or
    // empty `command` is incomplete, not invalid: a native override may supply it, and
    // `require_complete` reports the miss with the backend named.
    let verdicts = check_field_values(
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
    );
    let mut command = raw.command;
    let mut cwd = raw.cwd;
    let mut logs = raw.logs;
    for Verdict { resolve, result } in verdicts {
        result?;
        let Some(path_field) = resolve else {
            continue;
        };
        let slot = match path_field {
            PathField::Command => command.as_mut().and_then(|command| command.first_mut()),
            PathField::Cwd => cwd.as_mut(),
            PathField::Logs => logs.as_mut(),
        };
        if let Some(text) = slot {
            *text = resolve_path_string(id, path_field, text, base_dir, substitution, native, &mut warnings)?;
        }
    }
    let cwd = cwd.map(PathBuf::from);
    let logs = logs.map(PathBuf::from);
    let env = raw.env;

    let restart = shape(id, Field::Restart, raw.restart, substitution, parse_restart)?;
    let restart_delay = shape(
        id,
        Field::RestartDelay,
        raw.restart_delay,
        substitution,
        parse_restart_delay,
    )?;
    let kind = shape(id, Field::Kind, raw.kind, substitution, parse_kind)?;

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

/// Keep the parse in the shape phase, deferring exactly the values [`Substitution::defers`] names:
/// while pending, any with a `$` (a `$$` escape too; see `would_substitution_change`), and once
/// substituted, none.
fn shape<T>(
    id: &Id,
    field: Field,
    raw: Option<String>,
    substitution: Substitution,
    parse: fn(&Id, &str) -> Result<T, Error>,
) -> Result<Shaped<T>, Refused> {
    match raw {
        None => Ok(Shaped::Absent),
        Some(text) if substitution.defers(&text) => Ok(Shaped::Deferred),
        Some(text) => parse(id, &text)
            .map(Shaped::Parsed)
            .map_err(|err| Refused::at(field, err)),
    }
}

/// Completeness: every field the installed spec must actually have, plus the defaults. Errors
/// name `backend`.
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

    let restart = settled(&id, "restart", restart)?.unwrap_or(Restart::Never);
    let restart_delay = settled(&id, "restart-delay", restart_delay)?;
    let kind = settled(&id, "type", kind)?.unwrap_or(Kind::Simple);

    // The rules already ran on these values and on every path resolved (`command[0]` included, by
    // `resolve_shape`), so `new` is what makes the result a `DaemonSpec`, not a check: it cannot
    // refuse a manifest.
    let daemon = id.clone();
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
    .map_err(|err| contract_violation_from(&daemon, "the resolved daemon failed its own field rules", err))
}

/// A defect of this crate, not of any manifest: something an earlier step ruled out happened. An
/// error rather than a `debug_assert!`, which would abort the path that reports it.
fn contract_violation(id: &Id, message: &str) -> Error {
    invalid(id, &contract_violation_text(message, None))
}

/// The one wording of a contract violation, with the refusing rule's message when it has one.
fn contract_violation_text(message: &str, rule: Option<&str>) -> String {
    match rule {
        Some(rule) => format!("internal error: {message}: {rule}; this is a bug in goetia"),
        None => format!("internal error: {message}; this is a bug in goetia"),
    }
}

/// [`contract_violation`] for a field rule that refused what an earlier step should have let
/// through. The rule's own message is folded in, so the daemon is named once.
fn contract_violation_from(id: &Id, message: &str, cause: Error) -> Error {
    match cause {
        Error::Invalid {
            message: rule, source, ..
        } => Error::Invalid {
            daemon: id.as_str().to_string(),
            message: contract_violation_text(message, Some(&rule)),
            source,
        },
        other => invalid_from(id, &contract_violation_text(message, None), other),
    }
}

/// What a substituted pass leaves of a [`Shaped`] value: it defers nothing, so the value is parsed
/// or absent.
fn settled<T>(id: &Id, field: &str, shaped: Shaped<T>) -> Result<Option<T>, Error> {
    match shaped {
        Shaped::Parsed(value) => Ok(Some(value)),
        Shaped::Absent => Ok(None),
        Shaped::Deferred => Err(contract_violation(
            id,
            &format!("field `{field}` was left to a substitution that has already run"),
        )),
    }
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

/// Join a relative path against `base_dir`; leave an absolute path as-is.
/// Uses `Path::is_absolute`, not a string prefix check, since POSIX and Windows differ.
///
/// The result is checked ([`check_resolved`]), not `base_dir` on its own: a directory is refused
/// only through a path that is joined to it and so would carry its defect into an artifact. While
/// a substitution is [`Pending`](Substitution::Pending), a value it may change is left to a later
/// pass (`${D}` may be absolute, which no join would touch) and comes back as written. A resolved
/// path is never converted lossily: one that is not valid UTF-8 is refused. The native backend, when
/// named, refuses what it cannot carry in the result ([`BackendKind::refuse_path`]), here because
/// the written text and the joined path are both in hand.
///
/// Not a pure function of `(value, base_dir)`: `std::path::absolute`'s drive-relative fallback
/// reads the per-drive current directory. But every pass `resolve` makes runs in one process
/// against one unchanging process state, so identical values get identical verdicts, which makes
/// "the base pass passed, so this failure is the override's" sound.
fn resolve_path_string(
    id: &Id,
    field: PathField,
    raw: &str,
    base_dir: &Path,
    substitution: Substitution,
    native: Option<BackendKind>,
    warnings: &mut Vec<Warning>,
) -> Result<String, Refused> {
    let name = field.name();
    // A value a later pass will see substituted is that pass's to resolve and to judge.
    if substitution.defers(raw) {
        return Ok(raw.to_string());
    }
    let p = Path::new(raw);
    let under_manifest_dir = is_under_manifest_dir(p);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
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
            Refused::at(
                field.field(),
                invalid_from(
                    id,
                    &format!("field `{name}` is `{raw}`, which cannot be resolved to an absolute path"),
                    source,
                ),
            )
        })?;
        if resolved.is_absolute() {
            // Resolved against process state, so `install` and `diff` run from different
            // directories can disagree and report drift on an untouched artifact.
            warnings.push(Warning {
                id: Some(id.clone()),
                message: format!(
                    "field `{name}` is `{raw}`, a drive-relative path; it is anchored to the \
                     current directory on that drive in whichever process runs goetia, not to \
                     the manifest directory. Write a full path (e.g. `C:\\bin\\frpc.exe`) or a \
                     manifest-relative one (e.g. `.\\bin\\frpc.exe`) instead."
                ),
            });
        }
        resolved
    };
    debug_assert!(joined.is_absolute(), "`std::path::absolute` returned a relative path");
    check_resolved(id, field, raw, &joined, under_manifest_dir)?;
    let joined = joined
        .into_os_string()
        .into_string()
        .expect("`check_resolved` refused a path that is not valid UTF-8");
    if let Some(native) = native {
        native.refuse_path(id, &field.field(), raw, &joined, base_dir)?;
    }
    Ok(joined)
}

/// Whether `p` is joined to the manifest directory. A path with its own prefix (`C:bin`) replaces
/// `base_dir` in the join, and a root-relative one (`/bin`, `\bin` on Windows) keeps only the
/// drive of `base_dir`, so neither is under it.
pub(super) fn is_under_manifest_dir(p: &Path) -> bool {
    !p.has_root() && !matches!(p.components().next(), Some(std::path::Component::Prefix(_)))
}

/// The field rules for a resolved path, which is what an artifact would carry. `raw` has passed
/// them already, so the defect is one the user did not write: it came from the manifest directory
/// (`under_manifest_dir`) or, for a drive-relative path, from the per-drive current directory. The
/// message says the field *resolved* to the bad value and names its source. The directory's defect
/// is no field's: it carries none.
fn check_resolved(
    id: &Id,
    field: PathField,
    raw: &str,
    resolved: &Path,
    under_manifest_dir: bool,
) -> Result<(), Refused> {
    let name = field.name();
    let Some(defect) = path_defect(resolved) else {
        return Ok(());
    };
    let resolves = if under_manifest_dir {
        "resolves under the manifest directory to"
    } else {
        "resolves to"
    };
    let err = invalid(
        id,
        &format!(
            "field `{name}` is `{raw}`, which {resolves} `{}`, which {defect}",
            resolved.display()
        ),
    );
    Err(if under_manifest_dir {
        Refused::of_manifest_dir(err)
    } else {
        Refused::at(field.field(), err)
    })
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
/// canonicalised. For that reason the leading `..` of a relative `dir` is collapsed against it
/// (`-f ../app` is the parent of the working directory, however goetia was started), and no other
/// `..` is: see the loop below.
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
    let mut anchor = canonicalize_cwd(&cwd)?;
    // A leading `..` consumes a component of `anchor`, which holds no symlink, so collapsing it is
    // exact. It is the only `..` that is: a later one follows a component the user named, which
    // may be a symlink.
    let mut rest = dir
        .components()
        .filter(|c| *c != std::path::Component::CurDir)
        .peekable();
    while rest.next_if(|c| *c == std::path::Component::ParentDir).is_some() {
        anchor.pop();
    }
    let joined = normalize(&anchor.join(rest.collect::<PathBuf>()));
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

/// The field rules over a finished daemon, for [`DaemonSpec::new`].
pub(super) fn check_fields(fields: &DaemonFields) -> Result<(), Error> {
    let verdicts = check_field_values(&fields.id, &FieldValues::from(fields), Phase::Final);
    for verdict in verdicts {
        verdict.result?;
    }
    Ok(())
}

/// The values the field rules read, as the caller holds them: authored text, or a finished
/// daemon's fields. Paths are [`Path`]s so one function takes both. The systemd rules read the same
/// bundle (`backend`).
pub(super) struct FieldValues<'a> {
    pub(super) name: &'a str,
    /// `None` only while authored: the manifest may leave `command` to a native override.
    pub(super) command: Option<&'a [String]>,
    pub(super) cwd: Option<&'a Path>,
    pub(super) logs: Option<&'a Path>,
    pub(super) env: &'a BTreeMap<String, String>,
    pub(super) user: &'a User,
}

impl<'a> From<&'a DaemonFields> for FieldValues<'a> {
    fn from(fields: &'a DaemonFields) -> Self {
        FieldValues {
            name: &fields.name,
            command: Some(&fields.command),
            cwd: fields.cwd.as_deref(),
            logs: fields.logs.as_deref(),
            env: &fields.env,
            user: &fields.user,
        }
    }
}

impl<'a> From<&'a DaemonSpec> for FieldValues<'a> {
    fn from(spec: &'a DaemonSpec) -> Self {
        FieldValues::from(&**spec)
    }
}

impl<'a> From<&'a ShapedSpec> for FieldValues<'a> {
    fn from(spec: &'a ShapedSpec) -> Self {
        FieldValues {
            name: &spec.name,
            command: spec.command.as_deref(),
            cwd: spec.cwd.as_deref(),
            logs: spec.logs.as_deref(),
            env: &spec.env,
            user: &spec.user,
        }
    }
}

impl FieldValues<'_> {
    /// The path `field` holds (`command[0]`, `cwd` or `logs`), or `None` for another field or an
    /// absent value.
    pub(super) fn path(&self, field: &Field) -> Option<&Path> {
        match field {
            Field::Command(0) => self.command?.first().map(Path::new),
            Field::Cwd => self.cwd,
            Field::Logs => self.logs,
            _ => None,
        }
    }
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

    /// The field a resolution error is about: for `command`, its program.
    fn field(self) -> Field {
        match self {
            PathField::Command => Field::Command(0),
            PathField::Cwd => Field::Cwd,
            PathField::Logs => Field::Logs,
        }
    }
}

/// What the rules say of one field, and where its path is resolved.
struct Verdict {
    /// The path to resolve once `result` is `Ok`, before the next verdict is read: a resolution
    /// error falls at its field's place in the order.
    resolve: Option<PathField>,
    result: Result<(), Refused>,
}

/// The one place the field rules run: one [`Verdict`] per field present, in the order a manifest's
/// defects are reported (`name`, `command`, `cwd`, `logs`, each `env` entry, `user`). The caller
/// reads them in order and stops at the first error: [`check_fields`] on a finished daemon's
/// fields, [`resolve_shape`] on the authored values. A rule never resolves a path.
///
/// Beyond the rules both phases share, [`Phase::Authored`] refuses a `${...}` reference where
/// substitution must not produce one (an `env` key, a `user.id`), and [`Phase::Final`] refuses an
/// empty `command` and a relative `command[0]`, `cwd` or `logs`.
///
/// Every error says which [`Field`] it is about ([`Refused`]).
fn check_field_values(id: &Id, values: &FieldValues, phase: Phase) -> Vec<Verdict> {
    let name = Verdict {
        resolve: None,
        result: reject_control_chars(id, "name", values.name).map_err(|err| Refused::at(Field::Name, err)),
    };
    let command = values.command.map(|command| Verdict {
        resolve: Some(PathField::Command),
        result: check_command(id, command, phase),
    });
    let path = |field: PathField, path: Option<&Path>| {
        path.map(|path| Verdict {
            resolve: Some(field),
            result: check_path_field(id, field, path, phase),
        })
    };
    let env = Verdict {
        resolve: None,
        result: check_env(id, values.env, phase),
    };
    let user = Verdict {
        resolve: None,
        result: check_user_field(id, values.user, phase),
    };

    [
        Some(name),
        command,
        path(PathField::Cwd, values.cwd),
        path(PathField::Logs, values.logs),
        Some(env),
        Some(user),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn check_command(id: &Id, command: &[String], phase: Phase) -> Result<(), Refused> {
    if phase == Phase::Final {
        reject_empty_command(id, command).map_err(|err| Refused::at(Field::Command(0), err))?;
    }
    // Every element, `command[0]` included. Its emptiness is separate.
    for (index, arg) in command.iter().enumerate() {
        reject_control_chars(id, "command", arg).map_err(|err| Refused::at(Field::Command(index), err))?;
    }
    if let Some(first) = command.first() {
        reject_empty(id, "command[0]", first).map_err(|err| Refused::at(Field::Command(0), err))?;
        if phase == Phase::Final {
            reject_relative_path(id, "command[0]", Path::new(first))
                .map_err(|err| Refused::at(Field::Command(0), err))?;
        }
    }
    Ok(())
}

fn check_path_field(id: &Id, field: PathField, path: &Path, phase: Phase) -> Result<(), Refused> {
    let name = field.name();
    check_path(id, name, path).map_err(|err| Refused::at(field.field(), err))?;
    if phase == Phase::Final {
        reject_relative_path(id, name, path).map_err(|err| Refused::at(field.field(), err))?;
    }
    Ok(())
}

fn check_env(id: &Id, env: &BTreeMap<String, String>, phase: Phase) -> Result<(), Refused> {
    for (key, value) in env {
        // Checked for every backend: `scm: {env: {"${K}": v}}` must be caught from Linux too.
        // `interpolate::spec` keeps its own copy, so it does not depend on this running first.
        if phase == Phase::Authored && interpolate::would_substitution_change(key) {
            return Err(Refused::at(
                Field::EnvKey(key.as_str().into()),
                invalid(id, interpolate::ENV_NAME_MESSAGE),
            ));
        }
        check_env_key(id, key).map_err(|err| Refused::at(Field::EnvKey(key.as_str().into()), err))?;
        reject_control_chars(id, &format!("env[{key}]"), value)
            .map_err(|err| Refused::at(Field::EnvValue(key.clone()), err))?;
    }
    Ok(())
}

fn check_user_field(id: &Id, user: &User, phase: Phase) -> Result<(), Refused> {
    // Same reason as the `env`-name rule. `${S}` trims to itself, never empty, so this and
    // `check_user`'s blank check cannot both fire.
    if phase == Phase::Authored
        && let User::Id(AccountId::Sid(s)) = user
        && interpolate::would_substitution_change(s)
    {
        return Err(Refused::at(Field::User, invalid(id, interpolate::USER_ID_MESSAGE)));
    }
    check_user(id, user).map_err(|err| Refused::at(Field::User, err))
}

/// A `cwd` or `logs`. The message omits the path, as [`reject_control_chars`] omits a value.
fn check_path(id: &Id, field: &str, path: &Path) -> Result<(), Error> {
    if let Some(defect) = path_defect(path) {
        return Err(invalid(id, &format!("field `{field}` {defect}")));
    }
    reject_empty(id, field, path)
}

fn check_env_key(id: &Id, key: &str) -> Result<(), Error> {
    reject_env_key_with_equals(id, key)?;
    reject_control_chars(id, "env key", key)?;
    reject_empty(id, "env key", key)
}

fn check_user(id: &Id, user: &User) -> Result<(), Error> {
    match user {
        User::Root | User::Id(AccountId::Uid(_)) => Ok(()),
        User::Name(n) => {
            reject_control_chars(id, "user.name", n)?;
            reject_blank(id, "user.name", n)
        }
        User::Id(AccountId::Sid(s)) => {
            reject_control_chars(id, "user.id", s)?;
            reject_blank(id, "user.id", s)
        }
    }
}

/// How `path` breaks the path rules, as a predicate to follow the name of what was checked; `None`
/// if it does not: it must be valid UTF-8 and pass [`reject_control_chars`]'s checks. The one list
/// for an authored `cwd` or `logs` and for a resolved path; each caller supplies its own prefix.
fn path_defect(path: &Path) -> Option<String> {
    let Some(text) = path.to_str() else {
        return Some("is not valid UTF-8".to_string());
    };
    control_defect(text)
}

/// The gate every user-supplied string passes before it can be emitted into any artifact. A
/// control character (above all a newline) can end one systemd directive and start another, and
/// XML 1.0 cannot represent most C0 control characters at all, so one would make the launchd
/// plist unparseable.
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
fn reject_empty(id: &Id, field: &str, value: impl AsRef<std::ffi::OsStr>) -> Result<(), Error> {
    if value.as_ref().is_empty() {
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

/// `command[0]`, `cwd` and `logs` are always resolved to absolute paths here, so a relative one in a
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
