//! The `backend-specific:` manifest key's namespace: which service-manager
//! backend a set of overrides applies to.
//!
//! Lives under `spec` rather than `crate::backend` because its job is
//! parsing a manifest key, not artifact generation — `crate::backend` is
//! the tree that turns a resolved [`crate::spec::DaemonSpec`] into a real
//! systemd unit, launchd plist, or SCM registration.
//!
//! Also home to the per-backend cross-validation `resolve` runs over every
//! backend's merged spec (Task 5): [`ShapedSpec`]/[`Shaped`] (a spec that
//! has passed the shape gate but may not yet be complete),
//! [`Backend::warn`]/[`Backend::error`] (the advisory and rejection rules),
//! and the Windows built-in account vocabulary (`windows_builtin`) both
//! read.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::interpolate;
use super::overrides::Supplied;
use super::resolve::invalid;
use super::{AccountId, Id, Kind, Restart, User, Warning};
use crate::error::Error;

/// One of the three service-manager backends a `backend-specific:` entry
/// can target.
///
/// Declared in alphabetical order: serde's `unknown_variant` error lists
/// variants in declaration order, and this order also fixes the order
/// [`Backend::ALL`] iterates in, which in turn fixes the order any warning
/// keyed off it is emitted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Launchd,
    Scm,
    Systemd,
}

impl Backend {
    /// Every variant, in the same alphabetical order they are declared in.
    pub const ALL: [Backend; 3] = [Backend::Launchd, Backend::Scm, Backend::Systemd];

    /// The backend for the platform this binary is running on, or `None` on
    /// a platform with no service manager Goetia supports.
    ///
    /// Must agree with [`crate::manager::native`]'s supported set — see
    /// `native_backend_agrees_with_the_platforms_that_have_a_service_manager`
    /// in `backend_tests.rs`.
    pub const fn native() -> Option<Backend> {
        #[cfg(target_os = "linux")]
        {
            Some(Backend::Systemd)
        }
        #[cfg(target_os = "macos")]
        {
            Some(Backend::Launchd)
        }
        #[cfg(target_os = "windows")]
        {
            Some(Backend::Scm)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            None
        }
    }

    /// The manifest key's spelling for this variant: `systemd`, `launchd`,
    /// `scm`. Round-trips through [`Deserialize`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Launchd => "launchd",
            Backend::Scm => "scm",
            Backend::Systemd => "systemd",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ShapedSpec ==========================================================================================================

/// One of the three fields whose authored text is an enum or a duration
/// that `resolve` parses. Three states, because a shape pass that runs
/// before substitution can honestly reach only two of them.
#[derive(Debug)]
pub(crate) enum Shaped<T> {
    /// The authored text carried no `$` and parsed.
    Parsed(T),
    /// The authored text carries a `$`, so substitution would change it
    /// and parsing it now would parse the wrong string. Held verbatim
    /// until the completion phase, which sees the substituted text.
    Deferred(String),
    /// The manifest did not set the field. The default is applied in the
    /// completion phase, in one place.
    Absent,
}

impl<T: Copy> Shaped<T> {
    /// The parsed value, or `None` for "absent, or not knowable on this
    /// host". Advisories read the spec through this: an unavailable check
    /// is silent, never wrong.
    pub(crate) fn parsed(&self) -> Option<T> {
        match self {
            Shaped::Parsed(value) => Some(*value),
            Shaped::Deferred(_) | Shaped::Absent => None,
        }
    }
}

/// A spec whose every value has passed the shape gate, but which may
/// still be missing a required field and whose `$`-bearing enum/duration
/// fields are still unparsed. Only the backend being installed is
/// required to be complete.
#[derive(Debug)]
pub(crate) struct ShapedSpec {
    pub(crate) id: Id,
    pub(crate) name: String,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) user: User,
    pub(crate) restart: Shaped<Restart>,
    pub(crate) restart_delay: Shaped<Duration>,
    pub(crate) logs: Option<PathBuf>,
    pub(crate) kind: Shaped<Kind>,
}

// Advisories and rejections ===========================================================================================

impl Backend {
    /// Advisories this backend has about `spec`, appended to `out`.
    /// Reads the whole merged spec: an advisory is information this host
    /// may give about any value it can read, on any backend, regardless of
    /// which one wrote it.
    pub(crate) fn warn(self, spec: &ShapedSpec, out: &mut Vec<Warning>) {
        match self {
            Backend::Systemd => {}
            Backend::Launchd => warn_on_sub_second_restart_delay(spec, out),
            Backend::Scm => {
                warn_on_windows_divergences(spec, out);
                warn_on_over_long_restart_delay(spec, out);
            }
        }
    }

    /// Values that are wrong for this backend by construction, on any
    /// host. Reads **only** the fields `supplied` marks: a rejection is a
    /// verdict this host is entitled to make about a value written for
    /// this backend, and about nothing else. See Task 3's `Supplied`.
    pub(crate) fn error(self, spec: &ShapedSpec, supplied: Supplied) -> Result<(), Error> {
        if !supplied.user {
            return Ok(());
        }
        match self {
            Backend::Scm => match &spec.user {
                // A numeric uid is never a Windows account.
                User::Id(AccountId::Uid(uid)) => Err(invalid(
                    &spec.id,
                    &format!("`{uid}` is a numeric uid, which is never a Windows account"),
                )),
                _ => Ok(()),
            },
            Backend::Systemd | Backend::Launchd => match &spec.user {
                // Neither a SID nor a Windows built-in name is ever a POSIX
                // account.
                User::Id(AccountId::Sid(sid)) => Err(invalid(
                    &spec.id,
                    &format!(
                        "`{sid}` is a Windows SID, which is never a POSIX account; if you meant a numeric uid, \
                         write it unquoted, e.g. `user: {{id: 1000}}`"
                    ),
                )),
                // A `$`-bearing name cannot be classified as a built-in
                // from this host, so `error` returns `Ok` rather than
                // guessing — same predicate, same reasoning as the shape
                // phase's enum/duration deferral.
                User::Name(name)
                    if !interpolate::would_substitution_change(name) && windows_only_account(name).is_some() =>
                {
                    Err(invalid(
                        &spec.id,
                        &format!("`{name}` is a Windows built-in account, which is never a POSIX one"),
                    ))
                }
                _ => Ok(()),
            },
        }
    }
}

/// `type: managed` on Windows has no working-directory or stdout-capture
/// field, and SCM recovery actions never fire after a clean exit — so
/// `cwd`/`logs` and `restart: always` are accepted, not rejected, but
/// silently unavailable there. See the design spec's accepted divergences
/// (a) and (b).
///
/// Reads `kind`/`restart` through [`Shaped::parsed`], and the two fields
/// get different treatment: `kind == None` (absent, or unavailable) means
/// silence is correct either way, since an absent `type` defaults to
/// `Simple`, which warns about nothing. `restart == None` must **not**
/// blanket-suppress the `cwd`/`logs` warning below it — an absent
/// `restart` defaults to `Never`, which only ever suppressed the *second*
/// warning, so silence about the first would be a deletion. See
/// `managed_on_windows_warns_for_cwd_and_logs`/
/// `managed_on_windows_warning_names_the_argument_consequence`
/// (`resolve_tests.rs`), which set no `restart:` and still expect exactly
/// one warning.
fn warn_on_windows_divergences(spec: &ShapedSpec, out: &mut Vec<Warning>) {
    if spec.kind.parsed() != Some(Kind::Managed) {
        return;
    }
    if spec.cwd.is_some() || spec.logs.is_some() {
        out.push(Warning {
            id: Some(spec.id.clone()),
            message: "type: managed has no working-directory or stdout-capture field on Windows SCM; \
                      `cwd`/`logs` are silently unavailable there, and with no working directory, \
                      every relative path in an argument resolves against System32"
                .to_string(),
        });
    }
    if spec.restart.parsed() == Some(Restart::Always) {
        out.push(Warning {
            id: Some(spec.id.clone()),
            message: "restart: always is not faithfully expressible for type: managed on Windows: SCM \
                      recovery actions only fire on failure, never after a clean exit"
                .to_string(),
        });
    }
}

/// `restart-delay` is stored as authored so the metadata blob stays
/// deterministic across platforms, but launchd's `ThrottleInterval` is
/// integer seconds: `500ms` would truncate to `0`, which *disables*
/// throttling and yields an unbounded respawn storm. Warn here so the
/// rounding is not a silent surprise at install time.
fn warn_on_sub_second_restart_delay(spec: &ShapedSpec, out: &mut Vec<Warning>) {
    let Some(delay) = spec.restart_delay.parsed() else {
        return;
    };
    if delay.subsec_nanos() == 0 {
        return;
    }
    let rounded = delay.as_secs().saturating_add(1);
    out.push(Warning {
        id: Some(spec.id.clone()),
        message: format!(
            "restart-delay {delay:?} is not a whole number of seconds; launchd's ThrottleInterval \
             will round it up to {rounded}s"
        ),
    });
}

/// `SC_ACTION.Delay` is `dwDelay`, a `DWORD` of milliseconds: `windows-service`'s
/// `ServiceAction::to_raw` converts a `Duration` into one with
/// `u32::try_from(delay.as_millis()).expect("Too long delay")`, which
/// **panics** — not a recoverable `Error` — for anything past this.
/// `goetia.yaml`'s `restart-delay` has no upper bound of its own, so
/// `backend::scm::generate::registration` clamps to this rather than
/// letting that panic reach an install. Defined here, not in `generate.rs`,
/// so the advisory below and the clamp read one constant instead of two
/// that can drift; `backend::scm` depends on `spec`, never the reverse.
pub(crate) const MAX_SC_ACTION_DELAY: Duration = Duration::from_millis(u32::MAX as u64);

/// The SCM clamp advisory, moved here from `backend::scm::generate` so it
/// fires at resolve time on every host — through `Backend::Scm.warn` and
/// `resolve`'s dedup — rather than only when an effectful Windows install
/// happens to run the generator. The clamping itself stays in `generate.rs`
/// (generation, not validation); only the warning moved.
///
/// Reproduces both of the generator's original gates, which the plan that
/// first proposed this move omitted: `Kind::Simple` builds no
/// `FailureActions` at all, and `Restart::Never` builds none either, so a
/// `restart-delay` with neither `type: managed` nor `restart: on-failure`/
/// `always` warns about nothing.
fn warn_on_over_long_restart_delay(spec: &ShapedSpec, out: &mut Vec<Warning>) {
    if spec.kind.parsed() != Some(Kind::Managed) {
        return;
    }
    if !matches!(spec.restart.parsed(), Some(Restart::OnFailure) | Some(Restart::Always)) {
        return;
    }
    let Some(delay) = spec.restart_delay.parsed() else {
        return;
    };
    if delay > MAX_SC_ACTION_DELAY {
        out.push(Warning {
            id: Some(spec.id.clone()),
            message: format!(
                "restart-delay {delay:?} exceeds the ~49.71 days SC_ACTION.Delay (a DWORD of milliseconds) can \
                 express; clamped to {MAX_SC_ACTION_DELAY:?}"
            ),
        });
    }
}

// Windows built-in accounts ===========================================================================================

/// A Windows built-in service account, recognised from a name a manifest
/// author typed. `None` is an ordinary account name. Used for
/// canonicalisation (Task 7), where every spelling Windows accepts must
/// be recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    LocalSystem,
    LocalService,
    NetworkService,
}

impl Builtin {
    /// The spelling `CreateServiceW` accepts. `None` for `LocalSystem`,
    /// which is `ServiceInfo`'s default and is expressed by absence.
    ///
    /// Unread until Task 7's `canonical_account` lands and calls it — not a
    /// placeholder for a future concept, this task's own produced interface.
    #[allow(dead_code)]
    pub(crate) const fn canonical(self) -> Option<&'static str> {
        match self {
            Builtin::LocalSystem => None,
            Builtin::LocalService => Some(r"NT AUTHORITY\LocalService"),
            Builtin::NetworkService => Some(r"NT AUTHORITY\NetworkService"),
        }
    }
}

/// Recognise a Windows built-in account by every spelling Windows accepts:
/// bare (`LocalSystem`, `LocalService`, `NetworkService`, `SYSTEM`), or
/// `NT AUTHORITY\`-qualified, case-insensitively. Matches
/// `identity::account_needs_password`'s table exactly, plus the empty
/// string — see the note below.
///
/// `windows_builtin("")` is `Some(Builtin::LocalSystem)`: Task 7's
/// `canonical_account` maps an empty authored account to `LocalSystem`, and
/// both halves must agree. `Backend::error` never sees an empty name —
/// `reject_blank` fails first in `resolve_shape` — so this row is
/// unreachable from that direction; do not "simplify" it away, or
/// `User::Root`'s behaviour on Windows changes.
pub(crate) fn windows_builtin(name: &str) -> Option<Builtin> {
    let folded = name.to_ascii_lowercase();
    match folded.as_str() {
        "" | "system" | "localsystem" | r"nt authority\system" => Some(Builtin::LocalSystem),
        "localservice" | r"nt authority\localservice" => Some(Builtin::LocalService),
        "networkservice" | r"nt authority\networkservice" => Some(Builtin::NetworkService),
        _ => None,
    }
}

/// [`windows_builtin`], narrowed to the spellings that can name *only* a
/// Windows account — the predicate `Backend::error` uses to reject a
/// Windows built-in under a POSIX backend. Defined in terms of
/// `windows_builtin`, never as a second table.
///
/// A bare `system` (any case) is carved out: `SYSTEM` is a spelling Windows
/// resolves to the `LocalSystem` account, but `system` is also a perfectly
/// ordinary POSIX username (`useradd system` succeeds on Debian, and it is
/// a real account on Android), so rejecting it under `systemd`/`launchd`
/// would make that POSIX account inexpressible. `NT AUTHORITY\SYSTEM` is a
/// different string and is untouched by this filter — no prefix test is
/// needed or wanted. Likewise unreachable, for the same reason as
/// `windows_builtin("")`: `windows_only_account("")` is `Some(LocalSystem)`,
/// since `reject_blank` catches the empty case first. The carve-out is for
/// `system`, not for the empty string.
pub(crate) fn windows_only_account(name: &str) -> Option<Builtin> {
    windows_builtin(name).filter(|_| !name.eq_ignore_ascii_case("system"))
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;
