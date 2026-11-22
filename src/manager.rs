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
pub(crate) mod output;

pub use budget::Budget;

use std::io::Write;
use std::time::SystemTime;

use crate::error::{Error, Reason, Result};
use crate::spec::{DaemonKind, DaemonSpec, Id};

// RunInFlight =========================================================================================================

/// What [`ServiceManager::uninstall`] does with a run in flight: the service's running process, or a
/// task's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunInFlight {
    /// Stop it, then remove the daemon.
    Stop,
    /// Send it nothing and wait for nothing of it: remove the daemon and everything that would start
    /// another run. The caller may be the run itself, or a process it started.
    Leave,
}

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
    /// [`Error::LoadedJobInDoubt`] or
    /// [`Error::LoadedStateInDoubt`]
    /// (exit `4`).
    ///
    /// A run due while `install` changes an enabled task's schedule is missed on systemd and
    /// launchd: the task is disarmed and armed again around the change, and arming makes up no run
    /// the schedule named meanwhile. `goetia daemon start <id>` runs it.
    ///
    /// On Windows, changing a task's account is refused while a run of it is alive or its run records are
    /// held (by a run that is starting, a `run` or `stop` of it, or another program); the error says which,
    /// and the install can be retried when that ends. A scheduled run that comes due during the change does
    /// not run, even when the change is then refused. A failure after the old task was removed leaves
    /// nothing installed, and the error says so: run the install again.
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<crate::decide::Outcome>;

    /// What [`Self::install`] would do for `spec`, without doing it, always
    /// as if `force` were `false`: the forced outcome would hide the conflict
    /// `--force` exists to let a user decide about. Backs `goetia daemon diff`.
    /// The same flow as `install` minus the write, so the two cannot disagree.
    fn preview_install(&self, spec: &DaemonSpec) -> Result<crate::decide::Outcome>;

    /// Remove the artifact, and reload the manager. Operates by id alone, so it works without the
    /// original manifest. `in_flight` is what it does with a run in flight:
    /// [`RunInFlight::Stop`] stops it first, and [`RunInFlight::Leave`] sends it nothing and waits for
    /// nothing of it (`goetia daemon uninstall --no-stop`). The definition and everything that would
    /// start another run are removed either way. With no run in flight the two do the same, so a run
    /// is never left for want of one. Which backends honour `Leave`, and what each leaves while the
    /// run lives, is the README's "Uninstalling a daemon".
    ///
    /// **systemd** leaves the unit loaded with no file, which systemd never restarts, and sends it nothing in any
    /// state. One read of the unit decides what else: a run still in its start job, which a reload would end at
    /// the default start timeout, gets a stand-in unit of the same name under `/run/systemd/system`, written
    /// first (see `generate::standin`), when it is the run of the definition being removed; a restart that is
    /// waiting is cleared once the reload has failed it; a run past its start job is left as it is. The next
    /// `install` or `uninstall` of the id removes the stand-in once its run has no process. A file at that path
    /// that is not goetia's is refused with [`Error::StandinBlocked`], whether or not a run is in flight. A run
    /// somebody else starts between the backend's read and the reload is that caller's own conflicting action,
    /// and nothing guards it.
    ///
    /// On Windows a `protocol: plain` service leaves shim files behind (see
    /// `EmbeddedStockShim`), and this removes the ones no other
    /// service or process uses once its own host has exited. If that cleanup fails the service is
    /// still deleted and the error says so: re-run `uninstall` to retry it. `uninstall` of an id that
    /// is not installed is the same retry, and reports [`Error::NotInstalled`]
    /// when it has nothing left to do.
    ///
    /// A Windows task's `uninstall` disables the task, so no scheduled run starts from then on, and stops
    /// every live run before it deletes the task. One that fails or is interrupted leaves the task disabled
    /// or, after the delete, only its run records: re-run `uninstall`, or run the install again with
    /// `--force`. Once it has closed the task's run records, `enable`, `disable`, `run`, and a `stop` that
    /// finds a run to wait for refuse until one of those two has finished.
    fn uninstall(&self, id: &Id, in_flight: RunInFlight) -> Result<()>;

    /// Enable the service at boot. Does not start it. `Err(NotInstalled)` if
    /// `id` is not managed by Goetia. A task's `enable` arms its schedule; a task with no schedule
    /// has nothing to arm, and its `enable` is [`Error::Invalid`] before the manager is asked. Arming
    /// again starts no run for a time the schedule named while goetia's `disable` had the task
    /// disarmed: a missed start is made up only for time the machine was off or asleep.
    fn enable(&self, id: &Id) -> Result<()>;

    /// Disable the service at boot. Does not stop it if running. A task's `disable` disarms its
    /// schedule and leaves a run in flight.
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
    /// before the plain `kickstart` that declines to wait.
    /// On macOS a `stop` waits for launchd to remove the job, and a budget that
    /// does not wait (`Immediate`, a zero bound) does not bound that wait; a
    /// `start` waits for a job still going away before it loads it. See the
    /// README's "Waiting for a start or stop".
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
    /// On expiry, the error's [`cause`](Error::cause) is [`Error::WaitTimeout`], never `Ok(())` and
    /// never [`Error::Undetermined`], which claims the *installation* is in doubt when a settled
    /// id's request was issued and not waited out. A backend may wrap the timeout, and
    /// [`cause`](Error::cause) finds it: match the cause, not the error.
    ///
    /// Idempotent under every budget: starting an already-running service is
    /// `Ok(())`, not an error.
    ///
    /// **A task's start requests a run and returns once the manager has accepted it:** `budget` is
    /// not read, and `Ok` says nothing of the run.
    ///
    /// The answer is the kind of what was started, read from the definition the ownership gate
    /// passed: `None` where it will not decode, which does not stop a start.
    ///
    /// [`Error::WaitTimeout`]: crate::Error::WaitTimeout
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    fn start(&self, id: &Id, budget: Budget) -> Result<Option<DaemonKind>>;

    /// Run the task `id` once, now, and follow that run: request a start, copy what the run writes to
    /// `out` as it is written, and return how the run ended once it has.
    ///
    /// Only an idle task is run, so that `run` never joins or restarts a run someone else began:
    /// [`Error::NotATask`] for a service, [`Error::AlreadyRunning`] for a task in flight and
    /// [`Error::RunStateUnknown`] where the manager does not say. The state is read once, before the
    /// request: a run begun between that read and the request is one the request may join, which the
    /// manager decides. No request is sent after a refusal.
    ///
    /// Everything the run wrote before it ended has been written to `out` when `run` returns. A task
    /// with no `logs` file has no output to show: one note says so, and `run` waits for the run to end.
    /// [`Error::RunNotStarted`] means the manager ended the start before a run began (no Starting entry);
    /// [`Error::RunUnrecorded`] that the run's exit could not be established; a wait that lost track of
    /// the run is [`Error::RequestInDoubt`], which does not say it ended. If showing the
    /// output fails the run is still waited for: the wait's error is returned if it failed, else a
    /// [`Error::Step`] that names the exit and holds the failure.
    fn run(&self, id: &Id, out: &mut dyn Write) -> Result<Exit>;

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
    /// [`Self::request_restart`]: request a start right after a stop under
    /// `Immediate` (which on macOS has waited for the job's removal), and
    /// return.
    ///
    /// **Unlike [`Self::start`], not idempotent over a service that is still
    /// up.** A manager answering "already running" here has refused the
    /// start: the instance it sees may be the one the stop is still taking
    /// down (a Windows `protocol: plain` service reads `RUNNING` until its shim
    /// has taken the stop), and reading that answer as a start would report a
    /// restart that leaves the daemon down. launchd answers a `kickstart` of
    /// a task that still has its run as it answers a start, and Task Scheduler
    /// accepts a `Run` over a live run and begins nothing, so there a task's
    /// start reads the run first and is refused with [`Error::StillRunning`],
    /// sending nothing, and one whose read fails is [`Error::StopUnconfirmed`] (launchd), also sending nothing.
    /// `Ok(())` means the manager
    /// accepted a start request and nothing more. Takes no budget: it never
    /// waits.
    fn request_start_after_stop(&self, id: &Id) -> Result<()>;

    /// Stop the service now, waiting up to `budget` for it, or, with `None`, for as long as the daemon's
    /// own stop takes ([`Budget::for_stop`]): its `stop-action` `timeout` plus
    /// [`StopAction::TEARDOWN_MARGIN`](crate::spec::StopAction::TEARDOWN_MARGIN), without bound where it has
    /// no `timeout`, and [`Budget::DEFAULT`] for `send-signal: kill`. `None` is read from the definition this
    /// call reads and acts on, so the wait and the stop cannot disagree; a definition that does not decode
    /// has none to read, and `None` is then [`Error::StopActionUnreadable`], over the decode failure.
    /// Does not change its boot-enablement.
    ///
    /// `Ok(())` means the service manager reports the service as stopped, on
    /// the same terms as [`Self::start`]; `budget` bounds only the wait, as
    /// there; and an expiry is [`Error::WaitTimeout`] on the same terms too. A backend whose service
    /// can report stopped before its process has exited (SCM) also waits for that exit, under the
    /// same `budget`; its expiry is [`Error::ProcessWaitTimeout`], the service being stopped.
    ///
    /// **Where a platform offers no request-only form,
    /// [`Budget::Immediate`] still performs the full blocking call.**
    /// On macOS a `stop` waits for launchd to remove the job, and a budget that
    /// does not wait (`Immediate`, a zero bound) does not bound that wait; a
    /// `start` waits for a job still going away before it loads it. See the
    /// README's "Waiting for a start or stop".
    ///
    /// **A task's stop ends the run in flight and leaves its schedule armed;** the task is then
    /// [`State::Idle`]. On macOS, a task's `stop` is always issued, as on the other backends, whatever the
    /// budget (`Immediate` and a zero bound included): the run in flight is sent its `stop-action`'s signal
    /// (SIGTERM, or SIGKILL for `send-signal: kill`), after a wait for a start launchd has accepted to begin,
    /// so it can end it. It then waits until the run ends (bounded by `budget`; `Immediate` does not wait). A
    /// run that has not answered SIGTERM when the action's `timeout` is over is sent SIGKILL, if the budget
    /// lasts that long. Once the first signal is sent, a `Bounded` deadline that is spent sends nothing more:
    /// no SIGKILL, and no first signal to a run that began later. The stop then reads once, and returns `Ok`
    /// if the job is idle, else [`Error::WaitTimeout`], what it sent standing; a task that ignores SIGTERM past
    /// the budget keeps running, and `kill` ends it. The deadline is decided right before each send, not
    /// atomically with it. `stop` also ends a run that begins while it is stopping. If launchd shows the job
    /// with no process in a state that does not say it is idle, or the read fails, the result is
    /// [`Error::StopUnconfirmed`]. Under `Immediate`, a pid launchd shows that the stop cannot reach (it has
    /// ended, or it is not the job's) is followed at once by another reading: another pid is attempted like the
    /// first, and the same pid twice in a row is the same error, with nothing sent and a word to run the stop
    /// again.
    ///
    /// Idempotent: stopping an already-stopped service is `Ok(())`, not an
    /// error. `daemon restart` depends on this for a daemon that was never
    /// started, and `launchctl bootout` and `ControlService(STOP)` on an
    /// inactive service fail where `systemctl stop` does not, so an
    /// implementation papers over that difference itself.
    ///
    /// [`Error::WaitTimeout`]: crate::Error::WaitTimeout
    /// [`Error::ProcessWaitTimeout`]: crate::Error::ProcessWaitTimeout
    /// [`Error::StopUnconfirmed`]: crate::Error::StopUnconfirmed
    fn stop(&self, id: &Id, budget: Option<Budget>) -> Result<()>;

    /// End the process the manager reports as the service's, as a failure, and return once it has
    /// exited and the manager no longer reports it: on every backend, the next `status` does not show
    /// the killed process. Does not change the service's boot-enablement. What happens next is the
    /// manager's restart policy: a `restart: always` daemon comes back, a `restart: never` one stays
    /// down. A caller that wants the next process calls [`start`](Self::start) after `kill`, which
    /// returns once the service runs, starting it now if the policy's delay has not yet. The restart
    /// does not depend on the caller surviving the kill: the signal is sent before any wait.
    ///
    /// Addresses the service, never a bare pid: the manager is asked for the service's process, which
    /// is held (a pidfd on Linux, an audit token on macOS, a handle on Windows) and asked for again
    /// before it is signalled, so the signal reaches that process and never a stranger that took its
    /// pid. systemd and launchd send `SIGKILL`; SCM, which has no kill-by-service call, terminates the
    /// process. Descendants are the manager's to clean up: the cgroup, the process group, or
    /// `goetia-shim`'s Job Object.
    ///
    /// The process killed is the one the manager reported when the kill began. If it ended on its own
    /// in the moment before the signal landed, the kill still returns `Ok` once the manager has
    /// recorded that end, and the restart policy decides what follows, as it would had the process
    /// exited a moment before `kill` was called. Nothing chases a service that restarts under the
    /// kill.
    ///
    /// [`Error::NotRunning`] where the service has no process, **classified off the read that starts
    /// the kill**: the manager reports none. Not an `Ok(())`: nothing was killed. A launchd task whose
    /// accepted start has not forked yet (which `status` reports as running, after `kickstart`
    /// returned) has no process to kill, so its `kill` waits, under no budget, for the pid and then
    /// kills it; where the start does not happen, or the job was idle, it is `NotRunning`. An error
    /// after the signal was sent says so.
    ///
    /// Takes no budget. The exit of a process whose kill was delivered is an event, not a wait to
    /// bound, on every backend; a process the kernel holds (one in uninterruptible sleep, on a hung
    /// mount or in a stuck driver) cannot exit, and the wait for it lasts while it does, as `stop`'s
    /// does. The CLI's wait ends on Ctrl-C with the signal standing.
    ///
    /// [`Error::NotRunning`]: crate::Error::NotRunning
    fn kill(&self, id: &Id) -> Result<()>;

    /// The live state of one installed service: `state`, `pid` and `enabled` are read through
    /// [`Status::common`]. `Err` for an id whose blob
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

    /// Whether `id` is a service or a task, read from the definition goetia installed it with, and
    /// nothing else of its status. The ownership gate applies as for any verb ([`Error::NotInstalled`],
    /// [`Error::Undetermined`]); a definition that will not decode is its error ([`Error::Kept`]).
    ///
    /// [`Error::NotInstalled`]: crate::Error::NotInstalled
    /// [`Error::Undetermined`]: crate::Error::Undetermined
    /// [`Error::Kept`]: crate::Error::Kept
    fn kind(&self, id: &Id) -> Result<DaemonKind>;

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
/// A verb may be run only for a step that was prepared, and each prepared step once: [`Step::Install`]
/// by `install`, [`Step::Stop`] by `stop`, [`Step::Start`] by `start` or, where the start is only
/// requested, by `request_start_after_stop`. Anything else was not counted for and fails a debug
/// assertion; in a release build it makes what it needs on the spot, which can fail between legs. A
/// verb the ownership gate refuses draws nothing, so its step stays unspent. `enable` draws nothing
/// and spends no step.
///
/// The steps of one sequence share its reservation, and with it whatever a backend keeps there: the
/// answer of systemd's version gate, asked once for the install, its `enable` and its start.
#[must_use = "what was prepared is released as soon as this is dropped"]
pub trait Sequence {
    /// See [`ServiceManager::install`].
    fn install(&mut self, spec: &DaemonSpec, force: bool) -> Result<crate::decide::Outcome>;

    /// See [`ServiceManager::enable`], including the refusal of a task with no schedule.
    fn enable(&mut self, id: &Id) -> Result<()>;

    /// See [`ServiceManager::stop`], including [`Error::StopActionUnreadable`] for a `None` budget, which
    /// spends no step.
    fn stop(&mut self, id: &Id, budget: Option<Budget>) -> Result<()>;

    /// See [`ServiceManager::start`].
    fn start(&mut self, id: &Id, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::request_start_after_stop`].
    fn request_start_after_stop(&mut self, id: &Id) -> Result<()>;
}

// Installed / Status / State ==========================================================================================

/// One entry from [`ServiceManager::list`].
// `Ours` embeds a full `DaemonSpec` and its status; boxing it would ripple into every match arm of the
// 90 sites that read one, and `list` builds one per daemon, never in a hot loop. Same call as
// `decide::Ownership`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Installed {
    /// A Goetia-marked service whose blob decoded successfully.
    Ours {
        spec: DaemonSpec,
        /// What [`ServiceManager::status`] would report for this id at this moment; see
        /// `Backend::live_status`. Of the same kind as `spec`.
        status: Status,
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

/// The live state of one installed daemon, as [`ServiceManager::status`] reports it. Mirrors
/// [`DaemonSpec`]: one variant per [`DaemonKind`], each carrying that kind's status.
///
/// Read what every kind has through [`Status::common`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Service(DaemonStatus),
    Task(TaskStatus),
}

impl Status {
    /// The fields every kind reports.
    pub fn common(&self) -> &DaemonStatus {
        match self {
            Status::Service(status) => status,
            Status::Task(status) => &status.common,
        }
    }

    /// Which kind of daemon this is the status of.
    pub fn kind(&self) -> DaemonKind {
        match self {
            Status::Service(_) => DaemonKind::Service,
            Status::Task(_) => DaemonKind::Task,
        }
    }
}

/// How a task's run ended: the code the program exited with, or the signal that killed it.
///
/// Serializes as `{"code": n}` or `{"signal": n}`, the `--json` form of `last-result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Code(code) => write!(f, "exit code {code}"),
            Exit::Signal(signal) => write!(f, "signal {signal}"),
        }
    }
}

/// A task's status: the common fields, and what the manager knows of its runs. A field the
/// manager cannot say is `None`: no run yet, a run still in flight, or a manager that does not keep
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStatus {
    pub common: DaemonStatus,
    /// How the most recent finished run ended.
    pub last_result: Option<Exit>,
    /// When the most recent run started.
    pub last_run: Option<SystemTime>,
    /// When the manager will start the task next; `None` for a task with no schedule armed.
    pub next_run: Option<SystemTime>,
}

/// The status fields every kind of daemon reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub state: State,
    /// For `protocol: plain` on Windows this is `goetia-shim.exe`'s pid, not the
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

/// A daemon's run state, as the platform's manager reports it. A service is never `Idle` and a
/// task is never `Stopped`: a task between runs is `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Idle,
    Stopped,
    Failed,
    Unknown,
}

// native ==============================================================================================================

/// The platform's [`ServiceManager`] implementation.
///
/// Linux returns `Systemd`,
/// macOS `Launchd`,
/// Windows `Scm`. Any other
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
