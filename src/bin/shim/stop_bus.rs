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

use std::sync::{Condvar, Mutex};
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

    /// Block until `child` exits or a stop is requested, returning which. On
    /// a stop, also tears the whole contained tree down (`kill_tree`, with
    /// a fallback to killing just the direct child — see below) before
    /// returning.
    ///
    /// **Why the teardown happens here, inside this call, rather than in the
    /// caller after it returns.** A background thread performs the real,
    /// blocking `child.wait()`; `std::thread::scope` (below) does not
    /// return until every thread it spawned has finished, so that
    /// background thread's `child.wait()` must itself return before this
    /// function can. For a daemon that does not exit on its own — the
    /// ordinary `type: simple` case — nothing makes `child.wait()` return
    /// except killing the child. If the kill happened only after this call
    /// returned (the caller's job in an earlier version of this module),
    /// the scope's own join would block forever waiting for a worker that
    /// is parked on a process nothing has told it to kill yet — a deadlock
    /// on every commanded stop of a running daemon, not a rare case.
    /// Performing the kill from inside the scope, before the closure's
    /// return value is produced, is what lets the background thread's wait
    /// actually resolve.
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
    /// footnote, but it is always logged, and it is never a hang.
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
    pub fn wait_for_child_or_stop(&self, child: &Child, id: &str) -> WaitOutcome {
        {
            let mut g = self.state.lock().expect("StopBus mutex poisoned");
            g.child_done = false;
        }
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // Notified on *both* `Ok` and `Err`: an `Err` here means the
                // wait mechanism itself failed, which says nothing about
                // whether the child is still running — it is not evidence
                // to keep waiting on, and staying silent would leave the
                // outer `Condvar::wait_while` below with no further waker
                // at all in the (ordinary, no-stop-requested) case, hanging
                // the supervisor loop forever on a child that may already
                // be gone.
                if let Err(e) = child.wait() {
                    logging::log_failure(id, &format!("background wait on child: {e}"));
                }
                let mut g = self.state.lock().expect("StopBus mutex poisoned");
                g.child_done = true;
                self.cvar.notify_all();
            });
            let g = self.state.lock().expect("StopBus mutex poisoned");
            let g = self
                .cvar
                .wait_while(g, |g| !g.stopping && !g.child_done)
                .expect("StopBus mutex poisoned");
            if !g.stopping {
                return WaitOutcome::ChildExited;
            }
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
            // The background thread's `child.wait()` above can now return
            // (the child is dead, or `kill_tree`/`kill` at least tried and
            // logged why not), so the scope below can join it.
            WaitOutcome::Stopping
        })
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

#[cfg(test)]
#[path = "stop_bus_tests.rs"]
mod stop_bus_tests;
