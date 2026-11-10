//! The `ServiceManager` seam: the trait the CLI is written against, and
//! [`fake::Fake`], the in-memory test double every CLI and policy test in this
//! crate runs against instead of a real systemd/launchd/SCM.
//!
//! The trait exists so the CLI is written once and tested without touching a
//! real machine, not for runtime backend swapping. Each backend implements
//! the crate-private `Backend` trait, and a blanket impl makes every
//! `Backend` a `ServiceManager`, so `install`, `preview_install`, `status`
//! and `list` are written once, in `flow`, and [`crate::decide::decide`] is
//! called only from there. [`conformance::run`] asserts the contract against
//! any `&dyn ServiceManager`.

pub mod budget;
pub mod conformance;
pub mod fake;
pub(crate) mod flow;

pub use budget::Budget;

use crate::error::{Error, Reason, Result};
use crate::spec::{DaemonSpec, Id};

// ServiceManager ======================================================================================================

/// One platform's service manager, seen through the operations Goetia needs.
///
/// `install`/`uninstall`/`enable`/`disable`/`start`/`stop`/`kill` are the mutating
/// verbs; `status`/`list` are read-only. The CLI checks elevation itself
/// before calling any mutating verb — backends do not re-check it.
pub trait ServiceManager {
    /// Install (create or update) the service for `spec`. Never starts and
    /// never enables it at boot. [`crate::decide::decide`] chooses the outcome; see
    /// that function's doc comment for what each [`crate::decide::Outcome`]
    /// means and when `force` is honored.
    ///
    /// Never changes whether the service is running, with one launchd
    /// exception: a `Create` over a job already loaded under the same label
    /// (no goetia plist on disk) boots that job out. A changed spec takes
    /// effect at the service's next start where the platform allows that
    /// (systemd, SCM). launchd cannot update a loaded job, so `install` returns
    /// an error and writes nothing; if the job turns out loaded only after the
    /// write, it restores the previous plist and returns
    /// [`Error::LoadedJobInDoubt`](crate::error::Error::LoadedJobInDoubt) or
    /// [`Error::LoadedStateInDoubt`](crate::error::Error::LoadedStateInDoubt)
    /// (exit `4`).
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<crate::decide::Outcome>;

    /// What [`Self::install`] would do for `spec`, without doing it, always
    /// as if `force` were `false`: the forced outcome would hide the conflict
    /// `--force` exists to let a user decide about. Backs `goetia daemon diff`.
    /// The same flow as `install` minus the write, so the two cannot disagree.
    fn preview_install(&self, spec: &DaemonSpec) -> Result<crate::decide::Outcome>;

    /// Stop the service if running, remove the artifact, and reload the
    /// manager. Operates by id alone, so it works without the original manifest.
    fn uninstall(&self, id: &Id) -> Result<()>;

    /// Enable the service at boot. Does not start it. `Err(NotInstalled)` if
    /// `id` is not managed by Goetia.
    fn enable(&self, id: &Id) -> Result<()>;

    /// Disable the service at boot. Does not stop it if running.
    fn disable(&self, id: &Id) -> Result<()>;

    /// Start the service now, waiting up to `budget` for it. Does not
    /// change its boot-enablement.
    ///
    /// `Ok(())` means **the platform's service manager reports the service
    /// as running, as far as it knows**, not that it is ready to serve. An
    /// implementation must not fabricate that confirmation.
    ///
    /// **`budget` bounds waiting for that confirmation, never whether the
    /// request is sent.** Every real backend starts the clock when the verb
    /// begins, after the ownership read, and issues the request whatever is
    /// left of it, cutting short only the wait for the manager's answer. A
    /// budget that is already spent still reaches the manager.
    ///
    /// [`Budget::Immediate`] issues the request and returns without waiting,
    /// establishing nothing: `Ok(())` then means only that the request was
    /// accepted, so neither a caller nor the implementation may read any state
    /// from it. Every state assertion a backend makes belongs behind
    /// [`Budget::waits`].
    ///
    /// **Where a platform offers no request-only form, [`Budget::Immediate`]
    /// still performs the full blocking call.** launchd's `launchctl
    /// print`/`bootstrap` have no non-blocking spelling: a `start` with no
    /// budget still probes and, if needed, bootstraps the job to completion
    /// before the plain `kickstart` that declines to wait. On macOS, stopping,
    /// restarting, uninstalling and some starts and installs wait for launchd
    /// to remove a job; see the README's "Waiting for a start or stop". A
    /// `start`'s own wait for a removal is not bounded by the budget,
    /// `Immediate` included.
    ///
    /// An answer from the manager that itself establishes the goal is the
    /// confirmation, whatever is left of the budget: launchd's `print` of a
    /// job already running, read before any request is needed, and SCM's
    /// `ERROR_SERVICE_ALREADY_RUNNING` reply over a service it reports
    /// `RUNNING`. systemd's answer to a start is a job, never a state, so a
    /// unit already active is confirmed only when that job completes: under a
    /// budget shorter than the job's round trip (tens of milliseconds), the
    /// same start of a running daemon expires on systemd and succeeds on the
    /// other two.
    ///
    /// On expiry, [`Error::WaitTimeout`], never `Ok(())` and never
    /// [`Error::Undetermined`], which claims the *installation* is in doubt
    /// when a settled id's request was issued and not waited out.
    ///
    /// Idempotent under every budget: starting an already-running service is
    /// `Ok(())`, not an error.
    ///
    /// [`Error::WaitTimeout`]: crate::Error::WaitTimeout
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    fn start(&self, id: &Id, budget: Budget) -> Result<()>;

    /// Make ready, before the first of `steps` sends anything, whatever they could need under
    /// `budget` that might run out between two of them. A failure here means nothing was sent;
    /// later it would strand the daemon halfway (e.g. stopped by `restart` with no start sent). The
    /// returned [`Sequence`] draws on it, nothing else does; dropping it releases it. A verb outside
    /// a sequence reserves for itself. Each prepared step is used once (see [`Sequence`]).
    fn prepare(&self, steps: &[Step], budget: Budget) -> Result<Box<dyn Sequence + '_>>;

    /// `daemon restart --timeout 0` as one request, where the platform has
    /// one: the manager's own restart, confirming nothing. `None`,
    /// the default, where it has none — the caller then issues [`Self::stop`]
    /// under [`Budget::Immediate`] and [`Self::request_start_after_stop`].
    ///
    /// Only a single request is free of the window between two: systemd
    /// replaces a stop job it has not yet run with a start, which on a unit
    /// still active completes at once, restarting nothing.
    /// `Some(Ok(()))` means the manager accepted the request and nothing more.
    fn request_restart(&self, _id: &Id) -> Option<Result<()>> {
        None
    }

    /// `daemon restart --timeout 0`'s second step, where there is no
    /// [`Self::request_restart`]: request a start right after a stop request
    /// nobody waited for, and return.
    ///
    /// **Unlike [`Self::start`], not idempotent over a service that is still
    /// up.** A manager answering "already running" here has refused the
    /// start: the instance it sees may be the one the stop is still taking
    /// down (a Windows `type: simple` service reads `RUNNING` until its shim
    /// has taken the stop), and reading that answer as a start would report a
    /// restart that leaves the daemon down. `Ok(())` means the manager
    /// accepted a start request and nothing more. Takes no budget: it never
    /// waits.
    fn request_start_after_stop(&self, id: &Id) -> Result<()>;

    /// Stop the service now, waiting up to `budget` for it. Does not change
    /// its boot-enablement.
    ///
    /// `Ok(())` means the service manager reports the service as stopped, on
    /// the same terms as [`Self::start`]; `budget` bounds only the wait, as
    /// there; and an expiry is [`Error::WaitTimeout`] on the same terms too.
    ///
    /// **Where a platform offers no request-only form,
    /// [`Budget::Immediate`] still performs the full blocking call.**
    /// On macOS, stopping, restarting, uninstalling and some starts and
    /// installs wait for launchd to remove a job; see the README's "Waiting
    /// for a start or stop". A `stop` under `Immediate` waits without bound
    /// there; under a bounded budget it gives up at the deadline.
    ///
    /// Idempotent: stopping an already-stopped service is `Ok(())`, not an
    /// error. `daemon restart` depends on this for a daemon that was never
    /// started, and `launchctl bootout` and `ControlService(STOP)` on an
    /// inactive service fail where `systemctl stop` does not, so an
    /// implementation papers over that difference itself.
    ///
    /// [`Error::WaitTimeout`]: crate::Error::WaitTimeout
    fn stop(&self, id: &Id, budget: Budget) -> Result<()>;

    /// End the service's running process as a failure, and return once that is delivered. Does not
    /// wait for the process to be gone, and does not change its boot-enablement. What happens next
    /// is the manager's restart policy: a `restart: always` daemon comes back, a `restart: never`
    /// one stays down.
    ///
    /// Addresses the service, never a bare pid: systemd signals the unit's main process with
    /// `SIGKILL` (`systemctl kill`), launchd signals the job (`launchctl kill`), and SCM, which has
    /// no kill-by-service call, reads the service's process and terminates it only after SCM
    /// confirms it is still the service's. Descendants are the manager's to clean up: the cgroup,
    /// the process group, or `goetia-shim`'s Job Object.
    ///
    /// [`Error::NotRunning`] where the service has no process, **classified off the kill request's
    /// own answer** and never off a state read before it, which would race the kill. Not an
    /// `Ok(())`: nothing was killed.
    ///
    /// SCM returns once it has recorded the exit: the service is then stopped, or already restarted
    /// by its restart policy. systemd and launchd return once the signal is sent.
    ///
    /// Takes no budget: what SCM waits for is the exit of a process whose termination was
    /// delivered, which is not a wait to bound.
    ///
    /// [`Error::NotRunning`]: crate::Error::NotRunning
    fn kill(&self, id: &Id) -> Result<()>;

    /// The live state of one installed service. `Err` for an id whose blob
    /// will not decode, never a plausible-looking `Status` for state it
    /// cannot determine (see `Installed::OursUnusable`, which exists for the
    /// same reason on the `list` side); [`Error::Undetermined`] for an id whose
    /// artifact could not be read at all, where not even ownership was
    /// established. [`Error::NoManager`] where no manager can be asked, which
    /// the CLI reports as `unavailable` under the id.
    ///
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    /// [`Error::NoManager`]: crate::Error::NoManager
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
    /// document, the rendering that says goetia could not answer, and putting
    /// a settled answer behind it is the same conflation
    /// [`Installed::Undetermined`] exists to end, arrived at from the other
    /// side. A scan that *started* and did not finish is the opposite case
    /// and does report (see `Installed::scan_incomplete`). So does an entry
    /// whose live state could not be queried, as `OursUnusable` — except
    /// where no manager can be asked for any entry's: that is
    /// [`Error::NoManager`], for the whole listing.
    ///
    /// [`Error::NoManager`]: crate::Error::NoManager
    fn list(&self) -> Result<Vec<Installed>>;
}

// Sequence ============================================================================================================

/// One verb that can draw on a reservation — see [`ServiceManager::prepare`]. A verb outside a
/// sequence reserves for its own step; one inside draws on what the sequence made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Install,
    Start,
    Stop,
    Kill,
    Uninstall,
    RequestStartAfterStop,
}

/// What [`ServiceManager::prepare`] made ready, and the verbs that draw on it, with the contracts
/// of the [`ServiceManager`] verbs of the same names. Releases what it did not spend when dropped.
///
/// A verb may be run only for a step that was prepared, and each prepared step once: [`Step::Stop`]
/// by `stop`, [`Step::Start`] by `start` or, where the start is only requested, by
/// `request_start_after_stop`. Anything else was not counted for and fails a debug assertion; in a
/// release build it makes what it needs on the spot, which can fail between legs. A verb the
/// ownership gate refuses draws nothing, so its step stays unspent.
#[must_use = "what was prepared is released as soon as this is dropped"]
pub trait Sequence {
    /// See [`ServiceManager::stop`].
    fn stop(&mut self, id: &Id, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::start`].
    fn start(&mut self, id: &Id, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::request_start_after_stop`].
    fn request_start_after_stop(&mut self, id: &Id) -> Result<()>;
}

// Installed / Status / State ==========================================================================================

/// One entry from [`ServiceManager::list`].
#[derive(Debug)]
pub enum Installed {
    /// A Goetia-marked service whose blob decoded successfully.
    Ours {
        spec: DaemonSpec,
        state: State,
        /// The number [`ServiceManager::status`] would report for this id
        /// at this moment; see `Backend::live_status`.
        pid: Option<u32>,
        enabled: bool,
    },
    /// Ours by marker, but goetia cannot report on it. `reason` is why: goetia's own words, the
    /// error under them, or both. The causes are listed on [`Ownership::OursUnusable`]. One such
    /// artifact must not take down `list`/`status` for every other daemon.
    ///
    /// [`Ownership::OursUnusable`]: crate::decide::Ownership::OursUnusable
    OursUnusable { name: String, reason: Reason },
    /// Neither proof: the read that would have classified this id did not
    /// complete, so goetia established neither its absence nor its
    /// presence. The `list`-side counterpart of [`Error::Undetermined`],
    /// separated from the two variants above by what goetia *established*.
    /// `OursUnusable` for a unit file an unelevated caller merely could not
    /// open would claim ownership of what may be a stranger's service;
    /// omitting it would claim it does not exist.
    ///
    /// `name` is `None` only for an entry standing for more than one id. That
    /// happens for either of two reasons, and a caller must not assume the
    /// first: an enumeration that did not finish knows neither the names it
    /// never reached nor how many there were (see `Installed::scan_incomplete`),
    /// *or* one that enumerated fine drops the names because one entry cannot
    /// usefully carry hundreds, as Windows SCM does on an unelevated `list`,
    /// where most services deny a `Parameters` read at once. An entry for
    /// exactly one known id always names it.
    ///
    /// What a `None` obliges of every caller: **while such an entry is
    /// present, no negative conclusion about any id is sound.** It may stand
    /// for the very id being asked about, so "`x` is absent" does not follow
    /// from `x` going unnamed, whether for rendering, an exit code or a test
    /// assertion.
    ///
    /// Carries no `recovery`: that text belongs to [`Error::Undetermined`],
    /// and a second independently worded remedy per `list` entry would put
    /// two answers on one condition.
    ///
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    Undetermined { name: Option<String>, reason: Reason },
}

impl Installed {
    /// The reason `list` prints for an [`OursUnusable`](Installed::OursUnusable) entry, exactly as
    /// it prints it ([`Reason`]'s `Display`: escaped, a child's stderr laid out as the report lays
    /// it out); `None` for any other entry.
    pub fn unusable_reason(&self) -> Option<String> {
        match self {
            Installed::OursUnusable { reason, .. } => Some(reason.to_string()),
            _ => None,
        }
    }

    /// The aggregate entry an enumeration that stopped early leaves behind: `location` is what
    /// was being scanned, `detail` what did not complete there. Every backend builds it here, so
    /// a scan cut short in a unit directory, a `LaunchDaemons` directory or the SCM registry key
    /// says one thing rather than three.
    ///
    /// Whatever the scan *did* classify stays in the listing; returning `Err` from `list` would
    /// report those ids as absent. The wording names no cause: a scan stops on a denial, a corrupt
    /// directory or an I/O fault alike, and only the `detail` handed in was established.
    pub(crate) fn scan_incomplete(location: &str, detail: Error) -> Self {
        Installed::Undetermined {
            name: None,
            reason: Reason::around(
                format!("{location} could not be enumerated to the end ("),
                detail,
                "). Ownership is unknown for whatever the scan did not reach, so a Goetia daemon may be missing \
                 from this list.",
            ),
        }
    }
}

/// The live state of one installed service, as [`ServiceManager::status`]
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// For `type: simple` on Windows this is `goetia-shim.exe`'s pid, not the
    /// daemon's: SCM knows no other process for that service.
    pub pid: Option<u32>,
    /// Whether the service is enabled at boot: systemd's `.wants` symlink,
    /// the launchd plist's directory, or SCM's `SERVICE_AUTO_START`. Not spec
    /// data: this is the *installation's* current state, queried fresh each
    /// call, never cached from `install`, so `manager::conformance` can assert
    /// that `install` never enables and a re-install never changes it. Every
    /// real backend must answer it without rebooting (systemd: `systemctl
    /// is-enabled`; launchd: which directory the plist lives in; SCM:
    /// `dwStartType`).
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
/// macOS [`Launchd`](crate::backend::launchd::manager::Launchd),
/// Windows [`Scm`](crate::backend::scm::manager::Scm). Any other
/// target returns [`Error::UnsupportedPlatform`] rather than panicking.
pub fn native() -> Result<Box<dyn ServiceManager>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(crate::backend::systemd::manager::Systemd::new()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::backend::launchd::manager::Launchd::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::backend::scm::manager::Scm::new()))
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
