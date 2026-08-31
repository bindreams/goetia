//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back.

use std::collections::BTreeMap;
use std::process::Command;

use crate::error::{Error, Result};
use crate::manager::{State, Status};

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

/// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
/// needed. Idempotent: starting an already-active unit is a no-op that still exits 0.
pub(super) fn start_impl(unit: &str) -> Result<()> {
    let output = run_systemctl(&["start", unit])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl start {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// Idempotent per `ServiceManager::stop`'s doc comment. Exit code 5 ("unit not loaded") means there
/// was nothing to stop — the same convention `tests/support/service_guard.rs` already uses for
/// cleanup — which is success here, not a failure to stop something that was never running.
pub(super) fn stop_impl(unit: &str) -> Result<()> {
    let output = run_systemctl(&["stop", unit])?;
    if output.status.success() || output.status.code() == Some(5) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl stop {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
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
        _ => State::Unknown,
    };
    let pid = props
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0);
    let enabled = props.get("UnitFileState").is_some_and(|s| s == "enabled");
    Ok(Status { state, pid, enabled })
}
