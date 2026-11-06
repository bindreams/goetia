//! What the shim asks the operating system for, as three traits the supervisor and the log call
//! instead of `cosca`, `std::thread` and the file system: [`Os`] spawns the daemon and makes
//! threads, [`Spawned`] is the daemon once spawned, [`Channels`] is where the shim's own failure
//! lines go. [`Real`] implements all three; a test implements them with `fake::Fake`.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::process::ExitStatus;
use std::thread::JoinHandle;

use cosca::Containment;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;

mod eventlog;

/// What the supervisor asks the OS for.
pub(crate) trait Os: Send + Sync {
    /// Spawn the daemon `cmd` describes. A refusal leaves nothing running.
    fn spawn(&self, cmd: cosca::Command) -> Result<Box<dyn Spawned>, cosca::error::Error>;

    /// Run `f` on a new thread named `name`; the OS may refuse.
    fn thread(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>;
}

/// A spawned daemon. Dropping it kills its tree and waits for it, as `cosca::Child` does; see
/// [`Spawned::detach`] to avoid that.
pub(crate) trait Spawned: Send + Sync {
    /// How the daemon's tree is contained.
    fn containment(&self) -> Containment;

    /// Block until the daemon exits. May be called again once it has, with the same status.
    fn wait(&self) -> Result<ExitStatus, cosca::error::Error>;

    /// Kill the daemon alone, not its descendants.
    fn kill(&self) -> Result<(), cosca::error::Error>;

    /// Kill the daemon's whole tree.
    fn kill_tree(&self) -> Result<(), cosca::error::Error>;

    /// Let the daemon go: dropping it no longer kills it.
    fn detach(self: Box<Self>);
}

/// How a line is reported to the Event Log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Error,
    /// The daemon runs, but something about its registration needs the administrator.
    Warning,
}

/// Where the shim's failure and warning lines go.
pub(crate) trait Channels: Send + Sync {
    /// Append `line` and a newline to the file at `path`, creating missing parent directories. The
    /// line is written in one `write`, so output another handle appends to the same file cannot
    /// split it.
    fn append(&self, path: &Path, line: &str) -> io::Result<()>;

    /// Open the file at `path` for appending, creating missing parent directories. It is the
    /// daemon's log: the daemon's output is redirected into it.
    fn open_log(&self, path: &Path) -> io::Result<File>;

    /// Report `line` to the Windows Event Log as `severity`. Best-effort: this is itself a
    /// failure-reporting path, and it has no failure to report.
    fn event_log(&self, severity: Severity, line: &str);
}

/// The production implementation of all three traits.
pub(crate) struct Real;

impl Os for Real {
    fn spawn(&self, mut cmd: cosca::Command) -> Result<Box<dyn Spawned>, cosca::error::Error> {
        Ok(Box::new(cmd.spawn()?))
    }

    fn thread(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>> {
        std::thread::Builder::new().name(name.to_string()).spawn(f)
    }
}

impl Spawned for cosca::Child {
    fn containment(&self) -> Containment {
        cosca::Child::containment(self)
    }

    fn wait(&self) -> Result<ExitStatus, cosca::error::Error> {
        cosca::Child::wait(self)
    }

    fn kill(&self) -> Result<(), cosca::error::Error> {
        cosca::Child::kill(self)
    }

    fn kill_tree(&self) -> Result<(), cosca::error::Error> {
        cosca::Child::kill_tree(self)
    }

    fn detach(self: Box<Self>) {
        cosca::Child::detach(*self);
    }
}

impl Channels for Real {
    fn append(&self, path: &Path, line: &str) -> io::Result<()> {
        append_line(&mut open_append(path)?, line)
    }

    fn open_log(&self, path: &Path) -> io::Result<File> {
        open_append(path)
    }

    fn event_log(&self, severity: Severity, line: &str) {
        eventlog::report(severity, line);
    }
}

/// The OS error code behind a `cosca` failure, if it has one.
pub(crate) fn os_error_of(e: &cosca::error::Error) -> Option<i32> {
    match e {
        cosca::error::Error::Io(e) => e.raw_os_error(),
        cosca::error::Error::Unassessable { source, .. } => source.as_ref().and_then(std::io::Error::raw_os_error),
        _ => None,
    }
}

/// The OS error code SCM is told for a refused spawn: [`os_error_of`], and `ERROR_FILE_NOT_FOUND`
/// for a `NotFound` without a code. The shim passes only argv, so `std` searches `PATH` for a bare
/// program name before `CreateProcessW` is called, and its miss is `NotFound` with no code (a
/// missing absolute path fails in `CreateProcessW`, with the code). Nothing else on the spawn path
/// makes a code-less `NotFound`.
pub(crate) fn spawn_error_code(e: &cosca::error::Error) -> Option<i32> {
    match e {
        cosca::error::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound && io.raw_os_error().is_none() => {
            Some(ERROR_FILE_NOT_FOUND as i32)
        }
        _ => os_error_of(e),
    }
}

/// `s` as the null-terminated UTF-16 string the Win32 wide-character APIs take.
pub(crate) fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Open `path` for append, creating parent directories first.
fn open_append(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Writes `line` and a newline in one `write`, so output another handle appends in between cannot
/// split the line.
fn append_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    out.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
#[path = "os_tests.rs"]
mod os_tests;
