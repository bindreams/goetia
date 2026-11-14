//! RAII cleanup for the real system services the probes register.
//!
//! A guard exists before the write it cleans up. Each guard is declared where the thing it
//! removes is created, in creation order: locals drop in reverse, so a thing is removed only
//! after everything that uses it.
//!
//! On macOS it boots the job out and removes only
//! `/Library/LaunchDaemons/<id>.plist`, not goetia's staging directory. A
//! goetia-installed launchd id is guarded with the launchd suite's `Guard`.
//!
//! `Drop` cannot return a `Result`, so every failure other than "not found" is
//! logged to stderr naming the id and the error. A cleanup failure that
//! reports nothing surfaces later only as a mysterious straggler in
//! `systemctl` / `launchctl` / `services.msc`, long after the run that left it.

use super::cleanup::remove_path_retrying;
use super::{LAUNCHD_DAEMON_DIR, SYSTEMD_UNIT_DIR, cmd};

pub struct ServiceGuard {
    id: String,
}

impl ServiceGuard {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        for problem in cleanup(&self.id) {
            eprintln!("ServiceGuard[{}]: cleanup failed: {problem}", self.id);
        }
    }
}

/// Stops the timer and the unit, then removes both fragments, their drop-in directories (a leftover
/// one would poison the next install of the same id) and the `*.target.wants` symlinks a test that
/// enabled either leaves. The timer goes first, so nothing starts the unit again.
#[cfg(target_os = "linux")]
fn cleanup(id: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let dir = std::path::PathBuf::from(SYSTEMD_UNIT_DIR);

    // Exit 5 is "unit not loaded", the one benign outcome.
    for (suffix, wants_dir) in [("timer", "timers.target.wants"), ("service", "multi-user.target.wants")] {
        let unit = format!("{id}.{suffix}");
        problems.extend(cmd::run_cleanup("systemctl", &[&["stop", &unit]], &[5]));
        problems.extend(remove_path_retrying(&dir.join(&unit), false));
        problems.extend(remove_path_retrying(&dir.join(format!("{unit}.d")), true));
        problems.extend(remove_path_retrying(&dir.join(wants_dir).join(&unit), false));
    }
    problems.extend(cmd::run_cleanup("systemctl", &[&["daemon-reload"]], &[]));
    problems
}

#[cfg(target_os = "macos")]
fn cleanup(id: &str) -> Vec<String> {
    let mut problems = Vec::new();

    // 3 (no such process) and 113 (could not find specified service) both mean
    // the job was never bootstrapped, which is not a failure to clean up.
    let target = format!("system/{id}");
    problems.extend(cmd::run_cleanup("launchctl", &[&["bootout", &target]], &[3, 113]));
    let path = std::path::PathBuf::from(LAUNCHD_DAEMON_DIR).join(format!("{id}.plist"));
    problems.extend(remove_path_retrying(&path, false));

    // launchd exposes no way to delete an override-database entry, so a label
    // the disabled-key probe called `launchctl enable` on keeps one. It refers
    // to a job that no longer exists and the ids are random, so it is inert.
    problems
}

#[cfg(windows)]
fn cleanup(id: &str) -> Vec<String> {
    let mut problems = Vec::new();

    // `sc.exe` exits with the Win32 error: 1060 is ERROR_SERVICE_DOES_NOT_EXIST,
    // 1062 is ERROR_SERVICE_NOT_ACTIVE and 1072 is ERROR_SERVICE_MARKED_FOR_DELETE.
    problems.extend(cmd::run_cleanup("sc.exe", &[&["stop", id]], &[1060, 1062, 1072]));
    // 1072: `uninstall` may return while the record is only marked for deletion;
    // SCM drops it once its last handle closes. Nothing is left to delete.
    problems.extend(cmd::run_cleanup("sc.exe", &[&["delete", id]], &[1060, 1072]));
    problems
}
