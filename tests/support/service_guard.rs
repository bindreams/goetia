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

use std::time::{Duration, Instant};

use super::{LAUNCHD_DAEMON_DIR, SYSTEMD_UNIT_DIR, cmd};

pub struct ServiceGuard {
    id: String,
    /// A file the service's process may write and nothing else removes; see [`Self::with_leftover`].
    #[cfg(windows)]
    leftover: Option<std::path::PathBuf>,
}

impl ServiceGuard {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            #[cfg(windows)]
            leftover: None,
        }
    }

    /// A guard that, after its stop and delete, also removes `leftover`: a file the service's
    /// process may create, such as the shim's failure log. It waits for the service to be `STOPPED`
    /// first (a gone service counts), since a process that has reported `STOPPED` writes nothing
    /// more; if that takes longer than [`CLEANUP_BOUND`] it warns, naming the file, and leaves it.
    #[cfg(windows)]
    pub fn with_leftover(id: impl Into<String>, leftover: std::path::PathBuf) -> Self {
        Self {
            id: id.into(),
            leftover: Some(leftover),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let problems = cleanup(&self.id);
        // After `cleanup`: it is the stop the leftover's wait depends on.
        #[cfg(windows)]
        let leftover = self.leftover.as_ref().and_then(|path| remove_leftover(&self.id, path));
        #[cfg(not(windows))]
        let leftover: Option<String> = None;
        for problem in problems.into_iter().chain(leftover) {
            eprintln!("ServiceGuard[{}]: cleanup failed: {problem}", self.id);
        }
    }
}

/// How long a cleanup waits on a transient condition of unknown duration (an antivirus holding a
/// file, a process still exiting) before it lets go with a warning. The owner's bound.
pub const CLEANUP_BOUND: Duration = Duration::from_secs(5);

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

/// [`remove_path`], retried until it succeeds or [`CLEANUP_BOUND`] passes; then the last problem.
pub fn remove_path_retrying(path: &std::path::Path, dir: bool) -> Option<String> {
    let start = Instant::now();
    loop {
        match remove_path(path, dir) {
            Some(_) if start.elapsed() < CLEANUP_BOUND => std::thread::sleep(Duration::from_millis(100)),
            problem => return problem,
        }
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

/// Removes `leftover` once `id` is `STOPPED` or gone; otherwise the reason it was left.
///
/// A stop is waited for by notification, within [`CLEANUP_BOUND`]. SCM can refuse to watch a service
/// that `sc delete` has marked for deletion (`ERROR_SERVICE_MARKED_FOR_DELETE`); the state is then
/// read once, as the production wait does, and a service not yet `STOPPED` keeps the file.
#[cfg(windows)]
fn remove_leftover(id: &str, leftover: &std::path::Path) -> Option<String> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE};
    use windows_sys::Win32::System::Services::SERVICE_NOTIFY_STOPPED;

    use super::service_notify::arm_and_wait;

    let left = |why: String| Some(format!("left {}: {why}", leftover.display()));
    let is = |e: &std::io::Error, code: u32| e.raw_os_error() == Some(code as i32);
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        Ok(manager) => manager,
        Err(e) => return left(format!("connect to the service manager: {e}")),
    };
    let service = match manager.open_service(id, ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(e)) if is(&e, ERROR_SERVICE_DOES_NOT_EXIST) => {
            return remove_path_retrying(leftover, false);
        }
        Err(e) => return left(format!("open the service: {e}")),
    };
    match arm_and_wait(&service, SERVICE_NOTIFY_STOPPED, Some(Instant::now() + CLEANUP_BOUND)) {
        Ok(wait) => {
            if wait().is_none() {
                return left(format!("the service was not stopped after {CLEANUP_BOUND:?}"));
            }
        }
        Err(e) if is(&e, ERROR_SERVICE_DOES_NOT_EXIST) => {}
        Err(e) if is(&e, ERROR_SERVICE_MARKED_FOR_DELETE) => match service.query_status() {
            Ok(status) if status.current_state == ServiceState::Stopped => {}
            Ok(status) => {
                return left(format!(
                    "the service is {:?}, marked for deletion",
                    status.current_state
                ));
            }
            Err(e) => return left(format!("query the service: {e}")),
        },
        Err(e) => return left(format!("watch the service stop: {e}")),
    }
    remove_path_retrying(leftover, false)
}
