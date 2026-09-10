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
//!
//! **Why `command[0]` is absolutized here and `command[1..]` is not.**
//! `execve`, `posix_spawn` and `CreateProcess` all take an argument vector
//! as opaque bytes — no launcher on any platform resolves a path *inside*
//! an argument. A relative path in `command[1..]` is therefore resolved by
//! the program being launched, against whatever working directory that
//! program sees at the time; goetia has no say in it, and this is not a
//! goetia policy to begin with.
//!
//! `command[0]` is different because, unlike an argument, it is a field
//! with a defined role, and the three platforms disagree about that role
//! for a relative value. systemd requires "either an absolute path to an
//! executable or a simple file name without any slashes", resolving a bare
//! name against a fixed compile-time search path and **never** against
//! `WorkingDirectory=` — so a manifest's `bin/frpc` (a name with a slash
//! that is not absolute) is rejected outright there. A Windows service has
//! no working directory of its own at all, and resolves a relative binary
//! against `System32`. launchd does neither: it resolves
//! `ProgramArguments[0]` against the job's working directory, which
//! `generate` emits only when `cwd` is set, so a relative binary there
//! silently resolves against the default rather than being refused.
//! Absolutizing `command[0]` against the manifest's directory before any
//! backend sees it collapses those three disagreeing rules into one that
//! always holds — and it is the only one of the three that does not depend
//! on where the daemon happens to start.
//!
//! **The dragon is in the default.** Because only `command[0]` is
//! absolutized, a manifest that passes a relative path as an *argument* —
//! `command: ["bin/frpc", "-c", "host/frpc.toml"]` — and does not set
//! `cwd` inherits whatever working directory the platform defaults to.
//! systemd documents that default, for system instances, as the **root
//! directory**: `-c host/frpc.toml` then resolves against `/`, and the
//! daemon starts, cannot find its own config, and exits with no
//! indication that a working directory was ever the problem. Set `cwd`, or
//! write the argument absolute.
//!
//! # Why the grammar check exists, and why it is not just `scalar` with an
//! empty `Vars`
//!
//! `interpolate::scalar` rejects a `$` that is not part of `${...}` or `$$`,
//! rejects an unterminated `${`, an empty name, an invalid name character,
//! and a `$` inside a `:-` default. Every one of those is `str` grammar over
//! authored text — no `.env`, no substitution, no target machine — which is
//! shape. But `interpolate::spec` **never runs on a non-native override**,
//! so without a separate check `scm: {name: "$HOME"}` would resolve cleanly
//! on Linux and fail only at install on Windows, and a bad `$` in a base
//! value that the native override replaces would never be diagnosed at all.
//! Running `scalar` with `Vars::empty()` is not the check: it would fail on
//! `${VAR}` for the missing variable, which is precisely the thing a
//! non-native backend is not entitled to decide. `interpolate::check_grammar`/
//! `check_spec_grammar` are that separate check.
//!
//! `check_spec_grammar` runs in [`Phase::Authored`] only: a `.env` value
//! carrying a literal `$` (`SECRET=abc$def`) is legal, survives
//! substitution, and would fail a grammar check applied to substituted
//! text.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::backend::{Backend, Shaped, ShapedSpec};
use super::interpolate;
use super::overrides::Supplied;
use super::raw::RawManifest;
use super::user::{AccountId, RawUser, User};
use super::vars::Vars;
use super::{DaemonSpec, Id, Kind, RawSpec, Restart, Warning};
use crate::error::Error;

const MANIFEST_FILE_NAME: &str = "goetia.yaml";

/// Which kind of pass this is. Only the path advisories read it: they
/// quote the value as written, so a pass over authored text must stay
/// silent about a path a substitution is going to rewrite, or one path
/// produces two differently-worded warnings the dedup cannot collapse. See
/// `resolve_path_string`.
#[derive(Clone, Copy)]
enum Phase {
    /// Steps 3 and 4: uninterpolated, authored text.
    Authored,
    /// Step 5: substituted text — 5a's advisory derivations, 5b's native
    /// pass.
    Substituted,
}

/// Turn a parsed manifest into resolved daemon specs.
///
/// Substitution happens inside `resolve`, per daemon, on the merged native
/// spec — see step 5b below. That is what lets `resolve` promise that no
/// `${VAR}` survives into a returned spec: were it a separate step a caller
/// had to remember, a manifest reaching this one directly would resolve
/// with its references intact, and systemd applies its *own* `${...}`
/// expansion to `ExecStart=` — turning an unsubstituted reference into an
/// empty string inside a privileged unit. That guarantee holds of every
/// spec `resolve` *returns*; it does not hold of a non-native backend's
/// override, which is validated as written (uninterpolated) and then
/// discarded — the best any host can honestly do for a backend it cannot
/// install.
///
/// Every backend's override is validated, on every host: see steps 3-5.
/// Only the native backend's merged spec — substituted, shaped, and
/// completed — becomes the returned `DaemonSpec`. Relative `command[0]`,
/// `cwd`, and `logs` paths are resolved against `base_dir` and written back
/// absolute. Fails on the first invalid daemon; a valid manifest may still
/// produce `Warning`s for properties that are accepted but cannot be
/// faithfully honored on every platform.
pub fn resolve(raw: RawManifest, base_dir: &Path) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    resolve_as(raw, base_dir, Backend::native())
}

/// [`resolve`], with the native backend named rather than detected.
///
/// Production has exactly one caller, [`resolve`], which passes
/// [`Backend::native`]. Tests pass any of the three, because the native arm
/// of steps 4 and 5 — the skip, the verdict, the advisory — is otherwise
/// only reachable from the one platform that runs it. A test that names a
/// non-native backend as `native` exercises the same code an install on
/// that platform would.
fn resolve_as(
    raw: RawManifest,
    base_dir: &Path,
    native: Option<Backend>,
) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    // Declared before `absolutize` runs, not after: `absolutize` itself can
    // now push a manifest-level advisory (a drive-relative `-f`), and
    // `Vec::new()` reads nothing, so hoisting the declaration up is safe by
    // inspection. The alternative — a second vector concatenated in after
    // the daemon loop — would get the asserted warning order wrong.
    let mut warnings = Vec::new();

    // `DaemonSpec` documents every path as absolute and `blob::decode`
    // enforces it, so a relative `base_dir` would produce an artifact whose
    // own embedded blob cannot be decoded — breaking the drift invariant for
    // the entirely ordinary `goetia daemon install -f .`. Anchor it here, at
    // the one place that makes the guarantee, rather than trusting every
    // caller to pass an absolute path.
    let base_dir = absolutize(base_dir, &mut warnings)?;
    let base_dir = base_dir.as_path();

    // Step 1: ids. Must run before `Id::try_from`, or the id-pattern check
    // (whose charset already excludes `$`) answers first with its generic
    // message. The id is not overridable, so this runs once per daemon.
    let mut entries: Vec<(Id, RawSpec)> = Vec::with_capacity(raw.daemons.len());
    for (key, raw_spec) in raw.daemons {
        interpolate::reject_interpolated_id(&key)?;
        let id = Id::try_from(key)?;
        entries.push((id, raw_spec));
    }

    // Step 2: the `.env` gate, computed from the specs substitution will
    // actually run on — *both* of them, since step 5b substitutes the merged
    // native spec and step 5a substitutes the base spec. Reading it from
    // anything else either misses a `${VAR}` written only in the native
    // override (it currently fails "no value for X" while `.env` defines it)
    // or lets an unrelated/unreadable `.env` break a host whose native
    // backend has no `$` anywhere.
    let merged_native: Vec<(RawSpec, Supplied)> = entries
        .iter()
        .map(|(_, raw)| match native {
            Some(b) => raw.merged_for(b),
            None => (raw.without_overrides(), Supplied::NONE),
        })
        .collect();
    let native_needs_vars = merged_native
        .iter()
        .any(|(spec, _)| interpolate::spec_would_change(spec));
    // A separate predicate, not folded into the one above, because the two
    // owe different things to a `.env` that fails to read: step 5b's
    // substitution is what gets installed, so an unreadable `.env` there is
    // fatal, while step 5a's only feeds advisories, and an advisory this host
    // cannot compute is silent. Both must be asked even so — a native
    // override replacing every `$`-bearing base field leaves the merged spec
    // literal, and a gate reading only that skips `.env`, deleting advisories
    // the same manifest emits when written without variables.
    let base_needs_vars = entries
        .iter()
        .any(|(_, raw)| interpolate::spec_would_change(&raw.without_overrides()));
    let vars = if native_needs_vars {
        Vars::load(base_dir)?
    } else if base_needs_vars {
        Vars::load(base_dir).unwrap_or_else(|_| Vars::empty())
    } else {
        Vars::empty()
    };

    let mut specs = Vec::with_capacity(entries.len());

    for ((id, raw), (mut native_merged, native_supplied)) in entries.into_iter().zip(merged_native) {
        let path = format!("daemons.{id}");
        let mut daemon_warnings = Vec::new();

        // Step 3: the base pass. Reported verbatim — no backend
        // attribution, because a base-spec error is not any backend's
        // fault, and base errors therefore always win on precedence. Its
        // warnings are dropped: a later pass re-derives every one of them
        // from the same authored values. No `Backend` verdict runs here:
        // the base spec is nobody's override.
        let base = raw.without_overrides();
        interpolate::check_spec_grammar(&base, &path)?;
        resolve_shape(&id, base, base_dir, Phase::Authored)?;

        // Step 4: the all-backends sweep, and errors only. Backend
        // annotation covers the whole iteration, not just `resolve_shape`:
        // every `Error` raised anywhere in this pass — from shape or from
        // `Backend::error` — is annotated with `backend` before it
        // propagates. Each non-native backend's shaped spec is kept for
        // step 5, which owns every advisory and is the fallback when the
        // substituted derivation there is unavailable.
        let mut swept: Vec<(Backend, ShapedSpec)> = Vec::with_capacity(Backend::ALL.len());
        for backend in Backend::ALL {
            let (merged, supplied) = raw.merged_for(backend);
            let has_entry = raw.backend_specific.contains_key(&backend);
            let sweep: Result<Option<ShapedSpec>, Error> = (|| {
                interpolate::check_spec_grammar(&merged, &path)?;
                let (shaped, pass_warnings) = resolve_shape(&id, merged, base_dir, Phase::Authored)?;
                // `supplied` is read by the non-native arm only; the native
                // arm keeps neither warnings nor verdicts — `error`/`warn`
                // run in step 5 instead, on the substituted text they are
                // entitled to. This pass is not redundant even so: it is
                // what makes step 5's failures provably substitution's
                // fault, and it is where a *literal* control character in
                // the native override is caught and attributed here.
                if Some(backend) == native {
                    return Ok(None);
                }
                backend.error(&shaped, supplied)?;
                daemon_warnings.extend(pass_warnings);
                Ok(Some(shaped))
            })();
            if let Some(shaped) = sweep.map_err(|e| annotate_sweep_error(e, backend, has_entry))? {
                swept.push((backend, shaped));
            }
        }

        // Step 5a: every non-native backend's advisories, computed on the
        // substituted base plus that backend's authored override. The base
        // half is text step 5b substitutes anyway, from the same `vars`, so
        // a `${VAR}` there resolves to the value the returned spec carries
        // and must be judged as that value — computing it from authored
        // text instead loses the advisory the moment a field moves into
        // `.env`. The override half is never substituted for a backend this
        // host cannot install, so a `$`-bearing value there stays
        // `Shaped::Deferred` and stays silent: the honest "cannot resolve"
        // case. Shape warnings from this derivation are dropped —
        // `resolve_path_string`'s advisory is keyed to the four cases its
        // doc enumerates, and a fifth pass would say nothing step 5b does
        // not already say.
        let advisory_base = substituted_base(&raw, &path, &vars);
        for (backend, authored) in &swept {
            let substituted = advisory_base
                .as_ref()
                .map(|base| base.merged_for(*backend).0)
                .and_then(|merged| resolve_shape(&id, merged, base_dir, Phase::Substituted).ok());
            backend.warn(
                substituted.as_ref().map_or(authored, |(shaped, _)| shaped),
                &mut daemon_warnings,
            );
        }

        // Step 5b: the native pass, and the substitution whose result is
        // kept. Authoritative: it is where the injection gate inspects
        // final values, and its `ShapedSpec` is the one that becomes a
        // `DaemonSpec`. No
        // `check_spec_grammar` here — the text is substituted, and a
        // `.env` value may legally carry a literal `$`.
        let native_entry = native.filter(|b| raw.backend_specific.contains_key(b));
        let substituted: Result<(ShapedSpec, Vec<Warning>), Error> = (|| {
            interpolate::spec(&mut native_merged, &path, &vars)?;
            resolve_shape(&id, native_merged, base_dir, Phase::Substituted)
        })();
        let (shaped, mut pass_warnings) = substituted.map_err(|e| annotate_step5_error(e, native_entry))?;

        // The native backend's own verdict, annotated like step 4's rather
        // than like the substitution above it: step 4 skipped this backend,
        // so this is the *first* pass to apply a per-backend rule to the
        // native spec, and "after substituting from .env" would be a claim
        // about a pass that never ran — naming a `.env` that need not exist.
        // `Backend::error` reads only fields `native_supplied` marks, so
        // `backend-specific.<native>` is where the value was written, the
        // same attribution its non-native twin gets.
        if let Some(b) = native {
            b.error(&shaped, native_supplied).map_err(|e| attribute_to(e, b))?;
            b.warn(&shaped, &mut pass_warnings);
        }
        daemon_warnings.extend(pass_warnings);

        // Step 6: dedup and complete.
        let daemon_warnings = dedup_warnings(daemon_warnings);
        let spec = require_complete(shaped, native)?;
        specs.push(spec);
        warnings.extend(daemon_warnings);
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
    // The one and only place a manifest is read: every diagnostic a
    // manifest can produce — line, column, `null`, duplicate key, unknown
    // field — comes from here, on the file exactly as written. `resolve`
    // interpolates after it, on the typed result, so a `${VAR}` cannot move
    // a position or change a message.
    let raw = RawManifest::parse(&text)?;

    resolve(raw, &base_dir)
}

/// `raw`'s base spec, substituted, carrying `raw`'s overrides back so
/// [`RawSpec::merged_for`] can layer an authored override on top of it —
/// the spec step 5a's advisories read.
///
/// `None` when substitution fails, and the caller then falls back to the
/// authored spec step 4 shaped. Step 2 gates `vars` on this spec as well as
/// on the merged native one, so the ordinary reasons to fail are a `.env`
/// that could not be read and a variable it does not define — neither an
/// error here, because an advisory this host cannot compute is silent.
fn substituted_base(raw: &RawSpec, path: &str, vars: &Vars) -> Option<RawSpec> {
    let mut base = raw.without_overrides();
    interpolate::spec(&mut base, path, vars).ok()?;
    base.backend_specific = raw.backend_specific.clone();
    Some(base)
}

/// Backend annotation for step 4. `Error::Invalid` (from `resolve_shape` or
/// `Backend::error`) always gains the `backend-specific.<backend>: `
/// message prefix: this pass's spec is `merged_for(backend)`, and the base
/// pass (step 3) already validated the value's base form, so a failure here
/// can only be attributable to this backend's override — `has_entry` is
/// true whenever this actually fires. `Error::Interpolate` (from
/// `check_spec_grammar`) gets a path suffix instead, gated on `has_entry`
/// explicitly rather than relying on that same reasoning to make the
/// ungated case unreachable.
fn annotate_sweep_error(err: Error, backend: Backend, has_entry: bool) -> Error {
    match err {
        Error::Interpolate { path, message } if has_entry => Error::Interpolate {
            path: format!("{path} (backend-specific.{backend})"),
            message,
        },
        other => attribute_to(other, backend),
    }
}

/// `backend-specific.<backend>: ` on an `Error::Invalid`, and nothing on any
/// other variant. Shared by step 4's sweep and step 5's native verdict so
/// the two spell one attribution one way.
fn attribute_to(err: Error, backend: Backend) -> Error {
    match err {
        Error::Invalid { daemon, message } => Error::Invalid {
            daemon,
            message: format!("backend-specific.{backend}: {message}"),
        },
        other => other,
    }
}

/// Backend annotation for step 5. `Error::Interpolate` (from
/// `interpolate::spec`'s substitution) gains a "merged with" path suffix,
/// gated on `native_entry` — `Supplied` answers "which fields did this
/// override write" and gates `Backend::error`; `native_entry` (`raw`'s
/// `backend_specific.contains_key`) answers "was this backend mentioned at
/// all" and gates this annotation; the two are not interchangeable, since
/// `scm: {}` is an entry that sets no field and its `Supplied` is
/// byte-identical to a missing entry's. `Error::Invalid` (from
/// `resolve_shape`'s injection gate) gains an "after substituting from .env"
/// message prefix instead: the uninterpolated pass over the same merged spec
/// (step 4's native arm) already ran `resolve_shape` and passed, so the only
/// thing that changed is substitution, and the error must say so rather than
/// pointing a reader at the manifest line that merely referenced the
/// variable.
///
/// Applied to the substitution and shape half of step 5 only. The native
/// `Backend::error` verdict is annotated by `attribute_to` at its own call
/// site: step 4 never ran it, so the premise this prefix rests on does not
/// hold for it.
fn annotate_step5_error(err: Error, native_entry: Option<Backend>) -> Error {
    match err {
        Error::Interpolate { path, message } => {
            let path = match native_entry {
                Some(b) => format!("{path} (merged with backend-specific.{b})"),
                None => path,
            };
            Error::Interpolate { path, message }
        }
        Error::Invalid { daemon, message } => Error::Invalid {
            daemon,
            message: format!("after substituting from .env: {message}"),
        },
        other => other,
    }
}

/// Deduplicate `warnings` on the whole [`Warning`] (`id` plus `message`),
/// keeping the first occurrence and preserving order.
/// `resolve_path_string`'s drive-relative advisory is a property of an
/// authored path value, not of a backend, so a path written in the *base*
/// spec produces the identical warning in every pass that keeps warnings —
/// collapsing those duplicates to one is this function's whole job.
///
/// Only that advisory ever reaches here twice: a `Backend::warn` advisory
/// belongs to one backend, which exactly one pass owns. So this call site
/// is exercised only on Windows, by
/// `a_base_drive_relative_path_warns_once_not_once_per_backend_pass`.
///
/// **This depends on the warning text quoting the raw path.** Two
/// advisories about two *different* paths stay distinct only because the
/// message contains the path; if that text is ever shortened to drop the
/// path, this begins silently merging warnings about different paths, and
/// no test catches it — the existing tests assert counts only for cases
/// where the messages genuinely are identical.
fn dedup_warnings(warnings: Vec<Warning>) -> Vec<Warning> {
    let mut out: Vec<Warning> = Vec::with_capacity(warnings.len());
    for w in warnings {
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// Shape only: injection gate, path resolution, the three `$`-aware
/// parses. Requires no particular field to be present — completeness is
/// [`require_complete`]'s job.
fn resolve_shape(id: &Id, raw: RawSpec, base_dir: &Path, phase: Phase) -> Result<(ShapedSpec, Vec<Warning>), Error> {
    debug_assert!(
        raw.backend_specific.is_empty(),
        "resolve_shape must be handed a merged spec, never one still carrying overrides"
    );

    let mut warnings = Vec::new();

    let name = raw.name.unwrap_or_else(|| id.as_str().to_string());
    reject_unemittable(id, "name", &name)?;

    let mut command = raw.command;
    if let Some(cmd) = &command {
        for arg in cmd {
            reject_unemittable(id, "command", arg)?;
        }
    }
    if let Some(first) = command.as_mut().and_then(|c| c.first_mut()) {
        reject_empty(id, "command[0]", first)?;
        *first = resolve_path_string(id, "command", first, base_dir, phase, &mut warnings)?;
    }

    let cwd = resolve_optional_path(id, "cwd", raw.cwd, base_dir, phase, &mut warnings)?;
    let logs = resolve_optional_path(id, "logs", raw.logs, base_dir, phase, &mut warnings)?;

    let mut env = BTreeMap::new();
    for (key, value) in raw.env {
        // The `$`-in-a-key rule, checked here so it runs for every backend,
        // not just the native merged spec: `scm: {env: {"${K}": v}}` must be
        // caught from Linux too. Comment deliberately duplicated with
        // `interpolate.rs`'s copy — see `USER_ID_MESSAGE`'s note on
        // `reject_blank` for why this is not a duplicate to be collapsed.
        if interpolate::would_substitution_change(&key) {
            return Err(invalid(id, interpolate::ENV_NAME_MESSAGE));
        }
        reject_env_key_with_equals(id, &key)?;
        reject_unemittable(id, "env key", &key)?;
        reject_empty(id, "env key", &key)?;
        reject_unemittable(id, &format!("env[{key}]"), &value)?;
        env.insert(key, value);
    }

    let user = resolve_user(raw.user);
    match &user {
        User::Root => {}
        User::Name(n) => {
            reject_unemittable(id, "user.name", n)?;
            reject_blank(id, "user.name", n)?;
        }
        User::Id(AccountId::Sid(s)) => {
            // The `user.id` `$` rule, checked here for the same reason as
            // the `env`-name one above. Ordering against `reject_blank` below
            // does not matter: `${S}` trims to `${S}`, never empty, so the
            // two can never both fire on one value.
            if interpolate::would_substitution_change(s) {
                return Err(invalid(id, interpolate::USER_ID_MESSAGE));
            }
            reject_unemittable(id, "user.id", s)?;
            reject_blank(id, "user.id", s)?;
        }
        User::Id(AccountId::Uid(_)) => {}
    }

    let restart = shape(id, raw.restart, parse_restart)?;
    let restart_delay = shape(id, raw.restart_delay, parse_restart_delay)?;
    let kind = shape(id, raw.kind, parse_kind)?;

    // The absoluteness guarantee has exactly one other runtime enforcement
    // point, inside `blob::decode` — which only fires when re-reading an
    // artifact that was already written. On the direct resolve -> generate
    // path that `install`/`show` take, a regression here would be baked into
    // a unit file before anything noticed. Catch it at the source instead.
    // The `command[0]` half of this guarantee moved to `require_complete`,
    // the first point at which a `command` is known to exist at all.
    debug_assert!(
        cwd.as_deref().is_none_or(Path::is_absolute) && logs.as_deref().is_none_or(Path::is_absolute),
        "resolve_shape must return absolute paths; got cwd={cwd:?} logs={logs:?}",
    );

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

/// Keep the parse in the shape phase, and skip exactly the values a
/// substitution would change. Not `contains("${")`: `would_substitution_change`
/// is deliberately `contains('$')`, so a `$$` escape is deferred too, even
/// though it needed no `.env` — see that function's doc comment. Deferring
/// a value that didn't strictly need deferring costs one check running
/// slightly later; parsing text that substitution is about to change
/// parses the wrong string outright.
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
fn require_complete(shaped: ShapedSpec, backend: Option<Backend>) -> Result<DaemonSpec, Error> {
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

    // Both "absent" and "present but empty" go through the one
    // `reject_empty_command` check, so the two spellings of "no command"
    // cannot diverge and produce one message each.
    let command = command.unwrap_or_default();
    reject_empty_command(&id, &command).map_err(|_| missing_command_error(&id, backend))?;

    // The `command[0]` half of the absoluteness guarantee — see
    // `resolve_shape`'s matching comment. This is the first point at which
    // a `command` is known to exist; indexing `command[0]` any earlier is
    // exactly the assumption `Option<Vec<String>>` exists to remove.
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

    Ok(DaemonSpec {
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

fn missing_command_error(id: &Id, backend: Option<Backend>) -> Error {
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
/// The rule is the bare string form's alone: `user: root` is the
/// superuser, while `{name: root}` is the literal account called `root`,
/// which is that form's whole purpose. Applying it here rather than in
/// `RawUser`'s visitor is what lets `user: ${U}` with `U=root` mean
/// exactly what a typed `user: root` means, without `{name: ${U}}` also
/// collapsing to the superuser — a `User::Name` carries no record of
/// which syntax produced it, so a post-substitution re-normalisation
/// cannot tell the two apart. Same reasoning as [`parse_restart`], on the
/// one field whose *deserialization* was value-dependent.
pub(crate) fn resolve_user(raw: Option<RawUser>) -> User {
    match raw {
        None => User::Root,
        Some(RawUser::Scalar(s)) if s == "root" => User::Root,
        Some(RawUser::Scalar(s)) => User::Name(s),
        Some(RawUser::Name(s)) => User::Name(s),
        Some(RawUser::Id(id)) => User::Id(id),
    }
}

/// Parse an authored `restart:` string into a [`Restart`]. `pub(crate)`
/// rather than private: the backend-specific-override item's two-phase
/// resolution needs to call this from whichever phase can honestly run it,
/// once a field can also arrive from a per-backend override rather than
/// only from here.
///
/// A `String` field parsed here, rather than a derived `Deserialize` on
/// `Restart` itself, is what lets `restart: ${R}` be interpolated before
/// this ever runs — a derived `Deserialize` would reject `${R}` as an
/// unknown variant at YAML-parse time, before interpolation gets a chance.
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

/// Parse an authored `restart-delay:` string into a [`Duration`], replacing
/// `humantime_serde`'s serde-time parse (accepted syntax is unchanged:
/// `humantime_serde` was itself a thin wrapper over
/// `humantime::parse_duration`). `pub(crate)`: see [`parse_restart`].
pub(crate) fn parse_restart_delay(id: &Id, raw: &str) -> Result<Duration, Error> {
    humantime::parse_duration(raw).map_err(|source| {
        invalid(
            id,
            &format!("field `restart-delay` is `{raw}`, which is not a duration (e.g. `30s`, `1m 30s`): {source}"),
        )
    })
}

fn resolve_optional_path(
    id: &Id,
    field: &str,
    raw: Option<String>,
    base_dir: &Path,
    phase: Phase,
    warnings: &mut Vec<Warning>,
) -> Result<Option<PathBuf>, Error> {
    match raw {
        Some(s) => {
            reject_unemittable(id, field, &s)?;
            reject_empty(id, field, &s)?;
            Ok(Some(PathBuf::from(resolve_path_string(
                id, field, &s, base_dir, phase, warnings,
            )?)))
        }
        None => Ok(None),
    }
}

/// Join a relative path against `base_dir`; leave an absolute path as-is.
/// `Path::is_absolute` (not a string prefix check) so this behaves
/// correctly on both a POSIX host (`/opt/rt`) and a Windows host
/// (`C:\base`), whose absoluteness rules differ.
///
/// Every check here is deterministic given `(value, base_dir, process
/// state)` — `std::path::absolute`'s drive-relative fallback reads the
/// per-drive current directory, so this is not a pure function of `(value,
/// base_dir)` alone — but every pass `resolve` runs per daemon runs in one
/// process against one unchanging process state, so identical values get
/// identical verdicts across passes. That is what makes "the base pass
/// passed, therefore this failure is the override's" sound.
fn resolve_path_string(
    id: &Id,
    field: &str,
    raw: &str,
    base_dir: &Path,
    phase: Phase,
    warnings: &mut Vec<Warning>,
) -> Result<String, Error> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        normalize(&base_dir.join(p))
    };
    // `base_dir` is absolute, so a join can fail to yield an absolute result
    // only in one shape: a Windows drive-relative path (`C:bin/frpc.exe`),
    // which carries a prefix but no root. `PathBuf::push` replaces the
    // buffer whenever the pushed path carries a prefix, so
    // `base_dir.join("C:bin")` silently discards `base_dir` and yields
    // `C:bin` — still relative. Every currently-accepted input resolves
    // above and never reaches here.
    let joined = if joined.is_absolute() {
        joined
    } else {
        // `std::path::absolute` — not the still-unstable `Path::absolute`
        // method, and not `fs::canonicalize`, which requires the path to
        // exist, resolves symlinks, and returns a verbatim `\\?\` path on
        // Windows — resolves the drive-relative case the way
        // `GetFullPathNameW` does: against the per-drive current directory
        // of *this* process.
        let resolved = std::path::absolute(&joined).map_err(|source| {
            invalid(
                id,
                &format!("field `{field}` is `{raw}`, which cannot be resolved to an absolute path: {source}"),
            )
        })?;
        if resolved.is_absolute() {
            // `GetFullPathNameW` resolves against process state — the
            // current directory on that drive in whichever process ran
            // goetia — not against the manifest. An `install` run from one
            // directory and a `diff` run from another can resolve this
            // differently and disagree, reporting drift on an artifact
            // nobody touched.
            //
            // `Phase::Authored` runs on uninterpolated text, so a base path
            // written as a `${VAR}` template is suppressed here: two of the
            // three passes that keep warnings (the two non-native sweep
            // passes) would otherwise each emit an identical warning
            // quoting the raw template, and dedup cannot merge that with
            // the differently-worded one `Phase::Substituted` produces once
            // the template resolves. Four cases, all Windows-only (this
            // advisory only ever fires where `std::path::absolute` leaves a
            // path non-absolute — a Windows prefix-without-root property):
            // a base literal collapses three identical warnings to one; a
            // base template warns once, from the native pass, quoting the
            // substituted spelling; an `scm:`-only literal warns once, from
            // the native pass; a `systemd:`-only template warns zero times
            // — unavailable, not wrong. All four are Windows-native cases,
            // where `systemd` is a backend nothing substitutes for: the
            // same line as a non-native `{user: "${ACCT}"}` override, in
            // `spec::backend`'s module doc.
            let should_warn = match phase {
                Phase::Substituted => true,
                Phase::Authored => !interpolate::would_substitution_change(raw),
            };
            if should_warn {
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
        }
        resolved
    };
    // Assert the post-condition rather than assume the fallback achieved it.
    // Provably unreachable under every current `Prefix` variant, not merely
    // untested: `std`'s own `Prefix::has_implicit_root` is `true` for every
    // prefix except `Disk`, so a verbatim prefix (`Verbatim`, `VerbatimDisk`,
    // `VerbatimUNC`) is already absolute before `p.is_absolute()` above ever
    // reaches this fallback — `\\?\C:` included, despite reading as rootless.
    // The one prefix that genuinely lacks a root, plain `Disk` (`C:bin`), is
    // not verbatim, so `std::path::absolute` never returns it unchanged; it
    // always resolves it via `GetFullPathNameW` into an absolute path (the
    // drive-relative branch above). No known input reaches here; the check
    // stays as a defensive invariant in case a future `std` changes that
    // mapping.
    if !joined.is_absolute() {
        return Err(invalid(
            id,
            &format!("field `{field}` is `{raw}`, which cannot be resolved to an absolute path"),
        ));
    }
    Ok(joined.to_string_lossy().into_owned())
}

/// Make `dir` absolute against the process's working directory.
///
/// **Deduced values may be canonicalised; authored ones are honoured as
/// written.** `dir` is the manifest directory the user passed to `-f`, so it
/// is deliberately not `canonicalize()`d: that requires the path to exist
/// (a manifest may name a `cwd`/`logs` directory the installer is about to
/// create) and resolves symlinks, which would bake a deliberately
/// symlinked deployment path's target into the artifact instead of the path
/// the user wrote, so a service would stop following a repointed symlink.
/// One consequence: `-f .` cannot preserve a symlinked spelling — `getcwd`
/// already resolved it before goetia ever saw it, so there is nothing left
/// to honour. Anyone who wants a symlinked deployment path kept must pass it
/// explicitly to `-f`.
///
/// The process's own working directory, read below when `dir` is relative,
/// is the opposite case: nobody typed it, so it carries no intent to
/// preserve, and it always exists, so it *is* canonicalised.
fn absolutize(dir: &Path, warnings: &mut Vec<Warning>) -> Result<PathBuf, Error> {
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
    let cwd = canonicalize_cwd(&cwd)?;
    let joined = normalize(&cwd.join(dir));
    // The fallback below resolves `..` lexically, on this path only.
    // `normalize` leaves `..` alone everywhere else — removing it lexically
    // is wrong in the presence of symlinks — but `GetFullPathNameW`
    // collapses `..` lexically before the filesystem ever sees the path, so
    // a drive-relative path goetia resolved differently from every other
    // Windows tool would be the bug, not the other way around.
    let joined = if joined.is_absolute() {
        joined
    } else {
        let resolved = std::path::absolute(&joined).map_err(|source| {
            Error::Other(format!(
                "manifest directory `{}` cannot be resolved to an absolute path: {source}",
                dir.display()
            ))
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
    if !joined.is_absolute() {
        return Err(Error::Other(format!(
            "manifest directory `{}` cannot be resolved to an absolute path",
            dir.display()
        )));
    }
    Ok(joined)
}

/// Canonicalise the process's current working directory. **Deduced values
/// may be canonicalised; authored ones are honoured as written** — see
/// [`absolutize`]'s doc comment for the other half of that line. The
/// working directory is the one value in the chain nobody typed, so it
/// carries no intent to preserve, and it always exists, so
/// `canonicalize`'s existence requirement is satisfied by construction.
///
/// What this buys, per platform: on Unix, nearly nothing — `getcwd`
/// already returns a symlink-free path. On Windows it gains canonical
/// capitalisation, which fixes phantom drift at its source: installing from
/// `C:\App` and diffing from `C:\app` would otherwise store two different
/// strings for one directory and report drift on an artifact nobody
/// touched.
///
/// When this fails, every subcommand that loads a manifest by a relative
/// `-f` — including the default `-f .` — fails from that directory. That is
/// an **accepted limitation**, decided by the owner: the reachable causes
/// are narrow, and a fallback to the uncanonicalised directory would trade a
/// loud refusal for silent phantom drift on exactly the platform this
/// function exists to protect. Revisit if one of them shows up in use.
///
/// The reachable causes, measured rather than assumed:
///
/// - **Unix:** a path component the process cannot traverse (`EACCES`) —
///   the cwd is under a directory whose permissions changed after the
///   process entered it, or which it inherited and cannot re-walk.
/// - **Windows:** `canonicalize` is `CreateFileW` with
///   `FILE_FLAG_BACKUP_SEMANTICS` followed by `GetFinalPathNameByHandleW`
///   with `VOLUME_NAME_DOS`, so it fails when the open is refused (access
///   denied, sharing violation) *and* when the volume has no drive letter or
///   mount point for a DOS path to name — a volume mounted only under a GUID
///   path, or a VSS snapshot. Filesystem drivers that do not implement the
///   information classes that call needs fail here too.
///
/// A **deleted** working directory is not among them, despite being the
/// intuitive guess: it fails earlier, in `std::env::current_dir`, and never
/// reaches this function. Do not put it back in the message below.
fn canonicalize_cwd(cwd: &Path) -> Result<PathBuf, Error> {
    let canonical = fs::canonicalize(cwd).map_err(|source| {
        Error::Other(format!(
            "canonicalising the current directory {} failed; it may be unreadable, or on a volume \
             with no drive letter or mount point: {source}. Pass `-f` with an absolute path to \
             skip resolving it",
            cwd.display()
        ))
    })?;
    Ok(strip_verbatim_prefix(canonical))
}

/// Strip Windows' `\\?\` verbatim prefix from a `canonicalize`d path, so it
/// compares equal to every other spelling goetia or SCM produces for the
/// same directory. `canonicalize` only ever returns the two shapes matched
/// below; any other prefix is left untouched. Stripped unconditionally, even
/// past `MAX_PATH` (260): keeping the verbatim prefix instead would put a
/// `\\?\` string into the metadata blob — a spelling nothing else in the
/// system produces — guaranteeing permanent phantom drift, which is worse
/// than the length limit that already applies to every authored absolute
/// path. A no-op on every other platform, where `canonicalize` never
/// produces a `Prefix` component to begin with.
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
            // Not `UNC\server\share` — building the drive form by simply
            // dropping the leading `\\?\` would produce exactly that trap
            // for this variant.
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

/// Fields where the empty string is never meaningful and is silently
/// dangerous. `pub(crate)`: also called by `blob::decode`.
///
/// Applied to `command[0]`, `cwd` when present, `logs` when present, and
/// every `env` key — an empty one of these has no dangerous default, it
/// simply fails to resolve or, for `command[0]`, would silently resolve to
/// the manifest directory. `user.name` and `user.id` are *not* checked
/// here: an account identifier needs the stronger [`reject_blank`], since
/// a value that is empty only after trimming is exactly as dangerous as
/// one that is empty outright. Deliberately **not** applied to `name` (an
/// empty systemd `Description=` is harmless), to `env` values (`FOO=` is a
/// normal assignment), or to `command[1..]` (an empty argv element is
/// legitimate on POSIX and every generator quotes it).
pub(crate) fn reject_empty(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(invalid(id, &format!("field `{field}` must not be empty")));
    }
    Ok(())
}

/// Like [`reject_empty`], but also rejects a value that is nothing but
/// whitespace. Applied only to `user.name` and `user.id` (the `Sid` arm) —
/// never to a path or an `env` key, and that asymmetry is deliberate, not
/// an oversight to "complete":
///
/// - An account identifier has a dangerous default on every backend if the
///   whitespace is trimmed away downstream: `User=` (a bare space or
///   several) verifies and starts cleanly under `systemd-analyze verify`
///   on every platform tested, exactly like `User=` with nothing after
///   it — systemd does not reject either, so goetia is the only gate. A
///   whitespace-only value is therefore either the same reset-to-root
///   silently reached through a value [`reject_empty`] does not catch, or
///   it names an account that cannot exist (a loud start failure) — both
///   outcomes this task exists to close off.
/// - A path has no such default: a file literally named `" "` is legal on
///   Unix, so refusing it would be defending goetia's own implementation
///   rather than serving the user, and an all-whitespace `cwd`/`logs`/
///   `command[0]` simply fails to resolve like any other bad path — it
///   does not fall back to anything.
/// - An `env` key made of whitespace is a distinct, broader question
///   (whether an environment-variable name may contain a space at all)
///   that this task does not own; only its *emptiness* does, via
///   [`reject_empty`].
///
/// `pub(crate)`: also called by `blob::decode`.
///
/// An already-installed artifact whose blob carries an empty or
/// whitespace-only `user.name` now decodes as `Installed::OursUnreadable`
/// rather than silently as root — the correct disclosure, and goetia has
/// no release yet whose compatibility that would break.
///
/// A second, independent rejection of an empty `user.name` and empty
/// `user.id` SID lives in the per-backend-override validation
/// (`resolve_shape`): it found that `canonical_account("")` maps an empty
/// authored name to `LocalSystem` on Windows by a different route, and
/// stays scoped to the `User::Root` path it was written for. The two
/// checks have different reachability and neither is redundant with the
/// other — do not remove one as a "duplicate" of the other.
pub(crate) fn reject_blank(id: &Id, field: &str, value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(invalid(id, &format!("field `{field}` must not be empty")));
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

/// `pub(super)`: `spec::backend` (a sibling module, not a descendant of
/// `resolve`) constructs the same `Error::Invalid` shape for its own
/// rejections (`Backend::error`), and this is the one place that shape is
/// spelled.
pub(super) fn invalid(id: &Id, message: &str) -> Error {
    Error::Invalid {
        daemon: id.as_str().to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod resolve_tests;
