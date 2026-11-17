//! The one boundary between the shim and the operating system: the supervisor and the log call
//! [`Os`], [`Spawned`] and [`Channels`] instead of `cosca`, `std::thread` and the file system.
//! [`Real`] implements them; tests use `fake::Fake`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitStatus;
use std::thread::JoinHandle;

use cosca::Containment;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;

use crate::backend::unreaped::release_unreaped;
use crate::error::os_error_in;

mod eventlog;

/// What the supervisor asks the OS for.
pub(crate) trait Os: Send + Sync {
    /// Spawn the daemon `cmd` describes. A refusal that says the program may have started
    /// ([`cosca::error::Error::MayHaveStarted`]) may leave it running: read [`fate`](cosca::error::Error::fate).
    fn spawn(&self, cmd: cosca::Command) -> Result<Box<dyn Spawned>, cosca::error::Error>;

    /// Run `f` on a new thread named `name`; the OS may refuse.
    fn thread(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>;

    /// Open the file at `path` for appending, as `kind` says. It is the daemon's log: the daemon's output is
    /// redirected into it.
    fn open_log(&self, path: &Path, kind: LogKind) -> io::Result<File>;
}

/// Which log a file is, which decides how the shim opens it. The default log,
/// `%ProgramData%\Goetia\logs\<id>.log`, is an elevated install's: it made the folder and the file, and
/// judged them, so the shim makes nothing there and opens the file for appending alone
/// ([`LogKind::Default`]). A `logs:` path of the daemon's own is the user's to name, so its folders and the
/// file are made ([`LogKind::Own`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogKind {
    Own,
    Default,
}

/// A spawned daemon. Dropping it kills its tree, as `cosca::Child` does; a kill the OS refuses is
/// looked at once and never waited on.
pub(crate) trait Spawned: Send + Sync {
    /// How the daemon's tree is contained.
    fn containment(&self) -> Containment;

    /// Block until the daemon exits. May be called again once it has, with the same status.
    fn wait(&self) -> Result<ExitStatus, cosca::error::Error>;

    /// Kill the daemon alone, not its descendants.
    fn kill(&self) -> Result<(), cosca::error::Error>;

    /// Send the kill to the daemon's whole tree. `Ok` means every member was reached, not that any has
    /// exited.
    fn send_kill_tree(&self) -> Result<(), cosca::error::Error>;
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
    /// Append `line` and a newline to the file at `path`, opening it as `kind` says. The line is written in one `write`, so output another handle appends to the same
    /// file cannot split it.
    fn append(&self, path: &Path, line: &str, kind: LogKind) -> io::Result<()>;

    /// Report `line` to the Windows Event Log as `severity`. Best-effort: this is itself a
    /// failure-reporting path.
    fn event_log(&self, severity: Severity, line: &str);
}

/// The production implementation of all three traits.
pub(crate) struct Real;

impl Os for Real {
    fn spawn(&self, mut cmd: cosca::Command) -> Result<Box<dyn Spawned>, cosca::error::Error> {
        Ok(Box::new(cmd.spawn().map_err(release_unreaped)?))
    }

    fn thread(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>> {
        std::thread::Builder::new().name(name.to_string()).spawn(f)
    }

    fn open_log(&self, path: &Path, kind: LogKind) -> io::Result<File> {
        open_append(path, kind)
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

    fn send_kill_tree(&self) -> Result<(), cosca::error::Error> {
        cosca::Child::send_kill_tree(self)
    }
}

impl Channels for Real {
    fn append(&self, path: &Path, line: &str, kind: LogKind) -> io::Result<()> {
        append_line(&mut open_append(path, kind)?, line)
    }

    fn event_log(&self, severity: Severity, line: &str) {
        eventlog::report(severity, line);
    }
}

/// The OS error code SCM is told for a refused spawn: the first one on the error's `source()`
/// chain ([`os_error_in`]), and `ERROR_FILE_NOT_FOUND` for a `NotFound` with no code anywhere. The
/// shim passes only argv, so
/// `std` searches `PATH` for a bare program name before `CreateProcessW` is called, and its miss is
/// `NotFound` with no code (a missing absolute path fails in `CreateProcessW`, with the code).
/// Nothing else on the spawn path makes a code-less `NotFound`.
pub(crate) fn spawn_error_code(e: &cosca::error::Error) -> Option<i32> {
    os_error_in(e).or_else(|| match e {
        cosca::error::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => Some(ERROR_FILE_NOT_FOUND as i32),
        _ => None,
    })
}

/// Open `path` for append as `kind` says.
fn open_append(path: &Path, kind: LogKind) -> io::Result<File> {
    if let (LogKind::Own, Some(parent)) = (kind, path.parent()) {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// One `write` call, so another handle's appends cannot split the line.
fn append_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    out.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
#[path = "os_tests.rs"]
mod os_tests;
