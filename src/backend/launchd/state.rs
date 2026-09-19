//! Pure classification of launchd's live-state vocabulary — the half of
//! `status`'s "read `launchctl print`, guess a [`State`]" story that needs
//! no root, no real launchd, and no macOS. Not `#[cfg]`-gated: this compiles
//! and its tests run on every platform, unlike
//! [`manager`](crate::backend::launchd::manager), which actually shells out
//! to `launchctl` and is confined to `target_os = "macos"`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::manager::State;

/// Classifies one `launchctl print` body into the `(State, pid)` pair
/// `status`/`list` report.
///
/// Between `bootstrap` and a job's binary actually running, launchd's
/// `state = ` line passes through `xpcproxy`/`trampoline`: its own spawn
/// trampoline, holding the job's `pid` — launchd has already forked and
/// assigned it — while it execs into the job's real binary. That exec is
/// the *user process's* remaining transition, not launchd's, and is
/// explicitly not part of this contract: launchd's own side of `start` is
/// already done by the time `state` reads `xpcproxy`/`trampoline` with a
/// pid attached. Reporting that as `Stopped` — a bare `Some(_) =>
/// State::Stopped` catch-all once did exactly that — is the same
/// confidently-wrong answer the permission-failure branch in
/// `query_live_state` above this call already refuses to give: a job that
/// is in fact starting, reported as one that definitely is not.
pub(crate) fn classify(print_body: &str) -> (State, Option<u32>) {
    let pid = find_field(print_body, "pid").and_then(|s| s.trim().parse().ok());
    let state = match find_field(print_body, "state").map(str::trim) {
        Some("running") => State::Running,
        Some("xpcproxy") | Some("trampoline") if pid.is_some() => State::Running,
        Some("not running") | Some("exited") => State::Stopped,
        // Everything else folds into `Unknown` rather than guessing:
        // `spawn scheduled` (a job launchd has decided to start but not yet
        // forked — no pid to point to yet); `xpcproxy`/`trampoline` without
        // a pid (never observed, but nothing rules it out); any `state`
        // value a future macOS introduces; and no `state` line at all.
        //
        // This is one of three places a `State::Starting` would be
        // produced if goetia grows one (routed post-0.1.0) — the other two
        // are `map_state` in `src/backend/scm/manager.rs` and
        // `status_from_unit` in `src/backend/systemd/manager/systemctl.rs`.
        _ => State::Unknown,
    };
    (state, pid)
}

/// Extracts the value on the first `key = value` line of a `launchctl
/// print`-style body — tab-indented, one field per line.
pub(crate) fn find_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(prefix.as_str()))
}

/// `kickstart -p`'s stdout: the bare decimal pid and a newline, and nothing
/// else. `None` unless the whole `<digits>\n` line is present.
///
/// The trailing newline is the point of this function, not a formality. A
/// capture the deadline cut mid-write would otherwise turn `"4766\n"` into
/// `"47"`, which parses cleanly as a *different* pid — one that may well be
/// live, and belong to someone else — and `start` would hand it back as its
/// confirmation, the clock having picked which process it claims to have
/// launched. Requiring the terminator is what makes "launchd finished
/// writing this" observable at all.
///
/// The caller in `manager` carries the same guard independently, reading a
/// pid only from a capture that reached EOF inside the deadline, so neither
/// rule is the only thing standing between a truncated read and a confident
/// answer.
pub(crate) fn kickstart_pid(stdout: &str) -> Option<u32> {
    let digits = stdout.strip_suffix('\n')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Still fallible after the digit check: a run of digits too large for a
    // `u32` is not a pid either.
    digits.parse().ok()
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
