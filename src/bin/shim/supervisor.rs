//! The shim's restart-policy decision, kept pure and free of every Win32
//! dependency so it is unit-testable without a real service host — see
//! `main.rs` for the SCM wiring and the real wait primitives that feed it.
//!
//! Deliberately compiled on every platform, not only `#[cfg(windows)]` (see
//! `main.rs`'s own `mod supervisor;` line): the whole point of keeping this
//! module Win32-free is that its unit tests run on every CI cell this
//! workspace builds on, not only the Windows one. `service.rs` (the only
//! non-test caller) is Windows-only, so on every other platform nothing
//! outside `#[cfg(test)]` calls into this module at all — hence the
//! blanket allow below, mirroring `src/main.rs`'s identical
//! `#[cfg_attr(test, allow(dead_code))]` for the same shape of problem.
#![cfg_attr(not(any(test, windows)), allow(dead_code))]

use std::time::Duration;

use goetia::spec::Restart;

// Constants ===========================================================================================================

/// The delay `goetia-shim` waits before respawning when `goetia.yaml` sets
/// no `restart-delay`. Matches
/// `backend::scm::generate::DEFAULT_RESTART_DELAY`, the identical knob for
/// `type: managed`'s SCM recovery actions, and for the identical reason
/// documented there: long enough that a child that dies instantly on start
/// (a missing binary, a port already in use) does not spin at full CPU
/// retrying every microsecond, short enough that a benign flap recovers
/// quickly.
///
/// Reconciled against the two native defaults this project's other
/// backends inherit rather than choose: systemd's `RestartSec=` defaults to
/// 100ms, launchd's `ThrottleInterval` to 10s. 1s sits between them
/// deliberately — closer to systemd's "recover fast" instinct than to
/// launchd's "assume something is seriously wrong" one, since `type:
/// simple` daemons on Windows are typically the same kind of quick-flapping
/// network client/proxy processes systemd's own default was tuned for —
/// while still being long enough to matter for a persistently-failing
/// spawn (see `ChildOutcome::SpawnFailed`, folded into the same pacing).
/// Using the same 1s here as `type: managed` also means the two
/// Goetia-owned restart mechanisms do not silently disagree about what
/// "no configured delay" means on the same platform.
pub const DEFAULT_RESTART_DELAY: Duration = Duration::from_secs(1);

// ChildOutcome ========================================================================================================

/// What the most recent spawn attempt produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildOutcome {
    /// The child ran and exited with this code.
    Exited(i32),
    /// The child could not even be launched — a missing binary, exec
    /// denied, or any other `std::process::Command::spawn` failure. Folded
    /// into the same failure path as a nonzero exit: both are the kind of
    /// failure `restart: on-failure`/`always` exists to recover from, and
    /// treating them identically is what keeps a persistently-missing
    /// binary from being retried with no pacing at all instead of at
    /// `restart-delay`'s cadence like any other failure.
    SpawnFailed,
}

impl ChildOutcome {
    fn is_failure(self) -> bool {
        match self {
            ChildOutcome::Exited(code) => code != 0,
            ChildOutcome::SpawnFailed => true,
        }
    }
}

// RestartDecision =====================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    Respawn { delay: Duration },
    Stop,
}

/// The one place `restart:`'s three policies are interpreted.
///
/// `stopping` is an explicit, separate input — never inferred from
/// `outcome` alone. Terminating the child's Job Object makes it exit
/// non-zero, indistinguishable from a crash by exit code alone; a decision
/// that only looked at the exit code would respawn a replacement *outside*
/// the now-terminated job while the caller goes on to report
/// `SERVICE_STOPPED`, leaving an orphan daemon SCM believes is stopped.
/// Checking `stopping` first, unconditionally, is what makes that
/// structurally impossible rather than merely unlikely: see `main.rs`'s
/// wait loop, which samples `stopping` from the same `AtomicBool` the stop
/// control handler itself writes, read once and reused for both this
/// decision and the exit-code interpretation above it — never from *which*
/// handle a `WaitForMultipleObjects` call happened to report first, which
/// (both handles can legitimately signal together) cannot disambiguate the
/// two on its own.
pub fn decide_restart(
    restart: Restart,
    outcome: ChildOutcome,
    stopping: bool,
    restart_delay: Option<Duration>,
) -> RestartDecision {
    if stopping {
        return RestartDecision::Stop;
    }
    let should_respawn = match restart {
        Restart::Never => false,
        Restart::OnFailure => outcome.is_failure(),
        Restart::Always => true,
    };
    if should_respawn {
        RestartDecision::Respawn {
            delay: restart_delay.unwrap_or(DEFAULT_RESTART_DELAY),
        }
    } else {
        RestartDecision::Stop
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod supervisor_tests;
