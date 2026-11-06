//! Removal of what a test created, retried for the transient failures cleanup meets (an antivirus
//! holding a file, a process still exiting). Self-contained, so the shim's unit tests include it
//! through `#[path]`.

use std::time::{Duration, Instant};

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

/// How long a cleanup waits on a transient condition of unknown duration (an antivirus holding a
/// file, a process still exiting) before it lets go with a warning.
pub const CLEANUP_BOUND: Duration = Duration::from_secs(5);

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
