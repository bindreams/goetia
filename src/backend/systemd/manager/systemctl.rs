//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back.

use std::collections::BTreeMap;
use std::process::Command;

use crate::backend::bounded::{self, Capture, Finished};
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

/// The argv for one `systemctl <verb> <unit>` under `budget`.
///
/// Its own function so that `--no-block` is observable without a timing bet. No elevated test can
/// see the flag — `start` returns either way — so asserting the argv is the only way this choice
/// can fail a test at all; `systemctl_tests.rs` does exactly that.
fn verb_args<'a>(verb: &'a str, unit: &'a str, budget: Budget) -> Vec<&'a str> {
    if budget.waits() {
        vec![verb, unit]
    } else {
        vec![verb, "--no-block", unit]
    }
}

/// One `systemctl <verb> <unit>` under `budget`, as three deliberately different paths.
///
/// A budget that does not wait becomes `systemctl <verb> --no-block`, the exact native expression
/// of "issue the request and return": systemd enqueues the job and `systemctl` exits without
/// waiting for it to complete. `Budget::Unbounded` is today's plain blocking call. Both keep
/// `Command::output()`, because neither has anything to bound — cosca enters this module only where
/// a bound is actually required, which is the third path and only the third.
///
/// That third path waits on `deadline`, which the caller derived at verb entry: `--timeout` bounds
/// the verb as a whole — `require_installed`'s scan and the spawn included — as launchd's
/// `verb_deadline` does, not only the `systemctl` call. `budget` only picks the path and the
/// wording.
fn run_verb(verb: &str, unit: &str, budget: Budget, deadline: Deadline) -> Result<Finished> {
    run_verb_via("systemctl", verb, unit, budget, deadline)
}

/// [`run_verb`] with the program named, so its bounded path is testable against a child that
/// cannot exit on its own.
fn run_verb_via(program: &str, verb: &str, unit: &str, budget: Budget, deadline: Deadline) -> Result<Finished> {
    let args = verb_args(verb, unit, budget);
    let failed = |e: String| Error::Other(format!("failed to run `{program} {verb} {unit}`: {e}"));
    if !budget.waits() || budget == Budget::Unbounded {
        let output = Command::new(program)
            .args(&args)
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

    let mut cmd = bounded::command(program, &args).map_err(|e| failed(e.to_string()))?;
    let child = cmd.spawn().map_err(|e| failed(e.to_string()))?;
    bounded::wait_bounded(child, deadline).map_err(|e| failed(e.to_string()))
}

/// The message for a `systemctl` invocation that exited non-zero, built from the whole [`Capture`]
/// rather than from its bytes alone.
///
/// `systemctl` writes nothing on success, so `complete == false` only ever bites here — on
/// the one path whose entire value is the diagnostic. A truncated read presented as the whole story
/// is how "goetia stopped reading" comes out reading as "systemd said nothing".
///
/// `systemctl_tests.rs` covers all four shapes a capture arrives in; nothing else can, since
/// reaching this through a real `systemctl` needs one that actually fails.
fn failed(verb: &str, unit: &str, capture: &Capture) -> Error {
    // Whichever stream carried the diagnostic. `systemctl` writes its
    // failures to stderr, but a failure that produced only stdout would
    // otherwise reach the empty-diagnostic guard below and report that the
    // command "wrote no diagnostic" — false, since systemd wrote one, and
    // wrong on the one path whose entire value is the diagnostic.
    let diagnostic = if capture.stderr.is_empty() {
        String::from_utf8_lossy(&capture.stdout)
    } else {
        String::from_utf8_lossy(&capture.stderr)
    };
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

/// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
/// needed. Idempotent: starting an already-active unit is a no-op that still exits 0.
///
/// Every generated unit carries `Type=exec` (see `generate::unit`'s doc comment for why), so a
/// bounded `systemctl start` confirms the `exec` itself succeeded — not merely that systemd forked
/// the process. A missing executable or missing user comes back as a failed start.
pub(super) fn start_impl(id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
    let unit = super::unit_name(id);
    match run_verb("start", &unit, budget, deadline)? {
        Finished::Exited { status, .. } if status.success() => Ok(()),
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
        Finished::Exited { status, .. } if status.success() || status.code() == Some(5) => Ok(()),
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
