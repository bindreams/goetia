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

    /// Block until `child` exits or a stop is requested, returning which.
    /// On a stop, also tears the whole contained tree down (`kill_tree` then
    /// `wait_tree`, confirming via its real kernel drain edge that every
    /// member — not only the direct child — has exited) before returning.
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
    pub fn wait_for_child_or_stop(&self, child: &Child, id: &str) -> WaitOutcome {
        {
            let mut g = self.state.lock().expect("StopBus mutex poisoned");
            g.child_done = false;
        }
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // Best-effort: a wait failure here still lets the outer
                // `Condvar` wake on `stopping` if one was also requested: it
                // will not spuriously look like a completed child.
                if child.wait().is_ok() {
                    let mut g = self.state.lock().expect("StopBus mutex poisoned");
                    g.child_done = true;
                    self.cvar.notify_all();
                }
            });
            let g = self.state.lock().expect("StopBus mutex poisoned");
            let g = self
                .cvar
                .wait_while(g, |g| !g.stopping && !g.child_done)
                .expect("StopBus mutex poisoned");
            if !g.stopping {
                return WaitOutcome::ChildExited;
            }
            // Release the lock before calling into `child` — neither
            // `kill_tree` nor `wait_tree` needs it, and holding it across a
            // kernel wait would block `is_stopping()`/`request_stop()`
            // callers (e.g. a second, redundant SCM stop control) for no
            // reason.
            drop(g);
            if let Err(e) = child.kill_tree() {
                logging::log_failure(id, &format!("kill_tree on stop: {e}"));
            }
            if let Err(e) = child.wait_tree() {
                logging::log_failure(id, &format!("wait_tree confirming stop: {e}"));
            }
            // The background thread's `child.wait()` above can now return
            // (the tree is dead), so the scope below can join it.
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
