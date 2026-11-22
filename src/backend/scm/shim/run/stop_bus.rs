//! The stop signal shared between the SCM control-handler callback and the
//! service thread, and the composition that lets that thread block on either
//! "the child exited" or "a stop was requested" without polling.
//!
//! Process-tree waiting is `cosca::Child`'s job (`service.rs` spawns and
//! contains the daemon with it); this module supplies the piece `cosca` does not: a way to
//! interrupt a blocking `child.wait()` when SCM delivers `Stop`. It also
//! performs the actual tree teardown once a stop is observed — see
//! [`Supervisor::wait_for_child_or_stop`]'s doc comment for why that has to
//! happen here rather than back in the caller.
//!
//! The thread that performs that blocking wait is made before the daemon is
//! spawned, never after — see [`Waiter`].
//!
//! [`StopBus`] is only the gate. [`Supervisor`] holds it with the [`Os`] and [`Log`] the run uses.

use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cosca::CtrlEvent;
use cosca::containment::TreeDrain;

use crate::backend::scm::exit_status::Reported;

use super::logging::Log;
use super::os::{Os, Spawned};
use crate::backend::standby::Standby;
use crate::error::{Chain, os_error_in};
use crate::spec::{DaemonSpec, StopSignal};

/// How the run ends, decided once under the [`StopBus`] mutex: a stop accepted before the exit is
/// decided wins over any failure; an exit decided first is reported whatever stop follows. SCM
/// acknowledges that later stop anyway, and restarts the service if the restart policy covers the
/// exit.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Fate {
    #[default]
    Open,
    /// A stop was accepted: the run reports clean (or the incomplete-stop record).
    Stopped,
    /// A failure was decided: a later stop does not change it.
    Failed,
}

#[derive(Default)]
struct Gate {
    fate: Fate,
    /// Set by the waiter when its `child.wait()` returns. Reset by [`Supervisor::wait_for_child_or_stop`]
    /// before each wait.
    child_done: bool,
    /// Set by the waiter when its `child.wait()` failed: the OS error code, if the failure has one.
    /// `Some` means the daemon's fate is unknown and nothing else is watching it.
    wait_failure: Option<Option<i32>>,
}

/// The gate between the SCM control handler, the service thread and the waiter thread. It holds
/// the gate and nothing else, so the control handler needs no [`Os`] and no [`Log`]. Only
/// [`Supervisor::new`] makes one outside this module's tests, so a run's bus is the one its
/// waiters report to.
pub(crate) struct StopBus {
    state: Mutex<Gate>,
    cvar: Condvar,
}

/// What ended [`Supervisor::wait_for_child_or_stop`]'s wait, as `on_stopping` is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEnd {
    /// A stop was accepted (even if the daemon exited as well: the stop wins).
    Stop,
    /// The daemon exited, or its wait failed, with no stop accepted.
    Exit,
}

/// What an accepted stop does before the teardown. One function runs it
/// ([`Supervisor::wait_for_child_or_stop`]); a plan only says how much of it. A run derives its plan once, from its
/// spec ([`StopPlan::of`]), and hands it to everything that needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopPlan {
    /// Ask the daemon to exit with `event`, then wait for its whole tree for `timeout` (`None`: as long as it
    /// takes), counted from when the supervisor saw the stop. A service whose `send-signal` asks.
    AskAndWait {
        event: CtrlEvent,
        timeout: Option<Duration>,
    },
    /// No ask, no wait: the teardown at once. `send-signal: kill` and `wm-close` (it has no send yet).
    AtOnce,
    /// No ask; wait for the daemon's own exit with no cap of goetia's, then the teardown. For a task, whose stop is
    /// the console close event; see the module doc of `shim::run::task`.
    WaitOnly,
}

/// What a spawn sets for a stop, decided by the plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SpawnSetup {
    /// The console event the cooperative stop sends: set only where the plan asks.
    pub(super) ask_with: Option<CtrlEvent>,
    /// `Some(false)`: no process group of its own, which is what lets the event reach the daemon (a child that
    /// leads a group starts with Ctrl+C ignored, and Ctrl+Break reaches either). `None`: cosca's default.
    pub(super) process_group: Option<bool>,
}

impl StopPlan {
    /// The plan a daemon's stop runs.
    pub(super) fn of(spec: &DaemonSpec) -> StopPlan {
        match spec {
            DaemonSpec::Task(_) => StopPlan::WaitOnly,
            DaemonSpec::Service(service) => match console_event(service.stop_action.send_signal) {
                Some(event) => StopPlan::AskAndWait {
                    event,
                    timeout: service.stop_action.timeout,
                },
                None => StopPlan::AtOnce,
            },
        }
    }

    /// What the daemon is spawned with for this plan: the one place the spawn's stop settings are decided.
    pub(super) fn spawn_setup(self) -> SpawnSetup {
        match self {
            StopPlan::AskAndWait { event, .. } => SpawnSetup {
                ask_with: Some(event),
                process_group: Some(false),
            },
            StopPlan::AtOnce | StopPlan::WaitOnly => SpawnSetup::default(),
        }
    }

    /// Whether the stop asks through cosca's proxy, which needs a host that called `init_helpers`.
    pub(super) fn asks(self) -> bool {
        matches!(self, StopPlan::AskAndWait { .. })
    }
}

/// The console event a stop of `signal` sends the daemon, or `None` where it sends nothing: the one source for which
/// signals ask.
fn console_event(signal: StopSignal) -> Option<CtrlEvent> {
    match signal {
        StopSignal::Terminate | StopSignal::Interrupt | StopSignal::CtrlC => Some(CtrlEvent::CtrlC),
        StopSignal::Quit | StopSignal::CtrlBreak => Some(CtrlEvent::CtrlBreak),
        // `wm-close` sends nothing yet; a native signal is refused on Windows at install.
        StopSignal::Kill | StopSignal::WmClose | StopSignal::Native(_) => None,
    }
}

/// What a stop saw of the root's own exit: the fact the teardown is chosen by.
pub enum RootExit {
    /// The root was not seen to exit before the teardown (or the waiter's wait failed, so it is not known to have).
    NotSeen,
    /// The root exited by itself, when the stop was seen or during its plan, and a read of the process gave its
    /// status.
    Status(ExitStatus),
    /// The root exited by itself and no read gave its status: the waiter's `wait()` has it.
    Unread,
}

pub enum WaitOutcome {
    ChildExited,
    /// The waiter's `child.wait()` failed, so whether the daemon ended was unknown. The tree kill was
    /// sent, so the shim may report a restartable failure. `os_error` is the
    /// failure's OS error code. When the kill could not be sent the outcome is
    /// [`WaitOutcome::DaemonMayRun`] or [`WaitOutcome::TreeMayRun`] instead.
    WaitFailed {
        os_error: Option<i32>,
    },
    /// A stop was requested, and its plan ran: the ask and the wait it made, then the teardown, whose
    /// tree kill was sent (or the daemon had exited, and its tree was ended).
    Stopping(RootExit),
    /// The daemon may still be running: `send_kill_tree` and the direct-child fallback both failed, after
    /// a stop or a failed wait. See [`Supervisor::wait_for_child_or_stop`] for why this is a return
    /// rather than a wait, and `Reported::incomplete_stop` for what the caller reports for it.
    DaemonMayRun,
    /// `send_kill_tree` failed, after a stop or a failed wait, and the direct child was killed and reaped:
    /// its descendants may still be running. The caller reports `Reported::incomplete_stop` for it
    /// too.
    TreeMayRun,
}

// Waiter ==============================================================================================================

/// The thread [`Supervisor::wait_for_child_or_stop`]'s blocking `child.wait()` runs on, made before
/// the daemon is spawned and parked until it is handed that daemon. Handed nothing — the spawn
/// failed, so there is nothing to wait on — it ends.
///
/// **Why it is made first, and never after.** This wait is the only thing that ever observes the
/// daemon exiting or tears its tree down on a stop. Making it after the spawn means a daemon can
/// be running before the shim knows whether anything can watch it, and the OS refusing a thread
/// there leaves that daemon orphaned: nothing waiting on it, nothing to stop it, no kill-tree
/// teardown — with SCM told, at best, that its service just died. `std::thread::Scope::spawn`
/// could not even report that refusal: it has no `Result` and panics, taking the service process
/// down with the daemon already up.
/// [`std::thread::Builder::spawn`] returns `io::Result`, which is what lets the refusal be a
/// clean start failure with nothing launched — see `service::launch`, the one caller that
/// enforces the order.
///
/// A [`Standby`] of the daemon the thread waits on.
pub struct Waiter {
    standby: Standby<Arc<dyn Spawned>>,
    thread: std::thread::JoinHandle<()>,
    /// The bus this waiter's thread reports the child's exit to, so
    /// [`Supervisor::wait_for_child_or_stop`] can check it is the bus that will
    /// be waiting for that report. A waiter handed to any other bus leaves
    /// that bus's `wait_while` with no waker at all in the no-stop case — the
    /// permanent hang this module exists to avoid, reached with no OS failure
    /// involved. One `Arc` clone per waiter is what makes that checkable
    /// rather than conventional.
    bus: Arc<StopBus>,
}

impl Waiter {
    fn new(os: &dyn Os, log: &Arc<Log>, bus: &Arc<StopBus>, id: &str) -> io::Result<Waiter> {
        let reporting_to = Arc::clone(bus);
        let bus = Arc::clone(bus);
        let log = Arc::clone(log);
        let id = id.to_string();
        let (standby, thread) = Standby::with(
            "goetia-shim-waiter",
            |name, run| os.thread(name, run),
            move |child: Arc<dyn Spawned>| {
                // A panic from here on reports as a failed wait, or `wait_while` would have no waker.
                let mut report = ReportOnPanic { bus: &bus, armed: true };
                // Notified on `Err` too: nothing else wakes `wait_while` in the no-stop case.
                let failure = child.wait().err().map(|e| {
                    log.failure(&id, &wait_failed(&e));
                    os_error_in(&e)
                });
                bus.report_done(failure);
                report.armed = false;
            },
        )?;
        Ok(Waiter {
            standby,
            thread,
            bus: reporting_to,
        })
    }

    /// End a waiter that is never handed a daemon, and wait for its thread, so that nothing of
    /// this run is left running when the launch fails.
    pub(super) fn stand_down(self) {
        drop(self.standby);
        // A panic of the thread is of nothing it was handed: it was handed nothing.
        let _ = self.thread.join();
    }

    /// Hand the thread the daemon to wait on, and give back its handle to join once that wait can
    /// return — see [`Supervisor::wait_for_child_or_stop`], which is the only place that joins it.
    fn watch(self, child: Arc<dyn Spawned>) -> std::thread::JoinHandle<()> {
        let handed = self.standby.hand(child);
        debug_assert!(
            handed.is_ok(),
            "the waiter thread ended before it was handed the daemon"
        );
        self.thread
    }
}

impl StopBus {
    fn new() -> StopBus {
        StopBus {
            state: Mutex::new(Gate::default()),
            cvar: Condvar::new(),
        }
    }

    /// Called from the SCM control handler (`service::control_handler`), on the dispatcher thread.
    /// Only ever sets a flag and wakes waiters — it never acts on the child
    /// itself. Acting directly from this callback (calling `send_kill_tree()`
    /// here) could not know whether the service thread has spawned the child
    /// yet, so a stop delivered in that gap could kill nothing and be silently
    /// lost. Routing the actual kill through the service thread, gated on this
    /// same flag re-checked at every wake-up, means a stop is never missed
    /// regardless of when it arrives.
    ///
    /// Returns whether the stop was accepted: not once [`StopBus::settle`] has decided a failure,
    /// which is then reported as it is. A stop accepted here makes `settle` report clean.
    pub fn request_stop(&self) -> bool {
        let mut g = self.state.lock().expect("StopBus mutex poisoned");
        if g.fate == Fate::Failed {
            return false;
        }
        g.fate = Fate::Stopped;
        self.cvar.notify_all();
        true
    }

    /// The one wait on the gate: until `until` holds of it, or, given a `pace`, until that has passed. Says whether
    /// `until` holds. A `pace` only spaces a loop that re-reads the real condition it waits for between calls: its
    /// expiry proves nothing.
    fn wait_until(&self, pace: Option<Duration>, until: impl Fn(&Gate) -> bool) -> bool {
        let g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let g = match pace {
            None => self
                .cvar
                .wait_while(g, |g| !until(g))
                .unwrap_or_else(PoisonError::into_inner),
            Some(pace) => {
                self.cvar
                    .wait_timeout_while(g, pace, |g| !until(g))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
        until(&g)
    }

    /// Waits until a stop has been accepted or `pace` has passed, and says whether one has.
    pub(super) fn stopped_within(&self, pace: Duration) -> bool {
        self.wait_until(Some(pace), |g| g.fate == Fate::Stopped)
    }

    /// Decide how the run ends when `candidate` is what the daemon's end amounts to. An accepted
    /// stop wins over any candidate (terminating the daemon's job makes it exit non-zero, which a
    /// commanded stop must not report as a failure) and the result is [`Reported::Clean`]. Otherwise
    /// the result is `candidate`, and a failure among those closes the bus to later stops.
    ///
    /// Check and decision are one critical section with [`StopBus::request_stop`], so a stop is
    /// either accepted before the exit is decided or arrives after it, never both.
    pub fn settle(&self, candidate: Reported) -> Reported {
        let mut g = self.state.lock().expect("StopBus mutex poisoned");
        match (g.fate, candidate) {
            (Fate::Stopped, _) => Reported::Clean,
            (_, Reported::Clean) => Reported::Clean,
            (_, failure) => {
                g.fate = Fate::Failed;
                failure
            }
        }
    }

    /// Close the bus to later stops: the exit is decided, and a later stop must not change it. A
    /// stop accepted already stays accepted.
    fn refuse_stops(&self) {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if g.fate == Fate::Open {
            g.fate = Fate::Failed;
        }
    }

    /// Whether a stop has been accepted: for task mode's look right before its spawn.
    pub(super) fn stop_accepted(&self) -> bool {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).fate == Fate::Stopped
    }

    /// Whether the waiter has reported its wait over, and with what failure, if any.
    fn exit_report(&self) -> (bool, Option<Option<i32>>) {
        let g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (g.child_done, g.wait_failure)
    }

    /// Waits until the waiter has reported its wait over or `pace` has passed, and says whether it has. The report
    /// is made for a failed wait too: [`StopBus::exit_report`] says which it was.
    fn exited_within(&self, pace: Duration) -> bool {
        self.wait_until(Some(pace), |g| g.child_done)
    }

    /// Blocks until the waiter has reported that its wait is over, whatever it ended in. For a caller that
    /// knows the root has exited, so the report is on its way.
    pub(super) fn await_exit_report(&self) {
        self.wait_until(None, |g| g.child_done);
    }

    /// The waiter's report that its wait is over, with the failure it ended in, if any.
    fn report_done(&self, failure: Option<Option<i32>>) {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        g.wait_failure = failure;
        g.child_done = true;
        self.cvar.notify_all();
    }
}

// Supervisor ==========================================================================================================

/// What one run of the service supervises with: the [`Os`] it asks, the [`Log`] every failure of the
/// run goes to, and the [`StopBus`] gate the control handler shares. A run has one [`Log`], shared by
/// the clones of the supervisor's `Arc<Log>` (the [`Waiter`]'s thread holds one), so the daemon-log
/// path [`Log::set_daemon_log`] records is the one the waiter thread logs to too.
pub struct Supervisor {
    pub(crate) os: Arc<dyn Os>,
    pub(crate) log: Arc<Log>,
    pub(crate) bus: Arc<StopBus>,
}

impl Supervisor {
    /// A supervisor whose spawns and threads go through `os`, and whose supervisor and waiter
    /// threads report failures to `log`.
    pub fn new(os: Arc<dyn Os>, log: Arc<Log>) -> Supervisor {
        Supervisor {
            os,
            log,
            bus: Arc::new(StopBus::new()),
        }
    }

    /// The thread that will run the blocking `child.wait()` for the next wait, made before the
    /// daemon it will wait on is spawned. (The supervisor's own wait blocks on the gate's condvar;
    /// the [`Waiter`]'s thread is what blocks on the child.) See [`Waiter`] for why that order is
    /// the whole point, and `io::Result` rather than a panic the whole mechanism.
    pub fn waiter(&self, id: &str) -> io::Result<Waiter> {
        Waiter::new(&*self.os, &self.log, &self.bus, id)
    }

    /// Block until `child` exits or a stop is requested, returning which. On a stop, first runs `plan`
    /// (the ask and the wait of [`StopPlan::AskAndWait`], the wait for the daemon's own exit of
    /// [`StopPlan::WaitOnly`], or nothing), then tears the whole contained tree down (`send_kill_tree`, with a
    /// fallback to killing just the direct child — see below) before
    /// returning, and says in its return value whether that teardown
    /// succeeded.
    ///
    /// `on_stopping` is called exactly once, as soon as the wait has ended for any reason (a stop,
    /// the child exiting, a failed wait) and before anything is asked, waited for or torn down, with no lock held,
    /// told by a [`WaitEnd`] whether an accepted stop or the daemon's own end ended the wait. It is where the
    /// service reports `STOP_PENDING`, so that SCM never reads it as running while the daemon is being stopped,
    /// and where a task records the stop before the wait that may outlast the shim.
    ///
    /// `on_progress` is called, from the calling thread, while an ask-and-wait plan goes on: after each slice of
    /// [`PROGRESS_PERIOD`] of the wait for the tree that left members and had a next one, and after each pause of the
    /// re-ask loop that is followed by another ask. It is not called after the wait has ended, and never by a plan that
    /// asks nothing or waits for the program alone. The service reports `STOP_PENDING` with a higher checkpoint in
    /// it, which is what keeps Windows from ending a service whose stop takes long at a shutdown. Nothing is called
    /// while the ask itself is being made: cosca 0.10's `terminate_tree` waits for its proxy with no bound.
    ///
    /// **The read** of the process, once, where the stop is seen and before `on_stopping` (a read that fails counts as
    /// gone if the waiter has reported the exit): a root already gone is
    /// told as [`WaitEnd::Exit`], not [`WaitEnd::Stop`], and no plan runs for it. The status the read gives is the
    /// root's exit, whatever the waiter reports after it, and [`WaitOutcome::Stopping`] carries it.
    ///
    /// **The plan** runs only for a stop accepted with the daemon not yet read as gone. The
    /// `timeout` of an ask-and-wait is counted from when this call saw the stop. A daemon that exited
    /// before the plan is neither asked nor waited for, and the tree it left is ended as after any exit. A
    /// wait-only plan has no `timeout`: it waits as long as the daemon lives, so what ends the daemon is outside
    /// this call (a task's close event, which Windows follows by ending the shim).
    /// The ask's answers are mapped in one place ([`judge_ask`]). The waiter stays in `child.wait()`
    /// throughout, which cosca supports beside `terminate_tree` and `wait_tree_timeout`.
    ///
    /// **Which teardown** is chosen by the status a read gave, the stop's or one the plan made, and with none by the
    /// waiter's report once the plan is over: after an exit, `terminate_exited_tree` (no direct-child fallback; its
    /// failure is [`WaitOutcome::TreeMayRun`]), otherwise `kill_for_stop`.
    ///
    /// **Not cosca's `graceful_shutdown_tree`**, which ends by waiting with no deadline for every member
    /// its sweep killed: the standing rule below against that wait would be broken.
    ///
    /// `waiter` is consumed here: it was made before `child` was spawned
    /// ([`Waiter`]), and this is where it is handed the daemon it was made
    /// for. Taking it by value is what makes "no waiter, no wait" checkable
    /// at the call site rather than a convention; the `debug_assert` below
    /// does the same for *which* bus it was made for, since a waiter that
    /// reports to another bus would leave this wait with no waker.
    ///
    /// **Why the teardown happens here, inside this call, rather than in the
    /// caller after it returns.** The [`Waiter`]'s thread performs the real,
    /// blocking `child.wait()`, and this call joins it before returning, so
    /// that wait must itself return before this function can. For a daemon
    /// that does not exit on its own — the ordinary `protocol: plain` case —
    /// nothing makes `child.wait()` return except killing the child. If the
    /// kill happened only after this call returned, the join would block
    /// forever waiting for a worker that is parked on a process nothing has
    /// told it to kill yet — a deadlock on every commanded stop of a running
    /// daemon, not a rare case. Killing first, then joining, is what lets
    /// that wait actually resolve.
    ///
    /// **Why it joins at all**, rather than leaving the waiter detached: the
    /// waiter thread holds an `Arc<dyn Spawned>`, so a detached one would keep the
    /// child alive past the caller's own drop. The one arm that does not join
    /// is the one that already knows that wait cannot complete, and says so
    /// in its return value — see below.
    ///
    /// **A panic between the hand-off and that join detaches the waiter**: a
    /// panic at one of the `expect`s below (or inside `send_kill_tree`/`kill`)
    /// drops the `JoinHandle` instead of joining it,
    /// and the detached thread still holds an `Arc<dyn Spawned>` clone — so the
    /// caller's own `Arc` going out of scope during unwind does not drop
    /// the last reference, and `cosca::Child::drop`'s kill-tree teardown
    /// does not run. The daemon's Job Object is `KILL_ON_JOB_CLOSE`, so the
    /// tree still dies once the shim process exits; what is lost is the
    /// teardown before that. Worth knowing before anyone makes one of those
    /// `expect`s recoverable.
    ///
    /// **`send_kill_tree` can itself fail to kill anything** — a refused job termination, or a refused
    /// handle kill, since `send_kill_tree` ends in the same direct-handle kill the fallback below
    /// re-issues. A refused `TerminateJobObject` is a `Containment` over the `Io` that holds the OS
    /// code, so the code sits down the `source()` chain and nothing should match on the top variant.
    /// (The daemon always has a Job Object: `service::launch` refuses any other containment, so
    /// `Unsupported` does not arrive here.)
    /// That is exactly the same deadlock at one remove: if nothing kills the
    /// child, the background thread's `wait()` still never returns.
    /// `child.kill()` — the direct process handle, no containment required —
    /// is the fallback that lets the wait below resolve anyway. On a
    /// contained child it is a second attempt rather than a new idea:
    /// `send_kill_tree` ends in that same handle kill as its own backstop, so
    /// what the fallback is really for is a refusal transient enough to
    /// clear between the two. It cannot reach
    /// any of the child's own descendants, so a `send_kill_tree` failure
    /// genuinely does mean a reduced guarantee (root killed, tree possibly
    /// not), not merely a doc footnote. Both failures are always logged.
    ///
    /// **If that fallback `kill` fails too, the waiter is left detached and
    /// this call returns [`WaitOutcome::DaemonMayRun`]** ([`kill_for_stop`]
    /// returning [`Teardown::Failed`] below — the one path that does not join). Two failed kills are
    /// positive evidence that nothing has told the child to exit, so the
    /// waiter's `child.wait()` cannot return and joining it would block for
    /// as long as that child lives: `run_service` would never reach
    /// `report_stopped`, SCM's stop would never complete, and the service —
    /// which reads `STOP_PENDING` throughout its own teardown (`on_stopping`),
    /// accepting no stop — could then be neither stopped nor
    /// uninstalled for the daemon's whole remaining life. Skipping the join
    /// on that one arm bounds nothing and races nothing, because the wait it
    /// would wait for is already known not to resolve. What it gives up is the
    /// guarantee the join buys on every other path — the daemon is not reaped
    /// and may well still be running — so the outcome is a distinct one,
    /// reported as `Reported::incomplete_stop` rather than a clean one. The
    /// detached thread still holds its own `Arc<dyn Spawned>` clone, so this is
    /// memory-safe, and if the child ever does exit its `wait()` returns and
    /// that clone drops.
    ///
    /// **A failed wait is not evidence the daemon ended**, so the tree is killed as for a stop. The
    /// failure may be reported as one SCM restarts only if `send_kill_tree` succeeded; the daemon always
    /// runs in a Job Object (`service::launch` refuses any other containment), where `Ok` means
    /// `TerminateJobObject` succeeded. Otherwise (the tree kill failed, even if the direct child's
    /// fallback worked) the outcome is [`WaitOutcome::TreeMayRun`] (and [`WaitOutcome::DaemonMayRun`]
    /// when it failed too), and the bus refuses later
    /// stops: a refused termination leaves the members running indefinitely, and reporting a
    /// restartable failure over them would start a second copy beside them.
    ///
    /// **A stop whose tree kill failed** is [`WaitOutcome::TreeMayRun`] too, even when the direct
    /// child's fallback worked: its descendants may still run.
    ///
    /// **No drain after `send_kill_tree`.** It only sends the kill and never waits for a member:
    /// cosca's waiting call has no deadline, so a member stuck in a driver call would hold the stop
    /// for as long as it lives. Windows does not document `TerminateJobObject` as synchronous with
    /// member teardown, so members may still be exiting (holding a port or lock file) when STOPPED
    /// is reported.
    pub fn wait_for_child_or_stop(
        &self,
        waiter: Waiter,
        child: &Arc<dyn Spawned>,
        id: &str,
        plan: StopPlan,
        on_stopping: impl FnOnce(WaitEnd),
        mut on_progress: impl FnMut(),
    ) -> WaitOutcome {
        debug_assert!(
            Arc::ptr_eq(&waiter.bus, &self.bus),
            "this waiter was made for a different StopBus: it would report the child's exit to that one, \
             leaving this wait with no waker"
        );
        {
            let mut g = self.bus.state.lock().expect("StopBus mutex poisoned");
            g.child_done = false;
            g.wait_failure = None;
        }
        // After the reset, never before it: the waiter is parked until this
        // call, so nothing can report *this* child done until it is handed
        // one, and a reset afterwards could erase that report.
        let waiting = waiter.watch(Arc::clone(child));
        let g = self.bus.state.lock().expect("StopBus mutex poisoned");
        let g = self
            .bus
            .cvar
            .wait_while(g, |g| g.fate != Fate::Stopped && !g.child_done)
            .expect("StopBus mutex poisoned");
        let stopping = g.fate == Fate::Stopped;
        let wait_failure = g.wait_failure;
        // The `timeout` of an ask-and-wait is counted from here.
        let seen = Instant::now();
        // Release the lock before calling out — `on_stopping` and `send_kill_tree`/`kill` need it
        // not, and holding it across a kernel call would block `request_stop()`/`settle()`
        // callers (e.g. a second, redundant SCM stop control) for no reason.
        drop(g);
        // The one read of the live process at the stop, before `on_stopping`. Gone is decided here, once
        // ([`Supervisor::find_gone`]). A failed wait says nothing of the daemon: no read.
        let mut unreadable_told = false;
        let gone = if stopping && wait_failure.is_none() {
            self.find_gone(child.as_ref(), id, &mut unreadable_told)
        } else {
            None
        };
        on_stopping(if stopping && gone.is_none() {
            WaitEnd::Stop
        } else {
            WaitEnd::Exit
        });
        let outcome = if !stopping && wait_failure.is_none() {
            WaitOutcome::ChildExited
        } else {
            // A failed wait says nothing of the daemon, and nothing is watching it: no ask, no wait.
            // The status a read of the process gave, which is the root's exit whatever the waiter reports later.
            let read_status = if !stopping || wait_failure.is_some() {
                None
            } else if let Some(gone) = gone {
                gone.status
            } else {
                self.run_plan(plan, child.as_ref(), id, seen, unreadable_told, &mut on_progress)
            };
            // By the waiter's report when no read gave a status: a daemon that exited, during the plan or before
            // it, is a tree to end, not a process to kill.
            let (exited, wait_failure) = match read_status {
                Some(_) => (true, None),
                None => self.bus.exit_report(),
            };
            let teardown = if exited && wait_failure.is_none() {
                if terminate_exited_tree(child.as_ref(), &self.log, id) {
                    Teardown::Tree
                } else {
                    Teardown::ExitedTreeLeft
                }
            } else {
                kill_for_stop(child.as_ref(), &self.log, id)
            };
            match (teardown, wait_failure) {
                // Returning here is what leaves `waiting` unjoined: nothing
                // can make the wait it holds return, so joining it would
                // block for the child's whole remaining life. See this
                // function's doc comment — this is the one path where the
                // return value does not mean "terminated".
                (Teardown::Failed, _) => {
                    if !stopping {
                        self.bus.refuse_stops();
                    }
                    return WaitOutcome::DaemonMayRun;
                }
                // Logged by `terminate_exited_tree`: the daemon itself exited.
                (Teardown::ExitedTreeLeft, _) => WaitOutcome::TreeMayRun,
                (Teardown::ChildOnly, _) => {
                    self.log.failure(id, &tree_unkilled());
                    if !stopping {
                        self.bus.refuse_stops();
                    }
                    WaitOutcome::TreeMayRun
                }
                _ if stopping => WaitOutcome::Stopping(match read_status {
                    Some(status) => RootExit::Status(status),
                    None if exited && wait_failure.is_none() => RootExit::Unread,
                    None => RootExit::NotSeen,
                }),
                (Teardown::Tree, os_error) => WaitOutcome::WaitFailed {
                    os_error: os_error.flatten(),
                },
            }
        };
        // Joined only now, after the kill above: the waiter's `child.wait()`
        // can return — the child exited on its own, or one of the two kills
        // reached it; the arm where neither is true returned above rather
        // than joining.
        if waiting.join().is_err() {
            self.log.failure(id, WAITER_PANICKED);
        }
        outcome
    }
}

// Stop plans ----------------------------------------------------------------------------------------------------------

/// How often the wait for the tree is cut, so that the stop can be reported as going on: the length of one slice.
pub(super) const PROGRESS_PERIOD: Duration = Duration::from_secs(5);

/// How long the asking pauses after the first answer of `Unsupported`, which says the daemon has no console to be
/// sent the event on yet. Each such pass starts a proxy process, so the pause doubles ([`grown_pace`]) up to
/// [`REASK_PACE_CEILING`]: the first re-ask is fast, which is the case that matters (a daemon right after its
/// start), and a program that never has a console is asked about once a second. A pause only spaces the re-asks of a
/// question each answer decides; a daemon's exit ends it at once.
const REASK_PACE_FIRST: Duration = Duration::from_millis(25);
const REASK_PACE_CEILING: Duration = Duration::from_secs(1);

/// The pause after `pace`: twice as long, up to the ceiling.
fn grown_pace(pace: Duration) -> Duration {
    pace.saturating_mul(2).min(REASK_PACE_CEILING)
}

impl Supervisor {
    /// What an accepted stop does before the teardown, for a root that was not gone when the stop was seen
    /// ([`Supervisor::wait_for_child_or_stop`]). `seen` is when the supervisor saw the stop; `unreadable_told` says that
    /// a failed read of the process was told already. The status a read of the process gave, if one found the root
    /// gone during the plan.
    fn run_plan(
        &self,
        plan: StopPlan,
        child: &dyn Spawned,
        id: &str,
        seen: Instant,
        unreadable_told: bool,
        on_progress: &mut impl FnMut(),
    ) -> Option<ExitStatus> {
        match plan {
            StopPlan::AtOnce => None,
            // No cap of goetia's: the stop that Windows delivered ends the program, or Windows ends the shim. The
            // waiter's report ends the wait, a failed wait's included ([`StopBus::exit_report`] says which).
            StopPlan::WaitOnly => {
                self.bus.await_exit_report();
                None
            }
            StopPlan::AskAndWait { event, timeout } => {
                self.ask_and_wait(child, id, event, timeout, seen, unreadable_told, on_progress)
            }
        }
    }

    /// Ask the daemon's tree to exit, then wait for the whole tree to exit within what is left of `timeout`.
    ///
    /// The daemon's exit is read before every ask ([`Supervisor::find_gone`]: the process itself, or the waiter's
    /// report when that read fails): one that has exited is neither asked (again) nor waited for. The first ask is
    /// told in the log before it is made, so that the line comes before whatever the daemon does in answer.
    ///
    /// A tree is waited for after any answer that may mean the event was sent, whether or not the root has exited
    /// since. That includes a root that exits by itself between the read before an ask and that ask, which is then
    /// waited for though never asked: waiting is the safe side.
    ///
    /// **To delete when cosca ships it.** That window, when bindreams/cosca#1054 tells the two `Unassessable` answers
    /// apart. The re-ask loop, when bindreams/cosca#1053 answers "no console yet" apart from "never". The ask's own
    /// bound, when bindreams/cosca#1057 bounds it: `terminate_tree` waits for its proxy with no cap under cosca 0.10.
    /// This whole composition, when bindreams/cosca#1055 has a graceful stop that does not drain.
    fn ask_and_wait(
        &self,
        child: &dyn Spawned,
        id: &str,
        event: CtrlEvent,
        timeout: Option<Duration>,
        seen: Instant,
        mut unreadable_told: bool,
        on_progress: &mut impl FnMut(),
    ) -> Option<ExitStatus> {
        let left = || timeout.map(|t| t.saturating_sub(seen.elapsed()));
        let mut asking_told = false;
        let mut told = false;
        let mut pace = REASK_PACE_FIRST;
        loop {
            if let Some(gone) = self.find_gone(child, id, &mut unreadable_told) {
                if asking_told {
                    self.log.info(id, EXITED_UNASKED);
                }
                return gone.status;
            }
            if !asking_told {
                asking_told = true;
                self.log.info(id, &asking(event, left()));
            }
            match judge_ask(child.terminate_tree()) {
                Ask::Sent => break,
                Ask::MaySent(e) => {
                    self.log.warning(id, &may_have_sent(event, &e));
                    break;
                }
                Ask::NothingSent(e) => {
                    self.log.failure(id, &could_not_ask(&e));
                    return None;
                }
                Ask::NotYet(e) => {
                    if timeout.is_some_and(|t| seen.elapsed() >= t) {
                        self.log.failure(id, &never_asked(&e));
                        return None;
                    }
                    if !told {
                        told = true;
                        self.log.warning(id, &not_asked_yet(&e));
                    }
                    if self.bus.exited_within(left().map_or(pace, |left| left.min(pace))) {
                        // A wait that failed is reported the same way, and told by its own failure line.
                        if self.bus.exit_report().1.is_none() {
                            self.log.info(id, EXITED_UNASKED);
                        }
                        return None;
                    }
                    pace = grown_pace(pace);
                    if timeout.is_some_and(|t| seen.elapsed() >= t) {
                        self.log.failure(id, &never_asked(&e));
                        return None;
                    }
                    // Asked again after this pause, so the stop is still going on.
                    on_progress();
                }
            }
        }
        match wait_for_tree(child, left, on_progress) {
            // The job is empty, so the root has exited and its report is on its way.
            Ok(TreeDrain::AllMembersExited) => {
                self.log.info(id, EXITED_WITHIN_THE_WAIT);
                self.bus.await_exit_report();
            }
            Ok(TreeDrain::AllMarkersClosed) => self.log.warning(id, MARKERS_CLOSED),
            // Only a wait with a `timeout` ends with members left: one with none goes on.
            Ok(TreeDrain::MembersRemain) => {
                debug_assert!(timeout.is_some(), "a wait with no timeout ended with members left");
                if let Some(timeout) = timeout {
                    self.log.warning(id, &tree_not_seen_to_exit(timeout));
                }
            }
            Err(e) => self.log.failure(id, &wait_tree_failed(&e)),
        }
        None
    }

    /// The daemon's root, if it is gone: the process itself says it has exited (the waiter's report can lag the
    /// exit), with the status the read gave, or the read of it failed and the waiter has reported the exit, with no
    /// status. A failed read that the report does not settle says nothing, and is logged the first time it fails
    /// (`told` says it was); one that the report settles is not logged, since the exit is known.
    fn find_gone(&self, child: &dyn Spawned, id: &str, told: &mut bool) -> Option<Gone> {
        match child.try_wait() {
            Ok(status) => status.map(|status| Gone { status: Some(status) }),
            Err(e) => {
                let reported = self.bus.exit_report() == (true, None);
                if !reported && !std::mem::replace(told, true) {
                    self.log.warning(id, &exit_unreadable(&e));
                }
                reported.then_some(Gone { status: None })
            }
        }
    }
}

/// Wait for the whole tree to exit within `left()` (what is left of the `timeout`; `None`: as long as it takes), in
/// slices of [`PROGRESS_PERIOD`] that each re-check the tree, calling `on_progress` after each slice that left members
/// and had a next one. A slice that left members has waited its whole bound, so the next one is what the last left
/// of the wait, or what the clock says is left of the `timeout` if less: the slices add up to the `timeout`, never
/// more. With no `timeout` members left are no verdict, and the slices go on until the tree exits or the wait fails.
fn wait_for_tree(
    child: &dyn Spawned,
    left: impl Fn() -> Option<Duration>,
    on_progress: &mut impl FnMut(),
) -> Result<TreeDrain, cosca::error::Error> {
    let mut remaining = left();
    loop {
        let slice = remaining.map_or(PROGRESS_PERIOD, |remaining| remaining.min(PROGRESS_PERIOD));
        match child.wait_tree_timeout(slice) {
            Ok(TreeDrain::MembersRemain) => {
                remaining = remaining.map(|remaining| {
                    let after = remaining.saturating_sub(slice);
                    left().map_or(after, |left| after.min(left))
                });
                if remaining == Some(Duration::ZERO) {
                    return Ok(TreeDrain::MembersRemain);
                }
                on_progress();
            }
            answer => return answer,
        }
    }
}

/// A root that is gone. Its exit status is the root's exit whatever the waiter reports after the read that found it.
#[derive(Clone, Copy)]
struct Gone {
    /// What the read of the process gave; `None` when the read failed and the waiter's report said it was gone.
    status: Option<ExitStatus>,
}

/// What an ask answered, by the contract of `terminate_tree` with a proxy send.
enum Ask {
    /// The event was sent.
    Sent,
    /// It may have been sent ([`judge_ask`]), so the wait is made.
    MaySent(cosca::error::Error),
    /// Nothing was sent, and asking again cannot help: the proxy could not be started.
    NothingSent(cosca::error::Error),
    /// Nothing was sent: the daemon has no console to send it on, yet or ever.
    NotYet(cosca::error::Error),
}

/// The one mapping of an ask's answer. `Error` is non-exhaustive and cosca's contract names no other answer, so one it
/// does not name is read as "may have been sent". That includes `Unassessable`: cosca answers it both for a root that
/// had exited when it looked (nothing sent) and for an event the proxy generated and could not confirm (sent), and a
/// launcher that was sent the event and exited at once has a worker that needs its `timeout`. Ending a tree that may be
/// shutting down is the worse mistake, so the tree is waited for.
fn judge_ask(answer: Result<(), cosca::error::Error>) -> Ask {
    match answer {
        Ok(()) => Ask::Sent,
        Err(e @ cosca::error::Error::Io(_)) => Ask::NothingSent(e),
        Err(e @ cosca::error::Error::Unsupported { .. }) => Ask::NotYet(e),
        Err(e) => Ask::MaySent(e),
    }
}

// Waiter panic --------------------------------------------------------------------------------------------------------

/// Reports a panic in the waiter's closure as a failed wait. Without it the panic ends the thread
/// with `child_done` unset, and in the no-stop case nothing else would wake `wait_while`.
struct ReportOnPanic<'a> {
    bus: &'a StopBus,
    armed: bool,
}

impl Drop for ReportOnPanic<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.bus.report_done(Some(None));
        }
    }
}

// Teardown ============================================================================================================

/// What [`kill_for_stop`] reached.
pub(super) enum Teardown {
    /// `send_kill_tree` succeeded.
    Tree,
    /// `send_kill_tree` failed and the direct child was killed: its descendants were not reached.
    ChildOnly,
    /// The daemon had exited and `send_kill_tree` failed: its descendants may still be running.
    ExitedTreeLeft,
    /// Neither worked, so nothing has told the daemon to exit and no `child.wait()` on it can
    /// return.
    Failed,
}

/// Terminate the tree of a daemon that has already exited and is about to be reported `STOPPED`,
/// so that a restart or `start` never runs beside a tree nobody told to end. `true` is `send_kill_tree`
/// succeeding. `false` (logged) means the
/// tree may still be running and the stop must be reported as incomplete.
pub(super) fn terminate_exited_tree(child: &dyn Spawned, log: &Log, id: &str) -> bool {
    let Err(e) = child.send_kill_tree() else { return true };
    log.failure(id, &exited_tree_unkilled(&e));
    false
}

/// Kill the daemon: the whole contained tree, or failing that the direct child alone. Each failure
/// is logged as it happens; the [`Teardown`] says how far the kill reached.
pub(super) fn kill_for_stop(child: &dyn Spawned, log: &Log, id: &str) -> Teardown {
    let Err(e) = child.send_kill_tree() else {
        return Teardown::Tree;
    };
    log.failure(id, &send_kill_tree_failed(&e));
    let Err(e2) = child.kill() else {
        return Teardown::ChildOnly;
    };
    log.failure(id, &fallback_kill_failed(&e2));
    Teardown::Failed
}

// Failure reports -----------------------------------------------------------------------------------------------------

fn wait_failed(e: &cosca::error::Error) -> String {
    format!("background wait on child: {}", Chain(e))
}

const WAITER_PANICKED: &str = "the thread waiting on the child panicked";

fn send_kill_tree_failed(e: &cosca::error::Error) -> String {
    format!(
        "terminating the daemon's process tree failed: {}; falling back to killing the direct child only (its own descendants, if any, \
         cannot be reached without a containment mechanism)",
        Chain(e)
    )
}

/// What every report of a daemon that may still be running says SCM is told.
pub(super) const NOT_RESTARTED: &str = " — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure";

fn fallback_kill_failed(e: &cosca::error::Error) -> String {
    format!(
        "fallback kill: {}; the daemon was neither killed nor reaped and may still be running{NOT_RESTARTED}",
        Chain(e)
    )
}

fn could_not_ask(e: &cosca::error::Error) -> String {
    format!(
        "the stop could not ask the daemon to exit: {}; nothing was sent, so the daemon is ended without being asked",
        Chain(e)
    )
}

fn may_have_sent(event: CtrlEvent, e: &cosca::error::Error) -> String {
    format!(
        "the stop may have sent {} to ask the daemon to exit: {}; waiting for it as if it had been asked",
        event_name(event),
        Chain(e)
    )
}

fn exit_unreadable(e: &cosca::error::Error) -> String {
    format!("could not read whether the daemon has exited: {}", Chain(e))
}

fn not_asked_yet(e: &cosca::error::Error) -> String {
    format!(
        "the daemon cannot be sent the exit event yet (it has no console, or never will): {}; asking again until it answers otherwise, the daemon exits or the timeout is over",
        Chain(e)
    )
}

fn never_asked(e: &cosca::error::Error) -> String {
    format!(
        "the daemon was never asked to exit: {}; its timeout is over, so it is ended without being asked",
        Chain(e)
    )
}

fn event_name(event: CtrlEvent) -> &'static str {
    match event {
        CtrlEvent::CtrlC => "Ctrl+C",
        CtrlEvent::CtrlBreak => "Ctrl+Break",
    }
}

/// The line before the first ask. `left` is what is left of the spec's `timeout`, which counts from when the stop was
/// seen, to the millisecond: the wait ends at the end of it.
fn asking(event: CtrlEvent, left: Option<Duration>) -> String {
    let event = event_name(event);
    match left {
        Some(left) => {
            let left = Duration::new(left.as_secs(), left.subsec_millis() * 1_000_000);
            format!(
                "asking the daemon to exit with {event}; the daemon and what it started have {} to exit",
                humantime::format_duration(left)
            )
        }
        None => format!(
            "asking the daemon to exit with {event}; the daemon and what it started are waited for with no time limit"
        ),
    }
}

const EXITED_UNASKED: &str = "the daemon exited before it could be asked";

const EXITED_WITHIN_THE_WAIT: &str = "the daemon and what it started exited within the wait";

/// What a wait that ended with members left says: its process tree had not been seen to exit, and the teardown that
/// follows is told by its own lines. The root may have exited, and a member may be finishing its exit.
fn tree_not_seen_to_exit(timeout: Duration) -> String {
    format!(
        "the daemon's process tree had not been seen to exit when its timeout of {} was over; it is being ended",
        humantime::format_duration(timeout)
    )
}

/// `TreeDrain::AllMarkersClosed` is advisory: no member is known to have exited. (A Job Object does not answer it.)
const MARKERS_CLOSED: &str = "the wait for the daemon's process tree ended when the last copy of its marker was closed, which is no proof that every process in it exited; it is being ended";

fn wait_tree_failed(e: &cosca::error::Error) -> String {
    format!(
        "waiting for the daemon's tree to exit failed: {}; the tree is ended now",
        Chain(e)
    )
}

fn tree_unkilled() -> String {
    format!(
        "the daemon's descendants were not killed: only the direct child was killed; the daemon may still be running{NOT_RESTARTED}"
    )
}

fn exited_tree_unkilled(e: &cosca::error::Error) -> String {
    format!(
        "terminating the process tree after the daemon ended failed: {}; its descendants may still be running{NOT_RESTARTED}",
        Chain(e)
    )
}

#[cfg(test)]
#[path = "stop_bus_tests.rs"]
mod stop_bus_tests;
