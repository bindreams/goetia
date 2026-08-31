//! The stop signal shared between the SCM control-handler callback and the
//! supervisor loop, and the composition that lets the loop block on either
//! "the current child exited" or "a stop was requested" without polling.
//!
//! Process-tree containment and waiting are `cosca::Child`'s job (see
//! `main.rs`'s module doc comment for why nothing here hand-rolls a Job
//! Object or a `WaitForMultipleObjects` call); this module only supplies
//! the piece `cosca` does not: a way to interrupt a blocking `child.wait()`
//! when SCM delivers `Stop`.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use cosca::Child;

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
    /// itself owns, per `main.rs`'s `service_control_handler::register`).
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

    /// Block until `child` exits or a stop is requested — real
    /// condition-variable blocking, never a poll loop. A background thread
    /// performs the actual (real, blocking, event-driven) `child.wait()`;
    /// this call itself only waits for whichever of the two flags a
    /// notifier sets first, so a stop that arrives while the child is
    /// merely spawning (before this call is even entered) is not missed —
    /// the `Condvar::wait_while` predicate is checked before ever blocking.
    pub fn wait_for_child_or_stop(&self, child: &Child) -> WaitOutcome {
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
            if g.stopping {
                WaitOutcome::Stopping
            } else {
                WaitOutcome::ChildExited
            }
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
