//! The `backend-specific:` manifest key's namespace: which service-manager
//! backend a set of overrides applies to. For the manifest-facing
//! reference to `backend-specific:` itself, see `overrides`'s module doc;
//! this module is the validation machinery it drives.
//!
//! Lives under `spec` rather than `crate::backend` because its job is
//! parsing a manifest key; `crate::backend` turns a resolved
//! [`crate::spec::DaemonSpec`] into a unit, plist or registration.
//!
//! Also home to the per-backend cross-validation `resolve` runs over every
//! backend's merged spec: [`ShapedSpec`]/[`Shaped`] (a spec that
//! has passed the shape gate but may not yet be complete),
//! [`BackendKind::warn`]/[`BackendKind::error`] (the advisory and rejection
//! rules), and the Windows built-in account vocabulary (`windows_builtin`)
//! both read.
//!
//! A host validates only what the manifest wrote under a backend's own
//! `backend-specific:` key, and warns only about its native backend.
//!
//! [`BackendKind::error`] rejects only a field the override actually supplied
//! (`Supplied`, from `overrides.rs`): a base value is never a per-backend
//! verdict, since nobody wrote it for that backend. It runs for every
//! backend on every host, on the override as authored: `scm: {user: {id:
//! 1000}}` is caught from any host, while `scm: {user: "${ACCT}"}` is not
//! checkable from Linux because only the native backend's override is
//! substituted. The native override is checked and warned on after
//! substitution, in `resolve`'s step 5.
//!
//! [`BackendKind::warn`] runs for the native backend only, on its substituted
//! merged spec, base values included: an advisory describes what the native
//! install does with the value it reads. A base value the native backend cannot
//! honour is its generator's to refuse at install (`restart: always` on `scm`).

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
/// variants in declaration order, and [`BackendKind::ALL`] (and so warning
/// order) follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Launchd,
    Scm,
    Systemd,
}

impl BackendKind {
    /// Every variant, in declaration order.
    pub const ALL: [BackendKind; 3] = [BackendKind::Launchd, BackendKind::Scm, BackendKind::Systemd];

    /// The backend for the platform this binary is running on, or `None` on
    /// a platform with no service manager Goetia supports.
    ///
    /// Must agree with [`crate::manager::native`]'s supported set — see
    /// `native_backend_agrees_with_the_platforms_that_have_a_service_manager`
    /// in `backend_tests.rs`.
    pub const fn native() -> Option<BackendKind> {
        #[cfg(target_os = "linux")]
        {
            Some(BackendKind::Systemd)
        }
        #[cfg(target_os = "macos")]
        {
            Some(BackendKind::Launchd)
        }
        #[cfg(target_os = "windows")]
        {
            Some(BackendKind::Scm)
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
            BackendKind::Launchd => "launchd",
            BackendKind::Scm => "scm",
            BackendKind::Systemd => "systemd",
        }
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ShapedSpec ==========================================================================================================

/// One of the three fields whose authored text is an enum or a duration
/// that `resolve` parses. Three states, because the shape pass runs
/// before substitution.
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

impl BackendKind {
    /// Advisories this backend has about `spec`, appended to `out`. Called
    /// for the native backend only, on its substituted merged spec, and reads
    /// the whole of it: an advisory describes what the native install does
    /// with a value, whichever layer wrote it.
    pub(crate) fn warn(self, spec: &ShapedSpec, out: &mut Vec<Warning>) {
        match self {
            BackendKind::Systemd => {}
            BackendKind::Launchd => warn_on_sub_second_restart_delay(spec, out),
            BackendKind::Scm => {
                warn_on_windows_divergences(spec, out);
                warn_on_over_long_restart_delay(spec, out);
            }
        }
    }

    /// Values that are wrong for this backend by construction, on any
    /// host. Reads **only** the fields `supplied` marks. See `Supplied`.
    pub(crate) fn error(self, spec: &ShapedSpec, supplied: Supplied) -> Result<(), Error> {
        if !supplied.user {
            return Ok(());
        }
        match self {
            BackendKind::Scm => match &spec.user {
                // A numeric uid is never a Windows account.
                User::Id(AccountId::Uid(uid)) => Err(invalid(
                    &spec.id,
                    &format!("`{uid}` is a numeric uid, which is never a Windows account"),
                )),
                _ => Ok(()),
            },
            BackendKind::Systemd | BackendKind::Launchd => match &spec.user {
                User::Id(AccountId::Sid(sid)) => Err(invalid(
                    &spec.id,
                    &format!(
                        "`{sid}` is a Windows SID, which is never a POSIX account; if you meant a numeric uid, \
                         write it unquoted, e.g. `user: {{id: 1000}}`"
                    ),
                )),
                // A `$`-bearing name cannot be classified before substitution; do not guess.
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
/// field, so `cwd`/`logs` are accepted, not rejected, but silently
/// unavailable there.
///
/// Reads `kind` through [`Shaped::parsed`], whose `None` covers both an absent value and a
/// `Deferred` one: silence on an unavailable value is the contract. An absent `type` defaults
/// to `Simple`, which warns about nothing.
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
}

/// `restart-delay` is stored as authored so the metadata blob stays
/// deterministic across platforms, but launchd's `ThrottleInterval` is
/// integer seconds: `500ms` would truncate to `0`, which *disables*
/// throttling and yields an unbounded respawn storm. Warn that it rounds up.
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

/// `SC_ACTION.Delay` is a `DWORD` of milliseconds, and `windows-service`'s
/// `ServiceAction::to_raw` **panics** on a longer `Duration`. `restart-delay`
/// has no bound of its own, so `backend::scm::generate::registration` clamps
/// to this. Defined here so the advisory below and the clamp share one
/// constant; `backend::scm` depends on `spec`, never the reverse.
pub(crate) const MAX_SC_ACTION_DELAY: Duration = Duration::from_millis(u32::MAX as u64);

/// What SCM stores for an authored `restart-delay`: whole milliseconds, at most
/// [`MAX_SC_ACTION_DELAY`]. A finer delay rounds up, as launchd's `ThrottleInterval`
/// does to whole seconds, so what `generate::registration` renders is what a read-back
/// of the stored value renders. A delay over the maximum is clamped to it.
pub(crate) fn scm_restart_delay(delay: Duration) -> Duration {
    let delay = delay.min(MAX_SC_ACTION_DELAY);
    let sub_ms = Duration::from_nanos(u64::from(delay.subsec_nanos() % 1_000_000));
    if sub_ms.is_zero() {
        delay
    } else {
        // The maximum is a whole number of milliseconds, so a delay with a remainder is below it.
        delay - sub_ms + Duration::from_millis(1)
    }
}

/// The advisories for `generate::registration`'s restart-delay clamp and rounding, fired at
/// resolve time rather than only when an install runs the generator. Gated like the clamp:
/// `Restart::Never` builds no `FailureActions`, and the generator refuses `always`, so only
/// `on-failure` warns.
fn warn_on_over_long_restart_delay(spec: &ShapedSpec, out: &mut Vec<Warning>) {
    if spec.restart.parsed() != Some(Restart::OnFailure) {
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
        return;
    }
    let stored = scm_restart_delay(delay);
    if stored != delay {
        out.push(Warning {
            id: Some(spec.id.clone()),
            message: format!(
                "restart-delay {delay:?} is not a whole number of milliseconds; SCM stores whole \
                 milliseconds, so it will be rounded up to {stored:?}"
            ),
        });
    }
}

// Windows built-in accounts ===========================================================================================

/// A Windows built-in service account, recognised from a name a manifest
/// author typed. `None` is an ordinary account name. Read by
/// `backend::scm::generate::canonical_account`, which must recognise every
/// spelling Windows accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    LocalSystem,
    LocalService,
    NetworkService,
}

impl Builtin {
    /// The spelling `CreateServiceW` accepts. `None` for `LocalSystem`,
    /// which is `ServiceInfo`'s default and is expressed by absence.
    /// Read by `backend::scm::generate::canonical_account`.
    pub(crate) const fn canonical(self) -> Option<&'static str> {
        match self {
            Builtin::LocalSystem => None,
            Builtin::LocalService => Some(r"NT AUTHORITY\LocalService"),
            Builtin::NetworkService => Some(r"NT AUTHORITY\NetworkService"),
        }
    }
}

/// Recognise a Windows built-in account by every spelling Windows accepts:
/// bare (`LocalSystem`, `LocalService`, `NetworkService`, `SYSTEM`) or
/// `NT AUTHORITY\`-qualified, case-insensitively, plus the two spaced
/// spellings `LookupAccountSidW` returns for the well-known SIDs
/// (`NT AUTHORITY\LOCAL SERVICE`, `NT AUTHORITY\NETWORK SERVICE`) and the
/// empty string (below).
///
/// Enumerated rather than folded: stripping spaces would also accept
/// `NT AUTHORITY\S Y S T E M`, silently promoting a typo to a privileged
/// account. An unprefixed `Local Service` never comes from a SID lookup and
/// is a legal local account name, so it stays ordinary.
///
/// `windows_builtin("")` is `Some(Builtin::LocalSystem)`: `canonical_account`
/// maps an empty authored account to `LocalSystem`, and both halves must
/// agree. `BackendKind::error` never sees an empty name (`reject_blank` fails
/// first in `resolve_shape`), but do not remove this row, or `User::Root`'s
/// behaviour on Windows changes.
pub(crate) fn windows_builtin(name: &str) -> Option<Builtin> {
    match name.to_ascii_lowercase().as_str() {
        "" | "system" | "localsystem" | r"nt authority\system" | r"nt authority\localsystem" => {
            Some(Builtin::LocalSystem)
        }
        "localservice" | r"nt authority\localservice" | r"nt authority\local service" => Some(Builtin::LocalService),
        "networkservice" | r"nt authority\networkservice" | r"nt authority\network service" => {
            Some(Builtin::NetworkService)
        }
        _ => None,
    }
}

/// [`windows_builtin`], narrowed to the spellings that can name *only* a
/// Windows account — the predicate `BackendKind::error` uses to reject a
/// Windows built-in under a POSIX backend. Defined in terms of
/// `windows_builtin`, never as a second table.
///
/// A bare `system` (any case) is carved out: Windows resolves it to
/// `LocalSystem`, but it is also an ordinary POSIX username, so rejecting it
/// under `systemd`/`launchd` would make that account inexpressible.
/// `NT AUTHORITY\SYSTEM` is untouched. `windows_only_account("")` is
/// `Some(LocalSystem)` but unreachable, as for `windows_builtin("")`.
pub(crate) fn windows_only_account(name: &str) -> Option<Builtin> {
    windows_builtin(name).filter(|_| !name.eq_ignore_ascii_case("system"))
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;
