//! The stop signal shared between the SCM control-handler callback and the
//! service thread, and the composition that lets that thread block on either
//! "the child exited" or "a stop was requested" without polling.
//!
//! Process-tree waiting is `cosca::Child`'s job (see `main.rs`'s module doc
//! comment); this module supplies the piece `cosca` does not: a way to
//! interrupt a blocking `child.wait()` when SCM delivers `Stop`. It also
//! performs the actual tree teardown once a stop is observed — see
//! [`StopBus::wait_for_child_or_stop`]'s doc comment for why that has to
//! happen here rather than back in the caller.
//!
//! The thread that performs that blocking wait is made before the daemon is
//! spawned, never after — see [`Waiter`].

use std::io;
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};

use cosca::Child;
use goetia::backend::scm::exit_status::Reported;

use crate::logging;
use crate::service::os_error_of;

/// How the run ends, decided once under the [`StopBus`] mutex. A stop and a restartable failure
/// exclude each other: SCM must never be told a stop was accepted and then record a failure, which
/// it would restart.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Fate {
    #[default]
    Open,
    /// A stop was accepted: the run reports clean (or the incomplete-stop record).
    Stopped,
    /// A failure was decided: a stop is refused.
    Failed,
}

#[derive(Default)]
struct Gate {
    fate: Fate,
    /// Set by the waiter when its `child.wait()` returns. Reset by [`StopBus::wait_for_child_or_stop`]
    /// before each wait.
    child_done: bool,
    /// Set by the waiter when its `child.wait()` failed: the OS error code, if the failure has one.
    /// `Some` means the daemon's fate is unknown and nothing else is watching it.
    wait_failure: Option<Option<i32>>,
}

pub struct StopBus {
    state: Mutex<Gate>,
    cvar: Condvar,
}

pub enum WaitOutcome {
    ChildExited,
    /// The waiter's `child.wait()` failed, so whether the daemon ended was unknown. `kill_tree`
    /// terminated the tree, so the shim may report a restartable failure. `os_error` is the
    /// failure's OS error code. When the tree was not terminated the outcome is
    /// [`WaitOutcome::DaemonMayRun`] instead.
    WaitFailed {
        os_error: Option<i32>,
    },
    /// A stop was requested, and the daemon was terminated: the tree, or
    /// (if `kill_tree` failed) at least the direct child.
    Stopping,
    /// The daemon may still be running: a stop was requested and neither `kill_tree` nor the
    /// direct-child fallback could kill it, or a wait failure's `kill_tree` failed. See
    /// [`StopBus::wait_for_child_or_stop`] for why this is a return rather than a wait, and
    /// `Reported::incomplete_stop` for what the caller reports for it.
    DaemonMayRun,
}

// Waiter ==============================================================================================================

/// The thread [`StopBus::wait_for_child_or_stop`]'s blocking `child.wait()` runs on, made before
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
/// Deliberately not `goetia::backend::bounded`'s `Standby`, whose shape and vocabulary this
/// follows: `spares` is a private module of the library's `pub(crate)` `backend::bounded` and
/// this is a separate binary crate, and widening it would still not carry the part that matters
/// most here — its failure-injection seam
/// is `#[cfg(test)]` *inside the library*, which a binary linking that library never compiles, so
/// no shim test could make a library `Standby` fail.
pub struct Waiter {
    hand: mpsc::Sender<Arc<Child>>,
    thread: std::thread::JoinHandle<()>,
    /// The bus this waiter's thread reports the child's exit to, so
    /// [`StopBus::wait_for_child_or_stop`] can check it is the bus that will
    /// be waiting for that report. A waiter handed to any other bus leaves
    /// that bus's `wait_while` with no waker at all in the no-stop case — the
    /// permanent hang this module exists to avoid, reached with no OS failure
    /// involved. One `Arc` clone per waiter is what makes that checkable
    /// rather than conventional.
    bus: Arc<StopBus>,
}

impl Waiter {
    fn new(bus: &Arc<StopBus>, id: &str) -> io::Result<Waiter> {
        #[cfg(test)]
        test_hook::make_thread()?;
        let (hand, rx) = mpsc::channel::<Arc<Child>>();
        let reporting_to = Arc::clone(bus);
        let bus = Arc::clone(bus);
        let id = id.to_string();
        // Read here, on the calling thread: the closure runs on a thread the hook was not armed on.
        #[cfg(test)]
        let waiting = test_hook::waiting();
        let thread = std::thread::Builder::new()
            .name("goetia-shim-waiter".to_string())
            .spawn(move || {
                #[cfg(test)]
                let _exit = test_hook::waiter_running(&id);
                let Ok(child) = rx.recv() else {
                    return; // never handed a daemon: none was spawned
                };
                // A panic from here on reports as a failed wait, or `wait_while` would have no waker.
                let mut report = ReportOnPanic { bus: &bus, armed: true };
                // Notified on `Err` too: nothing else wakes `wait_while` in the no-stop case.
                #[cfg(test)]
                let waited = test_hook::wait(waiting).and_then(|()| child.wait());
                #[cfg(not(test))]
                let waited = child.wait();
                let failure = waited.err().map(|e| {
                    logging::log_failure(&id, &wait_failed(&e));
                    os_error_of(&e)
                });
                bus.report_done(failure);
                report.armed = false;
            })?;
        Ok(Waiter {
            hand,
            thread,
            bus: reporting_to,
        })
    }

    /// Hand the thread the daemon to wait on, and give back its handle to join once that wait can
    /// return — see [`StopBus::wait_for_child_or_stop`], which is the only place that joins it.
    ///
    /// The thread is parked in `recv` with nothing before it that could end it, so the only way
    /// the hand-off could fail is a thread that panicked before receiving anything, which nothing
    /// in its body can do.
    fn watch(self, child: Arc<Child>) -> std::thread::JoinHandle<()> {
        let handed = self.hand.send(child);
        debug_assert!(
            handed.is_ok(),
            "the waiter thread ended before it was handed the daemon"
        );
        self.thread
    }
}

impl StopBus {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(Gate::default()),
            cvar: Condvar::new(),
        }
    }

    /// Called from the SCM control-handler callback (a thread `windows-service`
    /// itself owns, per `service.rs`'s `service_control_handler::register`).
    /// Only ever sets a flag and wakes waiters — it never acts on the child
    /// itself. Acting directly from this callback (calling `kill_tree()`
    /// here) could not know whether the service thread has spawned the child
    /// yet, so a stop delivered in that gap could kill nothing and be silently
    /// lost. Routing the actual kill through the service thread, gated on this
    /// same flag re-checked at every wake-up, means a stop is never missed
    /// regardless of when it arrives.
    ///
    /// Returns whether the stop was accepted, which is what the handler answers SCM with. A stop is
    /// refused once [`StopBus::settle`] has decided a failure: SCM would record that failure and
    /// restart the service, so an acknowledged stop must never be followed by one. A stop accepted
    /// here makes `settle` report clean.
    pub fn request_stop(&self) -> bool {
        let mut g = self.state.lock().expect("StopBus mutex poisoned");
        if g.fate == Fate::Failed {
            return false;
        }
        g.fate = Fate::Stopped;
        self.cvar.notify_all();
        true
    }

    /// Decide how the run ends when `candidate` is what the daemon's end amounts to. An accepted
    /// stop wins over any candidate (terminating the daemon's job makes it exit non-zero, which a
    /// commanded stop must not report as a failure) and the result is [`Reported::Clean`]. Otherwise
    /// the result is `candidate`, and a failure among those closes the bus to later stops.
    ///
    /// Check and decision are one critical section with [`StopBus::request_stop`], so a stop is
    /// either accepted before the exit is decided or refused after it, never acknowledged and then
    /// overtaken by a failure report.
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

    /// Close the bus to later stops: the exit is decided as a failure that must not be followed by
    /// an acknowledged stop. A stop accepted already stays accepted.
    fn refuse_stops(&self) {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if g.fate == Fate::Open {
            g.fate = Fate::Failed;
        }
    }

    /// The waiter's report that its wait is over, with the failure it ended in, if any.
    fn report_done(&self, failure: Option<Option<i32>>) {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        g.wait_failure = failure;
        g.child_done = true;
        self.cvar.notify_all();
    }

    /// The thread that will run the blocking `child.wait()` for this bus's next wait, made before
    /// the daemon it will wait on is spawned. (This bus's own wait blocks on the condvar; the
    /// [`Waiter`]'s thread is what blocks on the child.) See [`Waiter`] for why that order is the
    /// whole point, and `io::Result` rather than a panic the whole mechanism.
    pub fn waiter(self: &Arc<Self>, id: &str) -> io::Result<Waiter> {
        Waiter::new(self, id)
    }

    /// Block until `child` exits or a stop is requested, returning which. On
    /// a stop, also tears the whole contained tree down (`kill_tree`, with a
    /// fallback to killing just the direct child — see below) before
    /// returning, and says in its return value whether that teardown
    /// succeeded.
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
    /// that does not exit on its own — the ordinary `type: simple` case —
    /// nothing makes `child.wait()` return except killing the child. If the
    /// kill happened only after this call returned, the join would block
    /// forever waiting for a worker that is parked on a process nothing has
    /// told it to kill yet — a deadlock on every commanded stop of a running
    /// daemon, not a rare case. Killing first, then joining, is what lets
    /// that wait actually resolve.
    ///
    /// **Why it joins at all**, rather than leaving the waiter detached: the
    /// waiter thread holds an `Arc<Child>`, so a detached one would keep the
    /// child alive past the caller's own drop. The one arm that does not join
    /// is the one that already knows that wait cannot complete, and says so
    /// in its return value — see below.
    ///
    /// **A panic between the hand-off and that join detaches the waiter**: a
    /// panic at one of the `expect`s below (or inside `kill_tree`/`kill`)
    /// drops the `JoinHandle` instead of joining it,
    /// and the detached thread still holds an `Arc<Child>` clone — so the
    /// caller's own `Arc` going out of scope during unwind does not drop
    /// the last reference, and `cosca::Child::drop`'s kill-tree teardown
    /// does not run. The daemon's Job Object is `KILL_ON_JOB_CLOSE`, so the
    /// tree still dies once the shim process exits; what is lost is the
    /// teardown before that. Worth knowing before anyone makes one of those
    /// `expect`s recoverable.
    ///
    /// **`kill_tree` can itself fail to kill anything** — a refused job termination, or a refused
    /// handle kill, since `kill_tree` ends in the same direct-handle kill the fallback below
    /// re-issues. A refusal is `Err(Io)`, or `Err(Elevation)` where it was an elevated child. (The
    /// daemon always has a Job Object: `service::launch` refuses any other containment, so
    /// `Unsupported` does not arrive here.)
    /// That is exactly the same deadlock at one remove: if nothing kills the
    /// child, the background thread's `wait()` still never returns.
    /// `child.kill()` — the direct process handle, no containment required —
    /// is the fallback that lets the wait below resolve anyway. On a
    /// contained child it is a second attempt rather than a new idea:
    /// `kill_tree` ends in that same handle kill as its own backstop, so
    /// what the fallback is really for is a refusal transient enough to
    /// clear between the two. It cannot reach
    /// any of the child's own descendants, so a `kill_tree` failure
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
    /// which reads `RUNNING` throughout its own teardown, since this shim
    /// never reports `STOP_PENDING` — could then be neither stopped nor
    /// uninstalled for the daemon's whole remaining life. Skipping the join
    /// on that one arm bounds nothing and races nothing, because the wait it
    /// would wait for is already known not to resolve. What it gives up is the
    /// guarantee the join buys on every other path — the daemon is not reaped
    /// and may well still be running — so the outcome is a distinct one,
    /// reported as `Reported::incomplete_stop` rather than a clean one. The
    /// detached thread still holds its own `Arc<Child>` clone, so this is
    /// memory-safe, and if the child ever does exit its `wait()` returns and
    /// that clone drops.
    ///
    /// **A failed wait is not evidence the daemon ended**, so the tree is killed as for a stop. The
    /// failure may be reported as one SCM restarts only if `kill_tree` succeeded; the daemon always
    /// runs in a Job Object (`service::launch` refuses any other containment), where `Ok` means
    /// `TerminateJobObject` stopped every thread of every member, so nothing of the old daemon
    /// executes. Otherwise (the tree kill failed, even if the direct child's fallback worked) the
    /// outcome is [`WaitOutcome::DaemonMayRun`], and the bus refuses later stops: reporting a
    /// restartable failure over a daemon that may still be running would start a second copy
    /// beside it, and an acknowledged stop must not end in that record. Over a stop the reduced
    /// guarantee of a direct-child kill stands, as for any stop.
    ///
    /// **No drain after `kill_tree`.** `cosca`'s Job Object `hard_kill` closes the job handle as
    /// part of terminating it, so `wait_tree` afterwards always fails with "the job handle was
    /// already closed". "Terminated" means the kill succeeded, not that the kernel has finished
    /// releasing what the members held.
    pub fn wait_for_child_or_stop(self: &Arc<Self>, waiter: Waiter, child: &Arc<Child>, id: &str) -> WaitOutcome {
        debug_assert!(
            Arc::ptr_eq(&waiter.bus, self),
            "this waiter was made for a different StopBus: it would report the child's exit to that one, \
             leaving this wait with no waker"
        );
        {
            let mut g = self.state.lock().expect("StopBus mutex poisoned");
            g.child_done = false;
            g.wait_failure = None;
        }
        // After the reset, never before it: the waiter is parked until this
        // call, so nothing can report *this* child done until it is handed
        // one, and a reset afterwards could erase that report.
        let waiting = waiter.watch(Arc::clone(child));
        let g = self.state.lock().expect("StopBus mutex poisoned");
        let g = self
            .cvar
            .wait_while(g, |g| g.fate != Fate::Stopped && !g.child_done)
            .expect("StopBus mutex poisoned");
        let stopping = g.fate == Fate::Stopped;
        let wait_failure = g.wait_failure;
        let outcome = if !stopping && wait_failure.is_none() {
            drop(g);
            WaitOutcome::ChildExited
        } else {
            // Release the lock before calling into `child` — `kill_tree`/
            // `kill` need it not, and holding it across a kernel call would
            // block `request_stop()`/`settle()` callers (e.g. a
            // second, redundant SCM stop control) for no reason.
            drop(g);
            let teardown = kill_for_stop(child, id);
            match (teardown, wait_failure) {
                // Returning here is what leaves `waiting` unjoined: nothing
                // can make the wait it holds return, so joining it would
                // block for the child's whole remaining life. See this
                // function's doc comment — this is the one path where the
                // return value does not mean "terminated".
                (Teardown::Failed, _) => {
                    if !stopping {
                        self.refuse_stops();
                    }
                    return WaitOutcome::DaemonMayRun;
                }
                _ if stopping => WaitOutcome::Stopping,
                (Teardown::ChildOnly, _) => {
                    logging::log_failure(id, &tree_unkilled());
                    self.refuse_stops();
                    WaitOutcome::DaemonMayRun
                }
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
            logging::log_failure(id, WAITER_PANICKED);
        }
        outcome
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Teardown {
    /// `kill_tree` succeeded.
    Tree,
    /// `kill_tree` failed and the direct child was killed: its descendants were not reached.
    ChildOnly,
    /// Neither worked, so nothing has told the daemon to exit and no `child.wait()` on it can
    /// return.
    Failed,
}

/// Terminate the tree of a daemon that has already exited and is about to be reported as a failure
/// SCM restarts, so that the restart does not overlap what the previous run left behind. `false`
/// (logged) means the tree may still be running and the failure must not be reported as a
/// restartable one.
pub(crate) fn terminate_failed_tree(child: &Child, id: &str) -> bool {
    let Err(e) = kill_tree(child) else { return true };
    logging::log_failure(id, &failed_tree_unkilled(&e));
    false
}

/// Kill the daemon for a commanded stop: the whole contained tree, or failing that the direct
/// child alone. See [`StopBus::wait_for_child_or_stop`], the only caller, for what it does with
/// [`Teardown::Failed`]. Each failure is logged as it happens.
pub(crate) fn kill_for_stop(child: &Child, id: &str) -> Teardown {
    let Err(e) = kill_tree(child) else {
        return Teardown::Tree;
    };
    logging::log_failure(id, &kill_tree_failed(&e));
    let Err(e2) = kill_child(child) else {
        return Teardown::ChildOnly;
    };
    logging::log_failure(id, &fallback_kill_failed(&e2));
    Teardown::Failed
}

// Failure reports -----------------------------------------------------------------------------------------------------

fn wait_failed(e: &cosca::error::Error) -> String {
    format!("background wait on child: {e}")
}

const WAITER_PANICKED: &str = "the thread waiting on the child panicked";

fn kill_tree_failed(e: &cosca::error::Error) -> String {
    format!(
        "kill_tree on stop: {e}; falling back to killing the direct child only (its own descendants, if any, \
         cannot be reached without a containment mechanism)"
    )
}

/// What every report of a daemon that may still be running says SCM is told.
const NOT_RESTARTED: &str = " — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure";

fn fallback_kill_failed(e: &cosca::error::Error) -> String {
    format!(
        "fallback kill on stop: {e}; the daemon was neither killed nor reaped and may still be running{NOT_RESTARTED}"
    )
}

fn tree_unkilled() -> String {
    format!(
        "the daemon's descendants were not killed after the background wait failed: only the direct child was killed; the daemon may still be running{NOT_RESTARTED}"
    )
}

fn failed_tree_unkilled(e: &cosca::error::Error) -> String {
    format!("kill_tree after the daemon failed: {e}; its descendants may still be running{NOT_RESTARTED}")
}

/// `child.kill_tree()`, as its own function so a test can make it fail or panic — the two
/// outcomes that decide whether the waiter is joined, and neither one the OS can be asked for on
/// demand. See `test_hook::kills`.
fn kill_tree(child: &Child) -> Result<(), cosca::error::Error> {
    #[cfg(test)]
    test_hook::killing(test_hook::Site::Tree)?;
    child.kill_tree()
}

/// `child.kill()`, the direct-child fallback — same seam as [`kill_tree`], for the same reason.
fn kill_child(child: &Child) -> Result<(), cosca::error::Error> {
    #[cfg(test)]
    test_hook::killing(test_hook::Site::Child)?;
    child.kill()
}

// test_hook ===========================================================================================================

/// Test hooks for the waiter and stop path, thread-local except the process-global waiter-end
/// record (a test that makes a waiter must consume its end). Mirrors `goetia::backend::bounded`'s
/// hook, which is invisible here.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::sync::{Condvar, Mutex, PoisonError};

    /// The error [`threads`] makes `Waiter::new` fail with.
    pub(crate) const NO_THREAD: &str = "no thread may be made (test hook)";
    /// The error [`Kill::Refuse`] and [`Kill::RefuseTree`] make a kill fail with.
    pub(crate) const KILL_REFUSED: &str = "the kill was refused (test hook)";
    /// The error [`Wait::Fail`] makes the waiter's `child.wait()` fail with when it has no OS code.
    pub(crate) const WAIT_FAILED: &str = "the wait failed (test hook)";

    thread_local! {
        static THREADS: Cell<Option<usize>> = const { Cell::new(None) };
        static KILLS: Cell<Option<Kill>> = const { Cell::new(None) };
        static WAITS: Cell<Option<Wait>> = const { Cell::new(None) };
    }

    /// What [`super::kill_tree`] and [`super::kill_child`] do instead of killing.
    #[derive(Clone, Copy)]
    pub(crate) enum Kill {
        /// Fail, as an OS refusal would — a child holding no actionable containment mechanism for
        /// `kill_tree`, a `TerminateProcess` refusal for `kill`.
        Refuse,
        /// Fail `kill_tree` only, so the direct-child fallback does its real kill.
        RefuseTree,
        /// Panic, which is the documented way the waiter is left detached mid-stop.
        Panic,
    }

    /// Make both kills on this thread's stop path do `what` rather than kill, until the returned
    /// guard drops. Only the two functions above are affected: a test can still kill the daemon it
    /// spawned itself through `Child`'s own methods.
    pub(crate) fn kills(what: Kill) -> Interference {
        KILLS.set(Some(what));
        Interference
    }

    /// See [`kills`].
    pub(crate) struct Interference;

    impl Drop for Interference {
        fn drop(&mut self) {
            KILLS.set(None);
        }
    }

    /// Which of the two kills is asking.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Site {
        Tree,
        Child,
    }

    pub(super) fn killing(site: Site) -> Result<(), cosca::error::Error> {
        match KILLS.get() {
            None => Ok(()),
            Some(Kill::Refuse) => Err(cosca::error::Error::Io(std::io::Error::other(KILL_REFUSED))),
            Some(Kill::RefuseTree) if site == Site::Tree => {
                Err(cosca::error::Error::Io(std::io::Error::other(KILL_REFUSED)))
            }
            Some(Kill::RefuseTree) => Ok(()),
            Some(Kill::Panic) => panic!("the kill panicked (test hook)"),
        }
    }

    /// What a waiter's `child.wait()` does instead of waiting.
    #[derive(Clone, Copy)]
    pub(crate) enum Wait {
        /// Fail as a broken wait mechanism would: with this OS error, or with [`WAIT_FAILED`].
        Fail(Option<i32>),
        /// Panic, ending the waiter thread without it reporting the child done.
        Panic,
    }

    /// Make the waiters this thread makes while the returned guard lives do `what` rather than
    /// wait. Read when the waiter is made, not when it runs.
    pub(crate) fn waits(what: Wait) -> Waiting {
        WAITS.set(Some(what));
        Waiting
    }

    /// See [`waits`].
    pub(crate) struct Waiting;

    impl Drop for Waiting {
        fn drop(&mut self) {
            WAITS.set(None);
        }
    }

    pub(super) fn waiting() -> Option<Wait> {
        WAITS.get()
    }

    pub(super) fn wait(what: Option<Wait>) -> Result<(), cosca::error::Error> {
        match what {
            None => Ok(()),
            Some(Wait::Fail(Some(code))) => Err(cosca::error::Error::Io(std::io::Error::from_raw_os_error(code))),
            Some(Wait::Fail(None)) => Err(cosca::error::Error::Io(std::io::Error::other(WAIT_FAILED))),
            Some(Wait::Panic) => panic!("the wait panicked (test hook)"),
        }
    }

    /// Let this thread make only `n` more waiter threads, until the returned guard drops.
    pub(crate) fn threads(n: usize) -> Allowance {
        THREADS.set(Some(n));
        Allowance
    }

    /// See [`threads`].
    pub(crate) struct Allowance;

    impl Drop for Allowance {
        fn drop(&mut self) {
            THREADS.set(None);
        }
    }

    /// How a waiter thread ended.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum WaiterEnd {
        Returned,
        Panicked,
    }

    static ENDS: Mutex<BTreeMap<String, WaiterEnd>> = Mutex::new(BTreeMap::new());
    static ENDED: Condvar = Condvar::new();

    /// Held for the whole life of the waiter thread's closure, so its drop runs after every log the
    /// waiter can make: it records how the thread ended under its daemon id and wakes
    /// [`waiter_end`].
    pub(super) struct WaiterExit(String);

    pub(super) fn waiter_running(id: &str) -> WaiterExit {
        WaiterExit(id.to_string())
    }

    impl Drop for WaiterExit {
        fn drop(&mut self) {
            let end = if std::thread::panicking() {
                WaiterEnd::Panicked
            } else {
                WaiterEnd::Returned
            };
            ENDS.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(std::mem::take(&mut self.0), end);
            ENDED.notify_all();
        }
    }

    /// Block until the waiter thread made for `id` has ended, then remove and return how. For the
    /// tests where `wait_for_child_or_stop` detaches the waiter, so they cannot join it: everything
    /// the waiter logged happens-before this returns.
    pub(crate) fn waiter_end(id: &str) -> WaiterEnd {
        let mut ends = ENDS.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(end) = ends.remove(id) {
                return end;
            }
            ends = ENDED.wait(ends).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Whether a waiter-end record for `id` is waiting to be consumed by [`waiter_end`]. Never blocks.
    pub(crate) fn has_waiter_end(id: &str) -> bool {
        ENDS.lock().unwrap_or_else(PoisonError::into_inner).contains_key(id)
    }

    pub(super) fn make_thread() -> std::io::Result<()> {
        match THREADS.get() {
            Some(0) => Err(std::io::Error::other(NO_THREAD)),
            Some(n) => {
                THREADS.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "stop_bus_tests.rs"]
mod stop_bus_tests;
