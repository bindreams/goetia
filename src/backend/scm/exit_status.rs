//! What `goetia-shim` tells SCM when its daemon ends, and the process exit code that goes with it.
//!
//! Two disjoint spaces. A daemon's exit code is a service-specific code: daemons exit with any
//! `u32` (NTSTATUS values included), so no part of that space can be set aside for the shim.
//! Everything the shim itself fails at is a Win32 code, which cannot collide with it. Win32 codes
//! are plain constants here so the shim, the manager and the tests share one definition; nothing in
//! this module needs Windows.
//!
//! Under the failure-actions flag SCM restarts after any `STOPPED` report whose Win32 code is
//! non-zero, a service-specific code included (it is carried as [`ERROR_SERVICE_SPECIFIC_ERROR`]),
//! and it does so for a stop it commanded too. A report whose Win32 code is 0 is never restarted,
//! whatever its service-specific code: SCM stores that pair as reported, which is how an incomplete
//! stop leaves a record without a restart.

/// `argv[1]` (the service id) is missing.
pub const ERROR_BAD_ARGUMENTS: u32 = 160;
/// The metadata blob is absent or cannot be decoded.
pub const ERROR_INVALID_DATA: u32 = 13;
/// A shim failure that carries no usable OS error.
pub const ERROR_GEN_FAILURE: u32 = 31;
/// A commanded stop that could not kill the daemon.
pub const ERROR_CAN_NOT_COMPLETE: u32 = 1003;
/// The control handler or the dispatcher could not be connected, so nothing is reportable.
pub const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: u32 = 1063;
/// The Win32 code SCM records alongside any service-specific code.
pub const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

/// The Win32 code an incomplete stop reports. Zero, not [`ERROR_CAN_NOT_COMPLETE`]: any non-zero
/// code queues a restart, which would start a second daemon beside the one that would not die. The
/// failure travels in the service-specific code instead.
const INCOMPLETE_STOP_WIN32: u32 = 0;

/// How a `goetia-shim` run ended, as SCM and the process exit code express it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reported {
    /// The daemon exited 0, or a commanded stop killed it.
    Clean,
    /// The daemon exited with this non-zero code, as the bits of a `u32`.
    DaemonExit(u32),
    /// The shim failed; holds a Win32 code that is neither 0 nor [`ERROR_SERVICE_SPECIFIC_ERROR`].
    ShimFailure(u32),
    /// A commanded stop that could not kill the daemon, which may still be running.
    IncompleteStop,
}

impl Reported {
    /// The daemon's exit code; 0 is [`Reported::Clean`].
    pub fn from_daemon_code(code: i32) -> Reported {
        match code {
            0 => Reported::Clean,
            code => Reported::DaemonExit(code as u32),
        }
    }

    /// A shim failure carrying `os_error`. `None`, 0 and [`ERROR_SERVICE_SPECIFIC_ERROR`] become
    /// [`ERROR_GEN_FAILURE`]: 0 would read as a clean exit and 1066 as a daemon's code.
    pub fn shim_failure(os_error: Option<i32>) -> Reported {
        match os_error.map(|code| code as u32) {
            Some(code) if code != 0 && code != ERROR_SERVICE_SPECIFIC_ERROR => Reported::ShimFailure(code),
            _ => Reported::ShimFailure(ERROR_GEN_FAILURE),
        }
    }

    pub fn incomplete_stop() -> Reported {
        Reported::IncompleteStop
    }

    /// The process exit code. SCM decides from the reported status, never from this.
    pub fn process_exit_code(self) -> i32 {
        match self {
            Reported::Clean => 0,
            Reported::DaemonExit(code) | Reported::ShimFailure(code) => code as i32,
            Reported::IncompleteStop => ERROR_CAN_NOT_COMPLETE as i32,
        }
    }

    /// `(win32, service_specific)` for the `STOPPED` report. A daemon failure is
    /// `(ERROR_SERVICE_SPECIFIC_ERROR, code)`, which is how SCM records a service-specific code;
    /// an incomplete stop is `(0, ERROR_CAN_NOT_COMPLETE)`. The pair needs a raw `SetServiceStatus`:
    /// `windows-service`'s `ServiceExitCode` cannot express a service-specific code beside Win32 0.
    pub fn scm_codes(self) -> (u32, u32) {
        match self {
            Reported::Clean => (0, 0),
            Reported::DaemonExit(code) => (ERROR_SERVICE_SPECIFIC_ERROR, code),
            Reported::ShimFailure(code) => (code, 0),
            Reported::IncompleteStop => (INCOMPLETE_STOP_WIN32, ERROR_CAN_NOT_COMPLETE),
        }
    }
}

/// The exit codes SCM holds for a service that reached `STOPPED`: what the shim reported, or
/// `ERROR_PROCESS_ABORTED` (1067) when its process ended without reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedExit {
    pub win32: u32,
    /// Meaningful when `win32` is [`ERROR_SERVICE_SPECIFIC_ERROR`], and for an incomplete stop.
    pub specific: u32,
}

impl RecordedExit {
    /// Whether the shim launched the daemon and the daemon then ended: a daemon exit
    /// ([`Reported::Clean`], [`Reported::DaemonExit`], [`Reported::IncompleteStop`]). Any other
    /// Win32 code is a shim failure before or around the launch.
    pub fn daemon_ran(self) -> bool {
        self.win32 == 0 || self.win32 == ERROR_SERVICE_SPECIFIC_ERROR
    }

    /// Whether this is [`Reported::IncompleteStop`]'s record: the shim could not kill a daemon it
    /// launched, which may still be running. A daemon that itself exits 1003 is
    /// [`ERROR_SERVICE_SPECIFIC_ERROR`] with specific 1003, not this.
    pub fn is_incomplete_stop(self) -> bool {
        (self.win32, self.specific) == Reported::IncompleteStop.scm_codes()
    }
}

impl std::fmt::Display for RecordedExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.win32 == ERROR_SERVICE_SPECIFIC_ERROR {
            write!(f, "service-specific exit code {}", self.specific)
        } else {
            write!(f, "exit code {}", self.win32)
        }
    }
}

#[cfg(test)]
#[path = "exit_status_tests.rs"]
mod exit_status_tests;
