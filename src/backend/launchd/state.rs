//! Pure classification of launchd's live-state vocabulary: the part of `status` that needs no root,
//! no real launchd, and no macOS. Not `#[cfg]`-gated, so it compiles and its tests run everywhere,
//! unlike `manager`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::manager::{Exit, State};

/// Classifies one `launchctl print` body into the `(State, pid)` pair `status`/`list` report.
///
/// Between `bootstrap` and the job's binary running, the `state = ` line reads `xpcproxy` or
/// `trampoline` with the job's `pid` attached. That is a job that is starting, not one that is not
/// running, so with a pid it is `Running`: launchd's side of the start is done, and the exec that
/// follows is the user process's.
pub(crate) fn classify(print_body: &str) -> (State, Option<u32>) {
    let pid = has_process(print_body);
    let state = match find_field(print_body, "state").map(str::trim) {
        Some("running") => State::Running,
        Some("xpcproxy") | Some("trampoline") if pid.is_some() => State::Running,
        Some("not running") | Some("exited") => State::Stopped,
        // Everything else is `Unknown` rather than a guess: `spawn scheduled` (decided to start,
        // not yet forked, no pid), `xpcproxy`/`trampoline` without a pid (never observed, so not
        // taken for a start), any `state` a future macOS introduces, and no `state` line at all.
        //
        // One of three places a `State::Starting` would be produced if goetia grows one (routed
        // post-0.1.0): also `map_state` in `src/backend/scm/manager.rs` and `status_from_unit` in
        // `src/backend/systemd/manager/systemctl.rs`.
        _ => State::Unknown,
    };
    (state, pid)
}

/// [`classify`] for a task's job, which a run starts and ends rather than keeps running: `exited` and
/// `not running` are [`State::Idle`], and `spawn scheduled` is [`State::Running`] with no pid, a start
/// launchd has accepted and not yet forked (`kickstart` returns before the fork), so a `run` straight
/// after a `start` is refused as running and a `stop` does not take it for an idle task.
pub(crate) fn classify_task(print_body: &str) -> (State, Option<u32>) {
    match classify(print_body) {
        (State::Stopped, pid) => (State::Idle, pid),
        (State::Unknown, pid) if spawn_scheduled(print_body) => (State::Running, pid),
        other => other,
    }
}

/// Whether one `launchctl print` body shows a start launchd has accepted and not yet forked
/// (`state = spawn scheduled`, no pid).
pub(crate) fn spawn_scheduled(print_body: &str) -> bool {
    find_field(print_body, "state").map(str::trim) == Some("spawn scheduled")
}

/// How a process ended, from the `wait(2)` status a kqueue `NOTE_EXITSTATUS` event carries: the exit
/// code (its second byte) for a process that exited, else the signal (its low seven bits; bit 7 says a
/// core was dumped) that killed it.
pub(crate) fn exit_of(raw_status: i32) -> Exit {
    match raw_status & 0x7f {
        0 => Exit::Code((raw_status >> 8) & 0xff),
        signal => Exit::Signal(signal),
    }
}

/// The job's pid in one `launchctl print` body, if it holds a process: from `xpcproxy` or
/// `trampoline` on, whatever the `state`. `None` for a job with no `pid` line, `spawn scheduled`
/// included.
pub(crate) fn has_process(print_body: &str) -> Option<u32> {
    find_field(print_body, "pid").and_then(|s| s.trim().parse().ok())
}

/// How the job's last run ended, from one `launchctl print` body: `last exit code = N` is
/// [`Exit::Code`], and `last terminating signal = Terminated: 15` (the signal's name, a colon, its
/// number) is [`Exit::Signal`]. `None` before the first exit (`last exit code = (never exited)`), and
/// for a body with neither line. `print` has one of the two lines at a time, and while a run is in
/// flight (a `pid` line) it holds the run before's.
pub(crate) fn last_result(print_body: &str) -> Option<Exit> {
    if let Some(code) = find_field(print_body, "last exit code") {
        return code.trim().parse().ok().map(Exit::Code);
    }
    let signal = find_field(print_body, "last terminating signal")?;
    signal.rsplit(':').next()?.trim().parse().ok().map(Exit::Signal)
}

/// What the kernel showed of the process `kickstart -p` named, once its start time was read: how it stands
/// after its exec, or that it did not get there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Then {
    /// No process at a later read, or one the watch showed had exited after it.
    Gone,
    /// The path read `xpcproxy`, and the process exited with no exec: with its `wait(2)` status, where the
    /// kernel reported one.
    ExitedBeforeExec(Option<i32>),
    /// Alive after its exec, `stopped` or not.
    Process { stopped: bool },
}

/// What `run` makes of the launch `kickstart -p` named.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Launch {
    /// No process when it was registered, or at the read of its start time: goetia never saw it, so whose
    /// launch it was is not known. The run ends in doubt.
    Lost,
    /// An older launch that is over: it started at or before our `debug` returned and is gone, or exited
    /// before its exec. Another pass is made.
    Ended,
    /// Started after our `debug`, and exited before its exec, with its `wait(2)` status where the kernel
    /// reported one: our launch, whose program never started. The run ends, not started.
    NotStarted(Option<i32>),
    /// Started after our `debug`, and gone at a read after its exec: our launch, which ended before goetia
    /// could follow it. The run ends, unrecorded.
    Unrecorded,
    /// Stopped, and started after our `debug`: the launch that `debug` held for this run.
    Held,
    /// Stopped, and started before our `debug` returned: an older launch that took a hold, which is
    /// resumed and waited for.
    TookHold,
    /// Not stopped, and started before our `debug` returned: a run already in flight (a scheduled one),
    /// which is waited for.
    InFlight,
    /// Not stopped, and started after our `debug`: our own `kickstart` launched it with no hold, which
    /// goetia does not follow.
    Unheld,
}

/// The [`Launch`] of a process that `started` at the time shown and then stood as `then`, where `ta` is the
/// time read after the `launchctl debug` process was reaped (both in `mach_absolute_time` units). A start
/// after `ta` began after `debug`; one at or before it began before `debug` returned. [`Launch::Lost`] is
/// not decided here: it is a process whose start time was never read.
pub(crate) fn launch_of(then: Then, started: u64, ta: u64) -> Launch {
    let ours = started > ta;
    match (then, ours) {
        (Then::Process { stopped: true }, true) => Launch::Held,
        (Then::Process { stopped: true }, false) => Launch::TookHold,
        (Then::Process { stopped: false }, false) => Launch::InFlight,
        (Then::Process { stopped: false }, true) => Launch::Unheld,
        (Then::Gone | Then::ExitedBeforeExec(_), false) => Launch::Ended,
        (Then::Gone, true) => Launch::Unrecorded,
        (Then::ExitedBeforeExec(status), true) => Launch::NotStarted(status),
    }
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
        let queued = spawn_scheduled(print_body);
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
