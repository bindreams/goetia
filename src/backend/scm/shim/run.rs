//! The Windows host goetia's daemons run under (`goetia-shim`), in one of two modes.
//!
//! **Service mode** ([`service`]): SCM has no working-directory, environment, or stdout-capture
//! field of its own (`backend::scm::generate`'s mapping table calls these cells "shim
//! spawns child"/"shim sets it"/"shim writes"), so for `protocol: plain`
//! `ScmRegistration::executable` always names the shim binary — never the
//! daemon's own command. It takes the service name SCM hands `ServiceMain`,
//! reads that daemon's real `DaemonSpec` back out of the same
//! `Services\<name>\Parameters` metadata blob the SCM backend wrote it into, and
//! does everything a real service host needs: spawn the daemon once, capture
//! its output, report its exit as the service's exit, and kill its whole
//! process tree on `SERVICE_STOP`.
//!
//! **Task mode** ([`task`]): Task Scheduler runs the shim as a task's action, and its arguments end
//! in the task-mode argument and the task's id. It reads its own task, runs the program the same way,
//! and records how the run ended.

use std::ffi::OsString;

use super::marker;

mod exit;
mod logging;
mod os;
mod service;
mod stop_bus;
mod task;
#[cfg(test)]
mod test_support;

/// Called from `main()`: runs as a task when the arguments say so ([`task::requested`]), as a service
/// otherwise. Never returns.
pub(crate) fn run() -> ! {
    // Every host reaches here, and this read is what keeps its version marker in the linked binary.
    std::hint::black_box(&marker::MARKER);
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match task::requested(&args) {
        Some(id) => task::run(id),
        None => service::run(),
    }
}
