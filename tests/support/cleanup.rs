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

/// A directory tree removed on drop with [`remove_path_retrying`], then let go with a warning
/// naming it. Never panics, so a drop that runs while the test unwinds still removes the tree.
pub struct DirGuard(std::path::PathBuf);

impl DirGuard {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        if let Some(problem) = remove_path_retrying(&self.0, true) {
            eprintln!("warning: left behind: {problem}");
        }
    }
}
