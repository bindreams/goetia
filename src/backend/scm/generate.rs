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
//! **`executable`/`arguments` are unquoted, for two independent reasons.**
//! `windows-service`'s `ServiceInfo` escapes `executable_path` and every
//! `launch_arguments` element itself when it builds `lpBinaryPathName`, so a
//! pre-quoted string would be escaped a second time and SCM would fail to launch
//! anything whose path contains a space — a silent install-time corruption, not a
//! compile error. And on readback, `windows-service`'s `ServiceConfig` has no
//! `launch_arguments` field at all; it stuffs the whole command line into
//! `executable_path`, which makes reconstructing an argv the only way to compare
//! against a desired [`ScmRegistration`] for drift. Keeping the fields unquoted
//! here defers escaping to the one place that already owns it (`windows-service`,
//! at effectful-install time) and keeps its `Display` text diffable field-by-field rather
//! than as one opaque quoted string.
//!
//! **That readback path has a real, residual limitation.** Real
//! `CommandLineToArgvW` parses `argv[0]` (the program name) under different rules
//! than every later argument: a backslash is always literal there, never doubled
//! or halved the way `windows-service`'s escaping and every later argument's
//! parsing treat it. So an `executable` that both needs quoting (holds a
//! space, tab, newline or vertical tab) *and* ends in a backslash cannot be losslessly recovered from
//! `lpBinaryPathName` — `windows-service` writes a doubled trailing backslash to
//! protect its closing quote, and `argv[0]` parsing has no rule that undoes that
//! doubling. See `generate_tests.rs`'s
//! `windows_style_split_matches_real_command_line_to_argv_w` (which pins the
//! `argv[0]` rule against the real Win32 API) and
//! `executable_trailing_backslash_does_not_round_trip_through_argv0` (which
//! documents the writer's behaviour). `executable` is a `HostCommand`'s `exe` or, for a `type: managed`
//! daemon, `command[0]`; one the writer would rewrite is refused, see `refuse_unwritable_executable`:
//! a `command[0]` by [`registration`], and a host's path by [`install_registration`], which `install` and
//! `diff` call.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::Identity;
use crate::backend::scm::parameters::{Stray, field_named_utf16, is_string_type, type_name};
use crate::blob::{self, Blob};
use crate::error::Error;
use crate::shim::HostCommand;
use crate::spec::{DaemonSpec, Id, Protocol, Restart, User, scm_restart_delay, windows_builtin};

// Metadata field names ================================================================================================

/// Byte-exact per the Global Constraints: the same four names every embed
/// site (systemd's `[X-Goetia]`, launchd's comment block, SCM's
/// `Parameters`) uses.
pub(crate) const FIELD_MARKER: &str = "Marker";
const FIELD_SCHEMA: &str = "Schema";
const FIELD_VERSION: &str = "Version";
const FIELD_SPEC: &str = "Spec";
pub(crate) const FIELDS: [&str; 4] = [FIELD_MARKER, FIELD_SCHEMA, FIELD_VERSION, FIELD_SPEC];

// Constants ===========================================================================================================

/// `SC_ACTION.Delay` when `goetia.yaml` sets no `restart-delay`. Matches
/// `hole`'s bridge service (`restart_failure_actions`): long enough that a
/// process that dies instantly on start doesn't spin SCM in a tight
/// relaunch loop, short enough that a benign flap recovers quickly.
const DEFAULT_RESTART_DELAY: Duration = Duration::from_secs(1);

/// `dwResetPeriod`: the window of health after which SCM's failure counter
/// resets to zero — **not** the restart delay, which is `SC_ACTION.Delay`
/// (`FailureActions::delay` below). Not spec data: `goetia.yaml` has no
/// field for it, so this is a fixed constant rather than something
/// `registration` derives. `hole`'s bridge service (see `DEFAULT_RESTART_DELAY`
/// above) uses the same 86400s (one day): a crash-loop's failure count keeps
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
/// so it is excluded here and therefore excluded from its `Display` text and from every
/// drift comparison built on it — see `display_excludes_boot_enablement`.
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
    /// The `Services\<name>\Environment` `REG_MULTI_SZ` lines, as written and as read back: raw,
    /// so a hand-reordered or malformed value compares as it stands. Empty for a `hosted` service
    /// — see [`registration`].
    pub environment: Vec<String>,
    /// The `Services\<name>\Parameters` values, including the metadata blob
    /// (`Marker`/`Schema`/`Version`/`Spec`).
    pub parameters: BTreeMap<String, String>,
    /// What else `Parameters` holds: empty for a registration built from a spec, and drift in a
    /// readback, which [`Display`](fmt::Display) shows and `install --force` clears.
    pub stray_parameters: Vec<Stray>,
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
    /// reporting `SERVICE_STOPPED` to SCM first — so a daemon that fails to
    /// bind a port and exits 1 *cleanly* (a well-behaved failure report, not
    /// a crash) would never restart, ever, including at boot. `hole`'s bridge
    /// service sets the same flag in `apply_failure_actions`, for the same
    /// reason.
    pub on_non_crash_failures: bool,
}

// Building ============================================================================================================

/// Build the [`ScmRegistration`] for `spec`. A `restart-delay` is rounded up to whole
/// milliseconds and clamped to what `SC_ACTION.Delay` can express, rather than left to
/// panic at install time — see `scm_restart_delay`; the advisories for those
/// fire at resolve time, from `BackendKind::Scm.warn`, when scm is the native backend.
///
/// `id` is the account already resolved by the effectful install path (see
/// [`Identity`]) — this function does no lookup of its own. `host` is called at most once, and
/// only for a `hosted` service, where the host (not the spec's own command) is what SCM launches,
/// with the host's own arguments; it is not judged here: see [`install_registration`]. Its error is
/// this function's.
///
/// Refuses, with [`Error::Invalid`]:
///
/// - a `type: managed` `command[0]` the registration writer would rewrite;
/// - `restart: always`: SCM recovery actions never fire after a clean exit, so the registration
///   could only run on-failure semantics.
pub fn registration(
    spec: &DaemonSpec,
    id: &Identity,
    host: &dyn Fn() -> Result<HostCommand, Error>,
) -> Result<ScmRegistration, Error> {
    let host = resolve_host(spec, host)?;
    build(spec, id, host.as_ref())
}

/// Whether `spec` runs under the shim host: a `Protocol::Plain` service, whose command is not what SCM
/// launches. A `Protocol::Native` service is its own executable, with no host.
pub(crate) fn hosted(spec: &DaemonSpec) -> bool {
    let DaemonSpec::Service(s) = spec;
    match s.protocol {
        Protocol::Plain => true,
        Protocol::Native => false,
    }
}

/// The host `spec` is installed with: asked for once, and only for a [`hosted`] service.
fn resolve_host(
    spec: &DaemonSpec,
    host: &dyn Fn() -> Result<HostCommand, Error>,
) -> Result<Option<HostCommand>, Error> {
    if hosted(spec) { host().map(Some) } else { Ok(None) }
}

/// [`registration`] for a `host` already resolved by [`resolve_host`].
fn build(spec: &DaemonSpec, id: &Identity, host: Option<&HostCommand>) -> Result<ScmRegistration, Error> {
    debug_assert_eq!(
        host.is_some(),
        hosted(spec),
        "a hosted spec's host is resolved before it is built, and only a hosted spec's"
    );
    let DaemonSpec::Service(s) = spec;
    // SCM applies `Environment` to the process it launches: the daemon for a native service, the shim
    // for a hosted one.
    let (executable, arguments, environment) = match host {
        // SCM only ever launches the host, which learns the service's name from SCM and spawns the
        // daemon once. The spec's own command never appears here. The shim already applies `spec.env`
        // to the daemon it spawns, and an `env:` key that collides with one the shim or `cosca` reads
        // (`__COSCA_GROUP_ROOT`) would corrupt the shim's own containment, so no `Environment` is
        // registered.
        Some(host) => (host.exe.clone(), host.args.clone(), Vec::new()),
        None => {
            let mut argv = s.common.command.iter();
            let executable = PathBuf::from(
                argv.next()
                    .expect("DaemonFields::command is non-empty: spec::resolve rejects an empty command"),
            );
            refuse_unwritable_executable(&s.common.id, ExecutablePath::Command0, &executable)?;
            (
                executable,
                argv.cloned().collect(),
                format_environment_lines(&s.common.env),
            )
        }
    };

    // SCM does every restart, for both kinds: the shim spawns the daemon once and reports its exit.
    let failure_actions = match s.restart {
        Restart::Never => None,
        Restart::OnFailure => {
            let delay = s.restart_delay.unwrap_or(DEFAULT_RESTART_DELAY);
            Some(FailureActions {
                delay: scm_restart_delay(delay),
                reset_period: FAILURE_RESET_PERIOD,
                on_non_crash_failures: true,
            })
        }
        Restart::Always => {
            return Err(Error::Invalid {
                daemon: s.common.id.to_string(),
                message: "`restart: always` cannot be honoured by the scm backend: SCM recovery actions never fire \
                          after a clean exit. To keep `always` on systemd and launchd, set \
                          `backend-specific: scm: { restart: on-failure }`"
                    .to_string(),
                source: None,
            });
        }
    };

    // `User::Root` is a universal token every backend maps for itself
    // (`User=0` on systemd, `root` on launchd); SCM's mapping is
    // `LocalSystem`, expressed here as `None` rather than the literal
    // string, so `Display` controls its own spelling. Every other variant
    // was already resolved to a platform account name upstream (a
    // `LookupAccountSid` call for `User::Id(Sid)` is I/O, which this
    // function must not do) — `id.user` carries that resolved name, which
    // `canonical_account` then canonicalises. `User::Root` must never
    // reach `canonical_account`: `identity::resolve(&User::Root)` returns
    // an empty `Identity::user`, and `canonical_account("")` is
    // deliberately `Some(LocalSystem)` (`None`) too — see its doc comment
    // — so this arm keeps that row unreachable from an authored spec
    // rather than merely coincidentally correct.
    let account = match &s.common.user {
        User::Root => None,
        _ => canonical_account(&id.user),
    };

    let mut parameters = BTreeMap::new();
    parameters.insert(FIELD_MARKER.to_string(), blob::MARKER.to_string());
    parameters.insert(FIELD_SCHEMA.to_string(), blob::SCHEMA.to_string());
    parameters.insert(FIELD_VERSION.to_string(), crate::version().to_string());
    parameters.insert(FIELD_SPEC.to_string(), blob::encode(spec));

    Ok(ScmRegistration {
        name: s.common.id.as_str().to_string(),
        display_name: s.common.name.clone(),
        executable,
        arguments,
        account,
        failure_actions,
        environment,
        parameters,
        stray_parameters: Vec::new(),
    })
}

/// [`registration`], for an install or its preview: also refuses a `type: simple` daemon when the
/// host's path would be written as another path. The path is the host's, not the daemon's, so this is
/// the install's refusal and not the rendering's: `list` and `status` render every daemon, and must
/// not report one as having an unusable embedded spec because of where goetia itself is installed.
/// A `type: managed` daemon never runs the host, so its path is not judged.
pub fn install_registration(
    spec: &DaemonSpec,
    id: &Identity,
    host: &dyn Fn() -> Result<HostCommand, Error>,
) -> Result<ScmRegistration, Error> {
    let DaemonSpec::Service(s) = spec;
    let host = resolve_host(spec, host)?;
    if let Some(host) = &host {
        refuse_unwritable_executable(&s.common.id, ExecutablePath::ServiceHost, &host.exe)?;
    }
    build(spec, id, host.as_ref())
}

/// What makes `windows-service` wrap an argument in quotes (an empty one is wrapped too), each with
/// the name a message uses for it.
const WRITER_QUOTE_TRIGGERS: [(char, &str); 5] = [
    ('"', "a double quote"),
    (' ', "a space"),
    ('\n', "a newline"),
    ('\t', "a tab"),
    ('\u{000B}', "a vertical tab"),
];

/// Which path [`refuse_unwritable_executable`] judges, as its message names it.
#[derive(Clone, Copy)]
enum ExecutablePath {
    /// `command[0]` of a `type: managed` daemon.
    Command0,
    /// The service host's path for a `type: simple` daemon, which no manifest sets.
    ServiceHost,
}

impl ExecutablePath {
    fn label(self) -> &'static str {
        match self {
            Self::Command0 => "`command[0]`",
            Self::ServiceHost => "the service host path",
        }
    }
}

/// Refuse an `executable` that `windows-service` would write as a different path.
///
/// Its writer wraps the path in quotes when it is empty or holds a [`WRITER_QUOTE_TRIGGERS`]
/// character. Inside those quotes it escapes a `"` with a backslash and doubles a trailing
/// backslash run. `argv[0]` is read literally up to the next `"`, so neither undoes: a quoted path
/// holding a `"`, or ending in any backslashes, registers as another path. An unquoted path is
/// written as is.
///
/// `which` names the path in the message.
fn refuse_unwritable_executable(daemon: &Id, which: ExecutablePath, executable: &Path) -> Result<(), Error> {
    let text = executable.to_string_lossy();
    // A `"` is refused whatever else quotes the path. An empty path is quoted too, but it ends in no
    // backslash.
    let reason = if text.contains('"') {
        "it holds a double quote, which the Windows service registration writer escapes, \
         and SCM reads the program name up to the next quote with every backslash literal, so SCM would \
         start a different path"
            .to_string()
    } else if let Some((_, trigger)) = WRITER_QUOTE_TRIGGERS.iter().find(|(c, _)| text.contains(*c))
        && text.ends_with('\\')
    {
        format!(
            "it holds {trigger}, so the Windows service registration writer wraps it in quotes and doubles the \
             backslashes it ends in, which SCM reads literally, so SCM would start a different path"
        )
    } else {
        return Ok(());
    };
    Err(Error::Invalid {
        daemon: daemon.to_string(),
        message: format!(
            "{} {text:?} cannot be written into the service registration: {reason}",
            which.label()
        ),
        source: None,
    })
}

/// `env` as the `REG_MULTI_SZ` lines `Services\<name>\Environment` needs: the format SCM reads
/// when it starts the service process, `NAME=value` per entry in key order.
pub fn format_environment_lines(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

// Account canonicalisation ============================================================================================

/// The account SCM should be given for `resolved`, canonicalised.
/// `None` means LocalSystem: the `ServiceInfo` default, and how
/// `User::Root` already renders.
///
/// Reads `spec::backend::windows_builtin` — the recognition half of this
/// table, which lives in `spec` because `BackendKind::error` must apply it
/// from every host (see that module's doc comment). This is the spelling
/// half: `Builtin::canonical` supplies the byte-exact output
/// `CreateServiceW` accepts, and this function is the whole of the
/// composition between the two — not a second copy of the fold.
///
/// `resolved` is expected to be the account a non-Root `User` already
/// resolved to (`Identity::user`); an empty string canonicalises to
/// `None` (LocalSystem), which is correct only for `User::Root`'s
/// legitimately empty identity — see `windows_builtin("")`'s doc comment.
/// Callers must apply this only to the non-Root branch, keeping the
/// existing `User::Root => None` arm untouched, so an *authored* empty
/// name (already rejected earlier by `reject_blank`) can never reach this
/// row.
pub fn canonical_account(resolved: &str) -> Option<String> {
    match windows_builtin(resolved) {
        Some(builtin) => builtin.canonical().map(str::to_string),
        None => Some(resolved.to_string()),
    }
}

/// Whether Windows will refuse to start this account without a password.
/// `None` (LocalSystem) never does; a recognised built-in or virtual
/// per-service account (`NT SERVICE\<id>`) never does either. Any other
/// resolved account name is assumed to be a real user account, which
/// does.
///
/// Reads `windows_builtin` for the built-in half, the same table
/// `canonical_account` reads — so a spaced spelling
/// (`NT AUTHORITY\LOCAL SERVICE`) is recognised here too, not only after
/// `canonical_account` has already run. `every_canonical_builtin_needs_no_password`
/// additionally pins that composing the two always answers `false` for
/// every built-in spelling, which is what `manager::apply`'s password gate
/// and `grant_service_logon_right`'s skip both depend on.
///
/// `NT SERVICE\<id>` accounts are not in `windows_builtin`'s table — they
/// are virtual per-service accounts, not built-ins — so they are checked
/// separately, by prefix.
///
/// A heuristic on the account *name* rather than a `LookupAccountSid`-based
/// well-known-SID check: `ServiceInfo`/`ChangeServiceConfigW` only ever see
/// the name, and every one of these accounts is required to be named as
/// such by SCM's own conventions — there is no other spelling a caller
/// could use for them.
pub fn account_needs_password(account: Option<&str>) -> bool {
    let Some(account) = account else {
        return false;
    };
    if windows_builtin(account).is_some() {
        return false;
    }
    !account.to_ascii_lowercase().starts_with(r"nt service\")
}

// Extraction ==========================================================================================================

/// Recover the embedded metadata blob from a service's `Parameters`: `Ok(None)` when it holds no
/// `Marker` (a foreign service). With one, `Spec` must decode: a missing or undecodable one is
/// `Err`, never partially trusted. `Schema` and `Version` are copies for a human and are not read.
/// `fields` and `strays` are those of [`Parameters`](crate::backend::scm::parameters::Parameters);
/// `strays` only lets the error say that a `Spec` is stored as something other than a string, or as
/// a string that is not text.
pub fn extract(fields: &BTreeMap<String, String>, strays: &[Stray]) -> Result<Option<Blob>, Error> {
    if !fields.contains_key(FIELD_MARKER) {
        return Ok(None);
    }
    let Some(spec_text) = fields.get(FIELD_SPEC) else {
        return Err(missing_spec_error(strays));
    };
    Ok(Some(blob::decode(spec_text)?))
}

/// Goetia's spelling of the field `name` spells in any ASCII case, or `None` for any other name.
/// ASCII-only: all four field names are plain ASCII, so no Unicode case-folding is needed. The
/// registry compares value names this way, so a stored `marker` is the `Marker` goetia writes.
pub(crate) fn field_named(name: &str) -> Option<&'static str> {
    FIELDS.into_iter().find(|field| field.eq_ignore_ascii_case(name))
}

fn missing_spec_error(strays: &[Stray]) -> Error {
    let stored = strays.iter().find_map(|stray| match stray {
        Stray::Value { name, vtype } if field_named_utf16(name) == Some(FIELD_SPEC) => Some(*vtype),
        _ => None,
    });
    let what = match stored {
        // `string_text` refused it: a NUL inside the text, an odd byte count or invalid UTF-16.
        Some(vtype) if is_string_type(vtype) => format!(
            "a {} that is not valid text: a NUL inside the text, an odd byte count or invalid UTF-16",
            type_name(vtype)
        ),
        Some(vtype) => format!("stored as {}, not as a string", type_name(vtype)),
        None => "missing".to_string(),
    };
    Error::blob(format!(
        "Parameters\\{FIELD_MARKER} is present but Parameters\\{FIELD_SPEC} is {what}"
    ))
}

// Rendering ===========================================================================================================

/// A registration's canonical text for diffing (`crate::diff::artifact_diff`'s
/// `was`/`now`), one field per line so a one-field change shows as a
/// one-line diff. Deterministic: `parameters` is already sorted by key, and
/// no other field's rendering depends on anything but its own value.
///
/// `DisplayName`, `Account`, and every `Parameters` value are rendered with
/// `{:?}` (as `Arguments` already was) rather than interpolated raw: `was`
/// is built from a readback of whatever is actually installed, and readback
/// data (`ServiceConfig::display_name`/`account_name`, arbitrary `Parameters`
/// values SCM never validates) is not covered by the injection gate that
/// keeps goetia-authored strings newline-free. Rendering it raw would let a
/// value containing a newline reproduce lines indistinguishable from real
/// fields, defeating the line-based diff this text exists to support.
/// `Name` is exempt: it is always `spec.id`, whose pattern
/// (`^[A-Za-z0-9._-]{1,80}$`) cannot contain a newline by construction.
///
/// Never mentions boot enablement — `ScmRegistration` has no such field, so
/// there is nothing here to render for it. See `display_excludes_boot_enablement`.
impl fmt::Display for ScmRegistration {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out, "Name: {}", self.name)?;
        writeln!(out, "DisplayName: {:?}", self.display_name)?;
        writeln!(out, "Account: {:?}", self.account.as_deref().unwrap_or("LocalSystem"))?;
        writeln!(out, "Executable: {:?}", self.executable)?;
        writeln!(out, "Arguments:")?;
        for arg in &self.arguments {
            writeln!(out, "  - {arg:?}")?;
        }
        match &self.failure_actions {
            None => writeln!(out, "FailureActions: none")?,
            Some(fa) => {
                writeln!(out, "FailureActions:")?;
                writeln!(out, "  Delay: {}", humantime::format_duration(fa.delay))?;
                writeln!(out, "  ResetPeriod: {}", humantime::format_duration(fa.reset_period))?;
                writeln!(out, "  OnNonCrashFailures: {}", fa.on_non_crash_failures)?;
            }
        }
        writeln!(out, "Environment:")?;
        for line in &self.environment {
            writeln!(out, "  {line:?}")?;
        }
        writeln!(out, "Parameters:")?;
        for (key, value) in &self.parameters {
            writeln!(out, "  {key}: {value:?}")?;
        }
        if !self.stray_parameters.is_empty() {
            writeln!(out, "Stray parameters:")?;
            for stray in &self.stray_parameters {
                writeln!(out, "  {stray}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "generate_tests.rs"]
mod generate_tests;
