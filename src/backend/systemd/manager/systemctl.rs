//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back.

use std::collections::BTreeMap;
use std::process::Command;

use crate::backend::bounded::{self, Capture, Finished, Role};
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State, Status};

pub(super) fn run_systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| Error::Other(format!("failed to run `systemctl {}`: {e}", args.join(" "))))
}

pub(super) fn daemon_reload() -> Result<()> {
    let output = run_systemctl(&["daemon-reload"])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl daemon-reload failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// `daemon-reload` after a unit file write that itself succeeded. A failure here must not be reported
/// as a successful install — the on-disk artifact and systemd's loaded view of it have diverged.
pub(super) fn daemon_reload_or_report(id: &str) -> Result<()> {
    daemon_reload().map_err(|e| {
        Error::Other(format!(
            "wrote the unit for `{id}` but `systemctl daemon-reload` failed, so systemd may not have \
             picked it up yet: {e}"
        ))
    })
}

/// The oldest systemd goetia runs against: generated units declare `Type=exec` (240), and every start
/// and stop runs `systemctl --show-transaction` (242).
const SYSTEMD_FLOOR: u32 = 242;

/// Refuse a systemd older than [`SYSTEMD_FLOOR`], as `systemctl --version` reports it. Asked wherever
/// goetia writes a unit or starts or stops one, and nowhere else: an older systemd does not reject
/// `Type=exec` but logs that it cannot parse it and runs the unit as `Type=simple`, silently losing
/// the failed-exec detection the directive is there for.
pub(super) fn require_supported() -> Result<()> {
    let output = run_systemctl(&["--version"])?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "`systemctl --version` failed, so goetia cannot tell whether this is systemd \
             {SYSTEMD_FLOOR}+: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    supported(&String::from_utf8_lossy(&output.stdout))
}

/// [`require_supported`]'s verdict on what `systemctl --version` printed: `systemd <N> (...)` first.
fn supported(version: &str) -> Result<()> {
    let first = version.lines().next().unwrap_or_default();
    let number = first
        .strip_prefix("systemd ")
        .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|digits| digits.parse::<u32>().ok());
    let found = match number {
        Some(n) if n >= SYSTEMD_FLOOR => return Ok(()),
        Some(n) => format!("`systemctl --version` reports {n}"),
        None => format!("`systemctl --version` names no version goetia can read ({first:?})"),
    };
    Err(Error::Other(format!(
        "goetia requires systemd {SYSTEMD_FLOOR} or newer, and {found}. An older systemd runs \
         goetia's `Type=exec` units as `Type=simple`, which cannot report a failed exec, and \
         rejects the `systemctl --show-transaction` every start and stop runs."
    )))
}

/// The argv for one `systemctl <verb> <unit>` under `budget`.
///
/// `--show-transaction` on every path: its announcement ([`enqueued`]) is the only evidence that
/// systemd took the request — `systemctl` exits `0` having done nothing in a chroot or under
/// `SYSTEMD_OFFLINE=1` — and on the bounded path it is also what the wait holds out for. A budget
/// that does not wait adds `--no-block`, which still announces the job (measured on systemd 257).
///
/// Its own function so that each path's flags are observable without a timing bet: no elevated test
/// can see `--no-block` (`start` returns either way). `systemctl_tests.rs` asserts both argvs.
fn verb_args<'a>(verb: &'a str, unit: &'a str, budget: Budget) -> Vec<&'a str> {
    if budget.waits() {
        vec![verb, "--show-transaction", unit]
    } else {
        vec![verb, "--no-block", "--show-transaction", unit]
    }
}

/// `systemctl`'s environment on every path. The announcement is `log_info`: an inherited
/// `SYSTEMD_LOG_LEVEL=warning` or `SYSTEMD_LOG_TARGET=null` silences it, and without it a request
/// systemd took is indistinguishable from one it ignored.
const AUDIBLE: [(&str, &str); 2] = [("SYSTEMD_LOG_LEVEL", "info"), ("SYSTEMD_LOG_TARGET", "console")];

/// Whether `systemctl --show-transaction`'s stderr says systemd has answered the request with a
/// job — the line it writes after `StartUnit`/`StopUnit` returns and before it waits for the job.
/// A substring, not a whole line: `SYSTEMD_LOG_TIME`/`SYSTEMD_LOG_LOCATION` prefix it.
fn enqueued(stderr: &[u8]) -> bool {
    const LINE: &[u8] = b"Enqueued anchor job ";
    stderr.windows(LINE.len()).any(|w| w == LINE)
}

/// One `systemctl <verb> <unit>` under `budget`, as three deliberately different paths.
///
/// A budget that does not wait becomes `systemctl <verb> --no-block`, the exact native expression
/// of "issue the request and return": systemd enqueues the job and `systemctl` exits without
/// waiting for it to complete. `Budget::Unbounded` is the plain blocking call. Both keep
/// `Command::output()`, because neither has anything to bound — cosca enters this module only where
/// a bound is actually required, which is the third path and only the third.
///
/// That third path waits on `deadline`, which the caller derived at verb entry, so discovery spends
/// the budget too — but the deadline bounds only the wait for the job, never whether it is
/// enqueued: until `systemctl` announces the job ([`enqueued`]) the wait is unbounded, so a budget
/// that discovery used up, or one a few milliseconds long, still reaches systemd, and an expiry is
/// then exactly what `budget::timed_out` says it is. `budget` only picks the path and the wording.
fn run_verb(verb: &str, unit: &str, budget: Budget, deadline: Deadline) -> Result<Finished> {
    require_supported()?;
    run_verb_via("systemctl", &verb_args(verb, unit, budget), budget, deadline)
}

/// [`run_verb`] with the program and argv named, so its bounded path is testable against a child
/// that cannot exit on its own.
fn run_verb_via(program: &str, args: &[&str], budget: Budget, deadline: Deadline) -> Result<Finished> {
    let failed = |e: String| Error::Other(format!("failed to run `{program} {}`: {e}", args.join(" ")));
    if !budget.waits() || budget == Budget::Unbounded {
        let output = Command::new(program)
            .args(args)
            .envs(AUDIBLE)
            .output()
            .map_err(|e| failed(e.to_string()))?;
        // `output()` reads both pipes to EOF before returning, so on either
        // unbounded path nothing was cut short and `complete` is simply true.
        return Ok(Finished::Exited {
            status: output.status,
            capture: Capture {
                stdout: output.stdout,
                stderr: output.stderr,
                complete: true,
            },
        });
    }

    let mut cmd = bounded::command(program, args).map_err(|e| failed(e.to_string()))?;
    for (key, value) in AUDIBLE {
        cmd.env(key, value);
    }
    let child = cmd.spawn().map_err(|e| failed(e.to_string()))?;
    bounded::wait_bounded(child, deadline, Role::AnnouncedRequest(enqueued)).map_err(|e| failed(e.to_string()))
}

/// Whether `line` is one `systemctl` writes on success as much as on failure — the transaction
/// `--show-transaction` announces, or a job's `finished` notice. Protocol, not diagnosis: left in, it
/// opens every failure message, and a diagnostic the deadline cut short can consist of nothing else.
fn is_protocol(line: &str) -> bool {
    enqueued(line.as_bytes())
        || line.contains("Enqueued auxiliary job ")
        || (line.contains("Job for ") && line.trim_end().ends_with(" finished."))
}

/// What `systemctl` said about its outcome: stderr, where it writes its diagnostics, or stdout when
/// stderr said nothing else — less the [`is_protocol`] lines either way.
fn diagnostic(capture: &Capture) -> String {
    let said = |stream: &[u8]| -> String {
        String::from_utf8_lossy(stream)
            .split_inclusive('\n')
            .filter(|line| !is_protocol(line))
            .collect()
    };
    let stderr = said(&capture.stderr);
    if stderr.is_empty() {
        said(&capture.stdout)
    } else {
        stderr
    }
}

/// The message for a `systemctl` invocation that exited non-zero, built from the whole [`Capture`]
/// rather than from its bytes alone.
///
/// `systemctl` writes nothing but protocol on success, so `complete == false` only ever bites here —
/// on the one path whose entire value is the diagnostic. A truncated read presented as the whole
/// story is how "goetia stopped reading" comes out reading as "systemd said nothing".
///
/// `systemctl_tests.rs` covers every shape a capture arrives in; nothing else can, since reaching
/// this through a real `systemctl` needs one that actually fails.
fn failed(verb: &str, unit: &str, capture: &Capture) -> Error {
    let diagnostic = diagnostic(capture);
    // Ahead of the `complete` branch, not inside the truncated one: both
    // streams can be empty either way, and trailing off after a colon is
    // just as uninformative when the read did finish. Only the *reason* it
    // is empty differs.
    if diagnostic.is_empty() {
        let why = if capture.complete {
            "and wrote no diagnostic"
        } else {
            "and no diagnostic was captured before goetia's budget expired"
        };
        return Error::Other(format!("systemctl {verb} {unit} failed, {why}"));
    }
    if capture.complete {
        return Error::Other(format!("systemctl {verb} {unit} failed: {diagnostic}"));
    }
    Error::Other(format!(
        "systemctl {verb} {unit} failed: {diagnostic} (truncated — goetia's budget expired while \
         reading the diagnostic, so this is a prefix of what systemd wrote)"
    ))
}

/// A `systemctl` that exited `0`: `Ok` only if systemd answered with a job. `systemctl` in a chroot,
/// or under `SYSTEMD_OFFLINE=1`, says "Running in chroot, ignoring command" and exits `0` having
/// asked systemd nothing — an image build's `install --start` is the ordinary case — and reporting
/// that as started or stopped is reporting a state nobody established.
///
/// Reliable on every path: without an announcement the bounded wait reads to EOF before it waits at
/// all, so a job line cannot be missing for want of reading.
fn took(verb: &str, unit: &str, capture: &Capture) -> Result<()> {
    if enqueued(&capture.stderr) {
        return Ok(());
    }
    let diagnostic = diagnostic(capture);
    let why = if diagnostic.is_empty() {
        " and said nothing about why".to_string()
    } else {
        format!(": {diagnostic}")
    };
    Err(Error::Other(format!(
        "systemctl {verb} {unit} exited 0 without enqueuing a job, so systemd did not act on it{why}"
    )))
}

/// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
/// needed. Idempotent: starting an already-active unit is a no-op that still exits 0. An expiry
/// means the job was enqueued and not waited out — see [`run_verb`]. That holds for an active unit
/// too: systemd answers the start with a job, not a state, so a budget shorter than that no-op job's
/// round trip expires where launchd and SCM report started (see `ServiceManager::start`).
///
/// Every generated unit carries `Type=exec` (see `generate::unit`'s doc comment for why), so a
/// bounded `systemctl start` confirms the `exec` itself succeeded — not merely that systemd forked
/// the process. A missing executable or missing user comes back as a failed start.
pub(super) fn start_impl(id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
    let unit = super::unit_name(id);
    match run_verb("start", &unit, budget, deadline)? {
        Finished::Exited { status, capture } if status.success() => took("start", &unit, &capture),
        Finished::Exited { capture, .. } => Err(failed("start", &unit, &capture)),
        Finished::Expired => Err(budget::timed_out(id, "running", budget)),
    }
}

/// Idempotent per `ServiceManager::stop`'s doc comment. Exit code 5 ("unit not loaded") means there
/// was nothing to stop — the same convention `tests/support/service_guard.rs` already uses for
/// cleanup — which is success here, not a failure to stop something that was never running.
pub(super) fn stop_impl(id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
    let unit = super::unit_name(id);
    match run_verb("stop", &unit, budget, deadline)? {
        Finished::Exited { status, capture } if status.success() => took("stop", &unit, &capture),
        Finished::Exited { status, .. } if status.code() == Some(5) => Ok(()),
        Finished::Exited { capture, .. } => Err(failed("stop", &unit, &capture)),
        Finished::Expired => Err(budget::timed_out(id, "stopped", budget)),
    }
}

fn show_properties(unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
    let joined = props.join(",");
    let output = run_systemctl(&["show", "--property", &joined, unit])?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "systemctl show {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}

/// `UnitFileState` is the same install-state systemd derives for `systemctl is-enabled` — one query
/// covers state, pid, and boot-enablement together.
pub(super) fn status_from_unit(unit: &str) -> Result<Status> {
    let props = show_properties(unit, &["ActiveState", "MainPID", "UnitFileState"])?;
    let state = match props.get("ActiveState").map(String::as_str) {
        Some("active") => State::Running,
        Some("inactive") => State::Stopped,
        Some("failed") => State::Failed,
        // `activating`/`deactivating` land here, along with anything else
        // systemd reports. One of three places a `State::Starting` would be
        // produced if goetia grows one (routed post-0.1.0) — the other two
        // are `state::classify` in `src/backend/launchd/state.rs` and
        // `map_state` in `src/backend/scm/manager.rs`.
        _ => State::Unknown,
    };
    let pid = props
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0);
    let enabled = props.get("UnitFileState").is_some_and(|s| s == "enabled");
    Ok(Status { state, pid, enabled })
}

#[cfg(test)]
#[path = "systemctl_tests.rs"]
mod systemctl_tests;
