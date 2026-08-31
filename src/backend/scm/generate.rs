//! Pure generation of the SCM registration from a resolved spec, and extraction of
//! the embedded metadata blob back out of one.
//!
//! Not `#[cfg]`-gated: this compiles and its tests run on every platform.
//!
//! There is no single artifact *file* for SCM the way there is a unit file or a
//! plist, so this module's "artifact" is [`ScmRegistration`]: everything an
//! effectful installer needs to hand to `windows-service`'s `ServiceInfo`, plus
//! the `Services\<name>\Parameters` values that carry the metadata blob. Building
//! one does no I/O and needs no privileges — see [`crate::backend::Identity`] for
//! why account resolution happens before this module ever runs.
//!
//! **`executable`/`arguments` are unquoted.** An earlier design carried a single
//! pre-quoted `image_path` string instead, and that was wrong twice over:
//! `windows-service`'s `ServiceInfo` escapes `executable_path` and every
//! `launch_arguments` element itself when it builds `lpBinaryPathName`, so a
//! pre-quoted string would be escaped a second time and SCM would fail to launch
//! anything whose path contains a space — a silent install-time corruption, not a
//! compile error. And on readback, `windows-service`'s `ServiceConfig` has no
//! `launch_arguments` field at all; it stuffs the whole command line into
//! `executable_path`, which makes reconstructing an argv the only way to compare
//! against a desired [`ScmRegistration`] for drift. Keeping the fields unquoted
//! here defers escaping to the one place that already owns it (`windows-service`,
//! at effectful-install time) and keeps `render()` diffable field-by-field rather
//! than as one opaque quoted string.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::Identity;
use crate::blob::{self, Blob};
use crate::error::Error;
use crate::spec::{DaemonSpec, Kind, Restart, User};

// Metadata field names ================================================================================================

/// Byte-exact per the Global Constraints: the same four names every embed
/// site (systemd's `[X-Goetia]`, launchd's comment block, SCM's
/// `Parameters`) uses.
const FIELD_MARKER: &str = "Marker";
const FIELD_SCHEMA: &str = "Schema";
const FIELD_VERSION: &str = "Version";
const FIELD_SPEC: &str = "Spec";

// Constants ===========================================================================================================

/// `SC_ACTION.Delay` when `goetia.yaml` sets no `restart-delay`. Matches
/// `~/src/hole/crates/bridge/src/platform/windows.rs`'s
/// `restart_failure_actions`: long enough that a process that dies
/// instantly on start doesn't spin SCM in a tight relaunch loop, short
/// enough that a benign flap recovers quickly.
const DEFAULT_RESTART_DELAY: Duration = Duration::from_secs(1);

/// `dwResetPeriod`: the window of health after which SCM's failure counter
/// resets to zero — **not** the restart delay, which is `SC_ACTION.Delay`
/// (`FailureActions::delay` below). Not spec data: `goetia.yaml` has no
/// field for it, so this is a fixed constant rather than something
/// `registration` derives. `~/src/hole/crates/bridge/src/platform/windows.rs`
/// uses the same 86400s (one day): a crash-loop's failure count keeps
/// climbing across restarts within a day, but a service that has been
/// healthy for a day gets a fresh budget.
const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(86_400);

// ScmRegistration =====================================================================================================

/// Everything an effectful SCM installer needs to register a service, or to
/// compare against one already installed. Resolved from a [`DaemonSpec`]
/// plus a pre-resolved [`Identity`]; building one does no I/O.
///
/// Deliberately has no start-type field. Boot enablement (`SERVICE_DEMAND_START`
/// vs. `SERVICE_AUTO_START`) is a property of the installation, not the service,
/// so it is excluded here and therefore excluded from [`render`] and from every
/// drift comparison built on it — see `render_excludes_boot_enablement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmRegistration {
    pub name: String,
    pub display_name: String,
    /// Unquoted — see the module doc comment.
    pub executable: PathBuf,
    /// Unquoted, one element per argv entry — see the module doc comment.
    pub arguments: Vec<String>,
    /// `None` => `LocalSystem`.
    pub account: Option<String>,
    pub failure_actions: Option<FailureActions>,
    /// The `Services\<name>\Parameters` values, including the metadata blob
    /// (`Marker`/`Schema`/`Version`/`Spec`).
    pub parameters: BTreeMap<String, String>,
}

/// SCM recovery-action configuration. Only takes effect together with
/// `on_non_crash_failures`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureActions {
    /// `SC_ACTION.Delay`: how long SCM waits before restarting.
    pub delay: Duration,
    /// `dwResetPeriod` — see the module-level constant of the same purpose.
    pub reset_period: Duration,
    /// Must be `true`. Without `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`, SCM
    /// runs recovery actions only when a process terminates *without*
    /// reporting `SERVICE_STOPPED` to SCM first — so a `type: managed`
    /// daemon that fails to bind a port and exits 1 *cleanly* (a
    /// well-behaved failure report, not a crash) would never restart, ever,
    /// including at boot. `~/src/hole/crates/bridge/src/platform/windows.rs`'s
    /// `apply_failure_actions` sets the same flag for the same reason.
    pub on_non_crash_failures: bool,
}

// Building ============================================================================================================

/// Build the [`ScmRegistration`] for `spec`.
///
/// `id` is the account already resolved by the effectful install path (see
/// [`Identity`]) — this function does no lookup of its own. `shim_path` is
/// only used for `Kind::Simple`, where the shim (not the spec's own command)
/// is what SCM launches.
pub fn registration(spec: &DaemonSpec, id: &Identity, shim_path: &Path) -> ScmRegistration {
    let (executable, arguments) = match spec.kind {
        // The shim supervises the child itself; SCM only ever launches the
        // shim, with the daemon id as its sole argument. The spec's own
        // command never appears here.
        Kind::Simple => (shim_path.to_path_buf(), vec![spec.id.as_str().to_string()]),
        Kind::Managed => {
            let mut argv = spec.command.iter();
            let executable = PathBuf::from(
                argv.next()
                    .expect("DaemonSpec::command is non-empty: spec::resolve rejects an empty command"),
            );
            (executable, argv.cloned().collect())
        }
    };

    let failure_actions = match spec.kind {
        // The shim supervises the child and implements restart itself
        // (matching the nssm-based deployment this project is modeled on,
        // where `sc qfailure` is deliberately empty) — SCM never restarts
        // it directly.
        Kind::Simple => None,
        Kind::Managed => match spec.restart {
            Restart::Never => None,
            Restart::OnFailure | Restart::Always => Some(FailureActions {
                delay: spec.restart_delay.unwrap_or(DEFAULT_RESTART_DELAY),
                reset_period: FAILURE_RESET_PERIOD,
                on_non_crash_failures: true,
            }),
        },
    };

    // `User::Root` is a universal token every backend maps for itself
    // (`User=0` on systemd, `root` on launchd); SCM's mapping is
    // `LocalSystem`, expressed here as `None` rather than the literal
    // string, so `render` controls its own spelling. Every other variant
    // was already resolved to a platform account name upstream (a
    // `LookupAccountSid` call for `User::Id(Sid)` is I/O, which this
    // function must not do) — `id.user` carries that resolved name.
    let account = match &spec.user {
        User::Root => None,
        _ => Some(id.user.clone()),
    };

    let mut parameters = BTreeMap::new();
    parameters.insert(FIELD_MARKER.to_string(), blob::MARKER.to_string());
    parameters.insert(FIELD_SCHEMA.to_string(), blob::SCHEMA.to_string());
    parameters.insert(FIELD_VERSION.to_string(), crate::version().to_string());
    parameters.insert(FIELD_SPEC.to_string(), blob::encode(spec));

    ScmRegistration {
        name: spec.id.as_str().to_string(),
        display_name: spec.name.clone(),
        executable,
        arguments,
        account,
        failure_actions,
        parameters,
    }
}

// Extraction ==========================================================================================================

/// Recover the embedded metadata blob from a service's `Parameters` values.
///
/// Returns `Ok(None)` only when `Marker` is absent — an ordinary foreign
/// service, not one Goetia manages. Once `Marker` is present, every other
/// defect (a value that doesn't match, a missing field, an undecodable
/// `Spec`) is `Err`: a `Parameters` key set that merely *claims* to be ours
/// must check out completely, not be partially trusted.
pub fn extract(parameters: &BTreeMap<String, String>) -> Result<Option<Blob>, Error> {
    let Some(marker) = parameters.get(FIELD_MARKER) else {
        return Ok(None);
    };
    if marker != blob::MARKER {
        return Err(Error::Blob(format!(
            "Parameters\\{FIELD_MARKER} is `{marker}`, not `{expected}`",
            expected = blob::MARKER
        )));
    }

    let schema_text = parameters
        .get(FIELD_SCHEMA)
        .ok_or_else(|| missing_field_error(FIELD_SCHEMA))?;
    schema_text.parse::<u32>().map_err(|source| {
        Error::Blob(format!(
            "Parameters\\{FIELD_SCHEMA} `{schema_text}` is not an integer: {source}"
        ))
    })?;

    if !parameters.contains_key(FIELD_VERSION) {
        return Err(missing_field_error(FIELD_VERSION));
    }

    let spec_text = parameters
        .get(FIELD_SPEC)
        .ok_or_else(|| missing_field_error(FIELD_SPEC))?;
    blob::decode(spec_text).map(Some)
}

fn missing_field_error(field: &str) -> Error {
    Error::Blob(format!(
        "Parameters\\{FIELD_MARKER} is present but Parameters\\{field} is missing"
    ))
}

// Rendering ===========================================================================================================

/// Render `reg` as canonical text for diffing (`crate::diff::artifact_diff`'s
/// `was`/`now`), one field per line so a one-field change shows as a
/// one-line diff. Deterministic: `parameters` is already sorted by key, and
/// no other field's rendering depends on anything but its own value.
///
/// Never mentions boot enablement — `ScmRegistration` has no such field, so
/// there is nothing here to render for it. See `render_excludes_boot_enablement`.
pub fn render(reg: &ScmRegistration) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Name: {}", reg.name);
    let _ = writeln!(out, "DisplayName: {}", reg.display_name);
    let _ = writeln!(out, "Account: {}", reg.account.as_deref().unwrap_or("LocalSystem"));
    let _ = writeln!(out, "Executable: {:?}", reg.executable);
    let _ = writeln!(out, "Arguments:");
    for arg in &reg.arguments {
        let _ = writeln!(out, "  - {arg:?}");
    }
    match &reg.failure_actions {
        None => {
            let _ = writeln!(out, "FailureActions: none");
        }
        Some(fa) => {
            let _ = writeln!(out, "FailureActions:");
            let _ = writeln!(out, "  Delay: {}", humantime::format_duration(fa.delay));
            let _ = writeln!(out, "  ResetPeriod: {}", humantime::format_duration(fa.reset_period));
            let _ = writeln!(out, "  OnNonCrashFailures: {}", fa.on_non_crash_failures);
        }
    }
    let _ = writeln!(out, "Parameters:");
    for (key, value) in &reg.parameters {
        let _ = writeln!(out, "  {key}: {value}");
    }
    out
}

#[cfg(test)]
#[path = "generate_tests.rs"]
mod generate_tests;
