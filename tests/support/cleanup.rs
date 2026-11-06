//! Removal of what a test created, retried for the transient failures cleanup meets (an antivirus
//! holding a file, a process still exiting). Self-contained, so the library's unit tests load it as
//! `backend::test_cleanup`.

use std::time::{Duration, Instant};

/// The problem a removal of `path` that returned `result` leaves, as a message for the caller to
/// log; "not found" is success.
pub fn removal_problem(path: &std::path::Path, result: std::io::Result<()>) -> Option<String> {
    match result {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("remove {}: {e}", path.display())),
    }
}

/// Removes a file, or a directory tree when `dir`; see [`removal_problem`].
pub fn remove_path(path: &std::path::Path, dir: bool) -> Option<String> {
    let result = if dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    removal_problem(path, result)
}

/// How long a cleanup waits on a transient condition of unknown duration (an antivirus holding a
/// file, a process still exiting) before it lets go with a warning.
pub const CLEANUP_BOUND: Duration = Duration::from_secs(5);

/// `attempt`, retried until it succeeds or [`CLEANUP_BOUND`] passes; then the last problem.
pub fn retrying(mut attempt: impl FnMut() -> Option<String>) -> Option<String> {
    let start = Instant::now();
    loop {
        match attempt() {
            Some(_) if start.elapsed() < CLEANUP_BOUND => std::thread::sleep(Duration::from_millis(100)),
            problem => return problem,
        }
    }
}

/// [`remove_path`], retried until it succeeds or [`CLEANUP_BOUND`] passes; then the last problem.
pub fn remove_path_retrying(path: &std::path::Path, dir: bool) -> Option<String> {
    retrying(|| remove_path(path, dir))
}

/// Prints the shared warning for the `problem` a cleanup gave up on, if any.
pub fn warn_left_behind(problem: Option<String>) {
    if let Some(problem) = problem {
        eprintln!("warning: left behind: {problem}");
    }
}

/// [`remove_path_retrying`], then a warning naming what it could not remove. Never panics, so a
/// cleanup that runs while the test unwinds still goes on. Every cleanup that has no use for the
/// problem itself goes through this one function.
pub fn remove_or_warn(path: &std::path::Path, dir: bool) {
    warn_left_behind(remove_path_retrying(path, dir));
}

/// A directory tree removed on drop with [`remove_or_warn`]. Never panics, so a drop that runs
/// while the test unwinds still removes the tree.
#[allow(dead_code, reason = "the library's launchd tests build none")]
pub struct DirGuard(std::path::PathBuf);

#[allow(
    dead_code,
    reason = "the library's launchd tests build no DirGuard, and its systemd tests never read the path"
)]
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
        remove_or_warn(&self.0, true);
    }
}

/// A file removed on drop like [`DirGuard`]'s tree. A symlink is unlinked, not followed.
pub struct FileGuard(std::path::PathBuf);

impl FileGuard {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self(path)
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        remove_or_warn(&self.0, false);
    }
}
