//! The `ServiceManager` seam: one trait implemented by each real backend
//! (Tasks 11-13) and by [`fake::Fake`], the in-memory test double every CLI
//! and policy test in this crate runs against instead of a real
//! systemd/launchd/SCM.
//!
//! The trait exists so the CLI is written once and tested without touching a
//! real machine — not for runtime backend swapping. Every implementation's
//! `install` must call [`crate::decide::decide`] rather than restate any row
//! of its policy table itself; see [`conformance::run`], which asserts that
//! contract against any `&dyn ServiceManager`.

pub mod conformance;
pub mod fake;

// All three supported platforms now return a real `ServiceManager`, so
// `Error::UnsupportedPlatform` is referenced only by the catch-all arm.
// Gating the import keeps every supported target from warning about it.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use crate::error::Error;
use crate::error::Result;
use crate::spec::{DaemonSpec, Id};

// ServiceManager ======================================================================================================

/// One platform's service manager, seen through the operations Goetia needs.
///
/// `install`/`uninstall`/`enable`/`disable`/`start`/`stop` are the mutating
/// verbs; `status`/`list` are read-only. The CLI checks elevation itself
/// before calling any mutating verb — implementations do not re-check it.
pub trait ServiceManager {
    /// Install (create or update) the service for `spec`. Never starts and
    /// never enables it at boot — see the crate-level design notes on
    /// install-off-by-default. Routes through [`crate::decide::decide`]; see
    /// that function's doc comment for what each [`crate::decide::Outcome`]
    /// means and when `force` is honored.
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<crate::decide::Outcome>;

    /// What [`Self::install`] would do for `spec`, without doing it —
    /// always as if `force` were `false`, since showing the forced outcome
    /// would hide the very conflict `--force` exists to let a user decide
    /// about. Backs `goetia daemon diff`: routing through the exact same
    /// [`crate::decide::decide`] call `install` uses (rather than `diff`
    /// reimplementing a partial version of that policy against `list()`'s
    /// output) is what keeps the two from being able to disagree — `diff`
    /// saying "up to date" or "would be created" about something `install`
    /// would actually refuse or conflict on.
    fn preview_install(&self, spec: &DaemonSpec) -> Result<crate::decide::Outcome>;

    /// Stop the service if running, remove the artifact, and reload the
    /// manager. Operates by id alone — no manifest needed, so it still works
    /// on a machine where the original `goetia.yaml` clone is gone.
    fn uninstall(&self, id: &Id) -> Result<()>;

    /// Enable the service at boot. Does not start it. `Err(NotInstalled)` if
    /// `id` is not managed by Goetia.
    fn enable(&self, id: &Id) -> Result<()>;

    /// Disable the service at boot. Does not stop it if running.
    fn disable(&self, id: &Id) -> Result<()>;

    /// Start the service now. Does not change its boot-enablement.
    /// Idempotent: starting an already-running service is `Ok(())`, not an
    /// error.
    fn start(&self, id: &Id) -> Result<()>;

    /// Stop the service now. Does not change its boot-enablement.
    /// Idempotent: stopping an already-stopped service is `Ok(())`, not an
    /// error — `daemon restart`'s `stop` then `start` depends on this
    /// holding for a daemon that was never started, and real managers
    /// disagree by default (`launchctl bootout`/`ControlService(STOP)` on
    /// an inactive service both fail; `systemctl stop` does not), so an
    /// implementation must paper over that difference itself, not leave it
    /// for a caller to rediscover per platform.
    fn stop(&self, id: &Id) -> Result<()>;

    /// The live state of one installed service. `Err` for an id whose blob
    /// will not decode — this must not fabricate a plausible-looking
    /// `Status` for state it cannot actually determine (see the crate-level
    /// design notes on `Installed::OursUnreadable`, which exists for the
    /// same reason on the `list` side) — and [`Error::Undetermined`] for an
    /// id whose artifact could not be read at all, where not even ownership
    /// was established.
    ///
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    fn status(&self, id: &Id) -> Result<Status>;

    /// Every id this backend could account for. A foreign (unmarked)
    /// service at some id is never included; see [`Installed`] for what
    /// happens when a marked one exists but its blob will not decode, and
    /// for the entry an id goetia could not read at all produces. Skipping
    /// such an id is not an implementation's choice to make: an unread id
    /// must appear as [`Installed::Undetermined`], because an enumeration
    /// that silently drops it is indistinguishable from one where it does
    /// not exist.
    ///
    /// **An absent container — no unit directory, no `Services` key — is
    /// `Ok(vec![])`, never `Err`.** Absence is established there: nothing is
    /// installed, which is a determinate answer and is reported as one. An
    /// `Err` reaches the CLI as `Kind::Unavailable`, exit `1` over an empty
    /// document — the rendering that says goetia could not answer — and
    /// putting a settled answer behind it is the same conflation
    /// [`Installed::Undetermined`] exists to end, arrived at from the other
    /// side. A scan that *started* and did not finish is the opposite case
    /// and does report (see `Installed::scan_incomplete`).
    fn list(&self) -> Result<Vec<Installed>>;
}

// Installed / Status / State ==========================================================================================

/// One entry from [`ServiceManager::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// A Goetia-marked service whose blob decoded successfully.
    Ours {
        spec: DaemonSpec,
        state: State,
        /// The same number [`ServiceManager::status`] would report for
        /// this id at this moment — not a cached or independently derived
        /// value. See each backend's `list` for how it obtains this.
        pid: Option<u32>,
        enabled: bool,
    },
    /// Ours by marker, but the blob will not decode — a newer schema, or
    /// corruption. Kept distinct from a silent omission: a single artifact
    /// written by a newer Goetia must not take down `list`/`status` for
    /// every other daemon, which is exactly what dropping this entry
    /// silently would do.
    OursUnreadable { name: String, reason: String },
    /// Neither proof: the read that would have classified this id did not
    /// complete, so goetia established neither its absence nor its
    /// presence. The `list`-side counterpart of [`Error::Undetermined`],
    /// and separated from the two variants above by the same rule — what
    /// goetia *established*, never what went wrong. Reporting
    /// `OursUnreadable` for a unit file an unelevated caller merely could
    /// not open claims ownership of what may be a stranger's service;
    /// omitting it claims it does not exist.
    ///
    /// `name` is `None` only for an entry standing for more than one id.
    /// That happens for either of two reasons, and a caller must not assume
    /// the first: an enumeration that did not finish knows neither the names
    /// it never reached nor how many there were (see `Installed::scan_incomplete`),
    /// *or* one that enumerated fine drops the names because
    /// one entry cannot usefully carry hundreds — which is what Windows SCM
    /// does on an unelevated `list`, where most services deny a `Parameters`
    /// read at once. An entry for exactly one known id always names it.
    ///
    /// What a `None` obliges of every caller: **while such an entry is
    /// present, no negative conclusion about any id is sound.** It may
    /// stand for the very id being asked about, so "`x` is absent" does not
    /// follow from `x` going unnamed in the list — not for rendering, not
    /// for an exit code, and not for a test assertion, which must fail
    /// rather than certify what the list cannot establish.
    ///
    /// Carries no `recovery`: that text belongs to [`Error::Undetermined`],
    /// which each backend builds through its own constructor, and a second
    /// independently worded remedy per `list` entry would put two answers
    /// on one condition.
    ///
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    Undetermined { name: Option<String>, reason: String },
}

impl Installed {
    /// The aggregate entry an enumeration that stopped early leaves behind: `location` is what
    /// was being scanned, `detail` what did not complete there. Every backend builds it here, so
    /// a scan cut short in a unit directory, a `LaunchDaemons` directory or the SCM registry key
    /// says one thing rather than three.
    ///
    /// Two rules it exists to keep. Whatever the scan *did* classify stays in the listing —
    /// dropping it, which is what returning `Err` from `list` amounts to, reports every one of
    /// those ids as absent. And the wording names no cause: a scan stops on a denial, a corrupt
    /// directory or an I/O fault alike, and only the `detail` handed in was established.
    pub(crate) fn scan_incomplete(location: &str, detail: &str) -> Self {
        Installed::Undetermined {
            name: None,
            reason: format!(
                "{location} could not be enumerated to the end ({detail}). Ownership is unknown \
                 for whatever the scan did not reach, so a Goetia daemon may be missing from this \
                 list."
            ),
        }
    }
}

/// The live state of one installed service, as [`ServiceManager::status`]
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// For `type: simple` on Windows this is `goetia-shim.exe`'s pid, not
    /// the supervised child's — SCM knows no other process for that
    /// service. Documented here rather than left as a surprise once Task 14
    /// lands.
    pub pid: Option<u32>,
    /// Whether the service is enabled at boot: systemd's `.wants` symlink,
    /// the launchd plist's directory, or SCM's `SERVICE_AUTO_START`. Not
    /// spec data (see the crate-level design notes on boot-enablement) —
    /// this reports the *installation's* current state, queried fresh each
    /// call, never cached from `install`. Present specifically so
    /// `manager::conformance` can assert `install` never enables and that a
    /// re-install never changes it; every real backend must be able to
    /// answer this without rebooting (systemd: `systemctl is-enabled`;
    /// launchd: which directory the plist lives in; SCM: `dwStartType`).
    pub enabled: bool,
}

/// A service's run state, as the platform's manager reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Stopped,
    Failed,
    Unknown,
}

// native ==============================================================================================================

/// The platform's [`ServiceManager`] implementation.
///
/// Linux returns [`Systemd`](crate::backend::systemd::manager::Systemd),
/// macOS [`LaunchdManager`](crate::backend::launchd::manager::LaunchdManager),
/// Windows [`ScmManager`](crate::backend::scm::manager::ScmManager). Any other
/// target returns [`Error::UnsupportedPlatform`] rather than panicking, so a
/// CLI user gets a diagnosable message instead of a crash.
pub fn native() -> Result<Box<dyn ServiceManager>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(crate::backend::systemd::manager::Systemd::new()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::backend::launchd::manager::LaunchdManager::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::backend::scm::manager::ScmManager::new()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(Error::UnsupportedPlatform {
            platform: std::env::consts::OS.to_string(),
        })
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
