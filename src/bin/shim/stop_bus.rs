//! The stop signal shared between the SCM control-handler callback and the
//! supervisor loop, and the composition that lets the loop block on either
//! "the current child exited" or "a stop was requested" without polling.
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
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use cosca::Child;

use crate::logging;

#[derive(Default)]
struct Gate {
    stopping: bool,
    /// Reset by [`StopBus::wait_for_child_or_stop`] before each spawn's
    /// wait, so a stale completion from a *previous* child can never be
    /// misread as the current one's.
    child_done: bool,
}

pub struct StopBus {
    state: Mutex<Gate>,
    cvar: Condvar,
}

pub enum WaitOutcome {
    ChildExited,
    Stopping,
}

// Waiter ==============================================================================================================

/// The thread [`StopBus::wait_for_child_or_stop`]'s blocking `child.wait()` runs on, made before
/// the daemon is spawned and parked until it is handed that daemon. Handed nothing — the spawn
/// failed, so there is nothing to wait on — it ends.
///
/// **Why it is made first, and never after.** This wait is the only thing that ever observes the
/// daemon exiting or tears its tree down on a stop. Making it after the spawn means a daemon can
/// be running before the shim knows whether anything can watch it, and the OS refusing a thread
/// there leaves that daemon orphaned: nothing supervising it, nothing to stop it, no kill-tree
/// teardown — with SCM told, at best, that its service just died. `std::thread::Scope::spawn`
/// (this module's shape before this type existed) could not even report that refusal: it has no
/// `Result` and panics, taking the service process down with the daemon already up.
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
}

impl Waiter {
    fn new(bus: &Arc<StopBus>, id: &str) -> io::Result<Waiter> {
        #[cfg(test)]
        test_hook::make_thread()?;
        let (hand, rx) = mpsc::channel::<Arc<Child>>();
        let bus = Arc::clone(bus);
        let id = id.to_string();
        let thread = std::thread::Builder::new()
            .name("goetia-shim-waiter".to_string())
            .spawn(move || {
                let Ok(child) = rx.recv() else {
                    return; // never handed a daemon: none was spawned
                };
                // Notified on *both* `Ok` and `Err`: an `Err` here means the
                // wait mechanism itself failed, which says nothing about
                // whether the child is still running — it is not evidence
                // to keep waiting on, and staying silent would leave
                // `wait_for_child_or_stop`'s `Condvar::wait_while` with no
                // further waker at all in the (ordinary, no-stop-requested)
                // case, hanging the supervisor loop forever on a child that
                // may already be gone.
                if let Err(e) = child.wait() {
                    logging::log_failure(&id, &format!("background wait on child: {e}"));
                }
                let mut g = bus.state.lock().expect("StopBus mutex poisoned");
                g.child_done = true;
                bus.cvar.notify_all();
            })?;
        Ok(Waiter { hand, thread })
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
    /// here) would reopen exactly the race `supervisor::decide_restart`'s
    /// explicit `stopping` input exists to close: this callback cannot know
    /// whether the supervisor loop has spawned its next child yet, so a stop
    /// delivered in that gap could kill nothing and be silently lost. Routing
    /// the actual kill through the loop's own thread, gated on this same
    /// flag re-checked at every wake-up, means a stop is never missed
    /// regardless of when it arrives.
    pub fn request_stop(&self) {
        let mut g = self.state.lock().expect("StopBus mutex poisoned");
        g.stopping = true;
        self.cvar.notify_all();
    }

    pub fn is_stopping(&self) -> bool {
        self.state.lock().expect("StopBus mutex poisoned").stopping
    }

    /// The thread that will run the blocking `child.wait()` for this bus's next wait, made before
    /// the daemon it will wait on is spawned. (This bus's own wait blocks on the condvar; the
    /// [`Waiter`]'s thread is what blocks on the child.) See [`Waiter`] for why that order is the
    /// whole point, and `io::Result` rather than a panic the whole mechanism.
    pub fn waiter(self: &Arc<Self>, id: &str) -> io::Result<Waiter> {
        Waiter::new(self, id)
    }

    /// Block until `child` exits or a stop is requested, returning which. On
    /// a stop, also tears the whole contained tree down (`kill_tree`, with
    /// a fallback to killing just the direct child — see below) before
    /// returning.
    ///
    /// `waiter` is consumed here: it was made before `child` was spawned
    /// ([`Waiter`]), and this is where it is handed the daemon it was made
    /// for. Taking it by value is what makes "no waiter, no wait" checkable
    /// at the call site rather than a convention.
    ///
    /// **Why the teardown happens here, inside this call, rather than in the
    /// caller after it returns.** The [`Waiter`]'s thread performs the real,
    /// blocking `child.wait()`, and this call joins it before returning, so
    /// that wait must itself return before this function can. For a daemon
    /// that does not exit on its own — the ordinary `type: simple` case —
    /// nothing makes `child.wait()` return except killing the child. If the
    /// kill happened only after this call returned (the caller's job in an
    /// earlier version of this module), the join would block forever waiting
    /// for a worker that is parked on a process nothing has told it to kill
    /// yet — a deadlock on every commanded stop of a running daemon, not a
    /// rare case. Killing first, then joining, is what lets that wait
    /// actually resolve.
    ///
    /// **Why it joins at all**, rather than leaving the waiter detached: the
    /// join is what makes the return value mean "the tree is dead" rather
    /// than "the kill was issued". `TerminateJobObject` returning does not
    /// mean the OS has finished signalling the process object, and the
    /// caller's very next act on `Stopping` is to report `SERVICE_STOPPED`
    /// to SCM. Only the waiter's own `child.wait()` completing proves the
    /// child is reaped, and joining is how this call learns that it has.
    ///
    /// **A panic between the hand-off and that join detaches the waiter**,
    /// which is a real difference from the `&Child`/`thread::scope` shape
    /// this replaced: a panic at one of the `expect`s below (or inside
    /// `kill_tree`/`kill`) drops the `JoinHandle` instead of joining it,
    /// and the detached thread still holds an `Arc<Child>` clone — so the
    /// caller's own `Arc` going out of scope during unwind does not drop
    /// the last reference, and `cosca::Child::drop`'s kill-tree teardown
    /// does not run. The daemon's Job Object is `KILL_ON_JOB_CLOSE`, so the
    /// tree still dies once the shim process exits; what is lost is the
    /// teardown before that. Worth knowing before anyone makes one of those
    /// `expect`s recoverable.
    ///
    /// **`kill_tree` can itself fail to kill anything** — `Err(Unsupported)`
    /// when this child holds no actionable containment mechanism (e.g. a
    /// nested/`Delegated` child, or one whose containment setup failed
    /// outright). That is exactly the same deadlock at one remove: if
    /// nothing kills the child, the background thread's `wait()` still
    /// never returns. `child.kill()` — the direct process handle, no
    /// containment required — is the fallback that keeps this call
    /// returning regardless; it cannot reach any of the child's own
    /// descendants, so a `kill_tree` failure genuinely does mean a reduced
    /// guarantee (root killed, tree possibly not), not merely a doc
    /// footnote. Both failures are always logged.
    ///
    /// **If that fallback `kill` fails too, this call does hang** (see the
    /// `Err(e2)` arm below, which logs and carries on): nothing has told the
    /// child to exit, so the waiter's `child.wait()` never returns and the
    /// join below blocks for as long as that child lives — SCM's stop never
    /// completes. `thread::scope`'s implicit join had exactly the same
    /// property, so this is not new here; and bounding the join would mean
    /// returning while a live thread still holds this child, which is what
    /// the paragraph above exists to prevent. An unkillable direct child is
    /// the case where there is nothing good to do.
    ///
    /// **No `wait_tree` call after `kill_tree` succeeds.** `cosca`'s Job
    /// Object `hard_kill` (what `kill_tree` calls) closes the job handle as
    /// part of terminating it, so a subsequent `wait_tree` always fails
    /// with "the job handle was already closed" — not because anything is
    /// still running. `TerminateJobObject`, which `hard_kill` issues, is
    /// itself synchronous per its documented Win32 contract (every member
    /// of the job is already terminated by the time the call returns), so
    /// `kill_tree`'s own `Ok(())` already is the confirmation; a follow-up
    /// `wait_tree` would only ever produce a spurious logged failure on
    /// every ordinary successful stop.
    pub fn wait_for_child_or_stop(&self, waiter: Waiter, child: &Arc<Child>, id: &str) -> WaitOutcome {
        {
            let mut g = self.state.lock().expect("StopBus mutex poisoned");
            g.child_done = false;
        }
        // After the reset, never before it: the waiter is parked until this
        // call, so nothing can report *this* child done until it is handed
        // one, and a reset afterwards could erase that report.
        let waiting = waiter.watch(Arc::clone(child));
        let g = self.state.lock().expect("StopBus mutex poisoned");
        let g = self
            .cvar
            .wait_while(g, |g| !g.stopping && !g.child_done)
            .expect("StopBus mutex poisoned");
        let outcome = if g.stopping {
            // Release the lock before calling into `child` — `kill_tree`/
            // `kill` need it not, and holding it across a kernel call would
            // block `is_stopping()`/`request_stop()` callers (e.g. a
            // second, redundant SCM stop control) for no reason.
            drop(g);
            if let Err(e) = child.kill_tree() {
                logging::log_failure(
                    id,
                    &format!(
                        "kill_tree on stop: {e}; falling back to killing the direct child only (its own \
                         descendants, if any, cannot be reached without a containment mechanism)"
                    ),
                );
                if let Err(e2) = child.kill() {
                    logging::log_failure(id, &format!("fallback kill on stop: {e2}"));
                }
            }
            WaitOutcome::Stopping
        } else {
            drop(g);
            WaitOutcome::ChildExited
        };
        // Joined only now, after the kill above: the waiter's `child.wait()`
        // can return (the child exited on its own, or the kill at least
        // tried and logged why not), and its completing is what makes this
        // call's return mean the child is really reaped.
        if waiting.join().is_err() {
            logging::log_failure(id, "the thread waiting on the child panicked");
        }
        outcome
    }

    /// Wait up to `delay` for a stop request — `restart-delay` itself, not a
    /// synchronization workaround; see
    /// `supervisor::DEFAULT_RESTART_DELAY`'s doc comment for the policy this
    /// implements. Returns `true` if a stop was requested during the wait
    /// (the caller must not respawn), `false` if the full delay elapsed.
    pub fn wait_or_stop(&self, delay: Duration) -> bool {
        let g = self.state.lock().expect("StopBus mutex poisoned");
        let (g, _timed_out) = self
            .cvar
            .wait_timeout_while(g, delay, |g| !g.stopping)
            .expect("StopBus mutex poisoned");
        g.stopping
    }
}

// test_hook ===========================================================================================================

/// Makes waiter-thread creation fail on demand, on this thread only, so the path that must refuse
/// to launch a daemon when it does is testable. Mirrors `goetia::backend::bounded`'s own hook,
/// which is `#[cfg(test)]` inside the library and therefore invisible here — see [`Waiter`].
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;

    thread_local! {
        static THREADS: Cell<Option<usize>> = const { Cell::new(None) };
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

    pub(super) fn make_thread() -> std::io::Result<()> {
        match THREADS.get() {
            Some(0) => Err(std::io::Error::other("no thread may be made (test hook)")),
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
