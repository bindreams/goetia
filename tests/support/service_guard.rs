//! RAII cleanup for the real system services the probes register.
//!
//! A guard exists before the write it cleans up.
//!
//! On macOS it boots the job out and removes only
//! `/Library/LaunchDaemons/<id>.plist`, not goetia's staging directory. A
//! goetia-installed launchd id is guarded with the launchd suite's `Guard`.
//!
//! `Drop` cannot return a `Result`, so every failure other than "not found" is
//! logged to stderr naming the id and the error. A cleanup failure that
//! reports nothing surfaces later only as a mysterious straggler in
//! `systemctl` / `launchctl` / `services.msc`, long after the run that left it.

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

/// Removes a file, or a directory tree when `dir`; "not found" is success, any other failure is
/// returned as a message for the caller to log.
pub fn remove_path(path: &std::path::Path, dir: bool) -> Option<String> {
    let result = if dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("remove {}: {e}", path.display())),
    }
}

/// Removes the fragment, its drop-in directory (a leftover one would poison the
/// next install of the same id) and the `multi-user.target.wants` symlink a
/// test that enabled the unit leaves.
#[cfg(target_os = "linux")]
fn cleanup(id: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let unit = format!("{id}.service");
    let path = std::path::PathBuf::from(SYSTEMD_UNIT_DIR).join(&unit);

    // Exit 5 is "unit not loaded", the one benign outcome.
    let stop = cmd::run("systemctl", &["stop", &unit]);
    if !stop.ok() && stop.code != Some(5) {
        problems.push(stop.to_string());
    }
    problems.extend(remove_path(&path, false));
    problems.extend(remove_path(&path.with_file_name(format!("{unit}.d")), true));
    let wants = std::path::PathBuf::from(SYSTEMD_UNIT_DIR)
        .join("multi-user.target.wants")
        .join(&unit);
    problems.extend(remove_path(&wants, false));
    let reload = cmd::run("systemctl", &["daemon-reload"]);
    if !reload.ok() {
        problems.push(reload.to_string());
    }
    problems
}

#[cfg(target_os = "macos")]
fn cleanup(id: &str) -> Vec<String> {
    let mut problems = Vec::new();

    // 3 (no such process) and 113 (could not find specified service) both mean
    // the job was never bootstrapped, which is not a failure to clean up.
    let bootout = cmd::run("launchctl", &["bootout", &format!("system/{id}")]);
    if !bootout.ok() && !matches!(bootout.code, Some(3 | 113)) {
        problems.push(bootout.to_string());
    }
    let path = std::path::PathBuf::from(LAUNCHD_DAEMON_DIR).join(format!("{id}.plist"));
    problems.extend(remove_path(&path, false));

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
    let stop = cmd::run("sc.exe", &["stop", id]);
    if !stop.ok() && !matches!(stop.code, Some(1060 | 1062 | 1072)) {
        problems.push(stop.to_string());
    }
    // 1072: `uninstall` may return while the record is only marked for deletion;
    // SCM drops it once its last handle closes. Nothing is left to delete.
    let delete = cmd::run("sc.exe", &["delete", id]);
    if !delete.ok() && !matches!(delete.code, Some(1060 | 1072)) {
        problems.push(delete.to_string());
    }
    problems
}
