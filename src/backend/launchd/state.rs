//! Pure classification of launchd's live-state vocabulary: the part of `status` that needs no root,
//! no real launchd, and no macOS. Not `#[cfg]`-gated, so it compiles and its tests run everywhere,
//! unlike `manager`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::manager::State;

/// Classifies one `launchctl print` body into the `(State, pid)` pair `status`/`list` report.
///
/// Between `bootstrap` and the job's binary running, the `state = ` line reads `xpcproxy` or
/// `trampoline` with the job's `pid` attached. That is a job that is starting, not one that is not
/// running, so with a pid it is `Running`: launchd's side of the start is done, and the exec that
/// follows is the user process's.
pub(crate) fn classify(print_body: &str) -> (State, Option<u32>) {
    let pid = find_field(print_body, "pid").and_then(|s| s.trim().parse().ok());
    let state = match find_field(print_body, "state").map(str::trim) {
        Some("running") => State::Running,
        Some("xpcproxy") | Some("trampoline") if pid.is_some() => State::Running,
        Some("not running") | Some("exited") => State::Stopped,
        // Everything else is `Unknown` rather than a guess: `spawn scheduled` (decided to start,
        // not yet forked, no pid), `xpcproxy`/`trampoline` without a pid, any `state` a future
        // macOS introduces, and no `state` line at all.
        //
        // One of three places a `State::Starting` would be produced if goetia grows one (routed
        // post-0.1.0): also `map_state` in `src/backend/scm/manager.rs` and `status_from_unit` in
        // `src/backend/systemd/manager/systemctl.rs`.
        _ => State::Unknown,
    };
    (state, pid)
}

/// Whether one `launchctl print` body shows a job whose `bootout` has returned and whose process has
/// not exited: `state = SIGTERMed` (measured on macOS CI, 2026-11-09). Apple does not guarantee the
/// format, so any other `state`, one this does not recognise included, is a loaded job.
pub(crate) fn tearing_down(print_body: &str) -> bool {
    find_field(print_body, "state").map(str::trim) == Some("SIGTERMed")
}

/// What a `launchctl print` must show before goetia ends the `kickstart` an expiry left running.
/// Ended before launchd has the request, the start is lost, so only a reading that cannot be an
/// older instance of the job counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// `spawn scheduled`: a start queued. Where a `running` could be an older instance, which would
    /// not hold this request: after a read that was not a clean `Stopped`, and, conservatively,
    /// after the corrective cycle's `bootout`, where a wrong kill costs the start and leaving it
    /// costs a `launchctl` running for up to `restart_delay`.
    Queued,
    /// A start queued, or the job running (`pid` attached from `xpcproxy` on): after a clean
    /// `Stopped` read, where a `running` is a fork after it.
    QueuedOrRunning,
}

impl Evidence {
    /// Whether one `launchctl print` body shows it. Any other reading, an unrecognised or missing
    /// `state` included, is no evidence that launchd has the request.
    pub(crate) fn shown_by(self, print_body: &str) -> bool {
        let queued = find_field(print_body, "state").map(str::trim) == Some("spawn scheduled");
        match self {
            Evidence::Queued => queued,
            Evidence::QueuedOrRunning => queued || classify(print_body).0 == State::Running,
        }
    }
}

/// Extracts the value on the first `key = value` line of a `launchctl print`-style body (one
/// tab-indented field per line).
pub(crate) fn find_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(prefix.as_str()))
}

/// `kickstart -p`'s stdout: the bare decimal pid and a newline, nothing else. `None` unless the
/// whole `<digits>\n` line is present.
///
/// The newline is the point: a capture cut mid-write would turn `"4766\n"` into `"47"`, which
/// parses as a different pid, possibly live and someone else's. The caller in `manager` reads a pid
/// only from a capture that reached EOF, so neither rule alone stands between a truncated read and a
/// confident answer.
pub(crate) fn kickstart_pid(stdout: &str) -> Option<u32> {
    let digits = stdout.strip_suffix('\n')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Digits too large for a `u32` are not a pid either.
    digits.parse().ok()
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
