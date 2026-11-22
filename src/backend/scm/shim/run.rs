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
//! its output, report its exit as the service's exit, and on `SERVICE_STOP` stop it as its
//! `stop-action` says: ask it to exit with a console event, wait for its whole process tree for the
//! `timeout`, then kill what is left (`stop_bus::StopPlan`). The event is sent through a copy of the
//! host program (cosca's proxy), so a host that hosts the shim itself calls `shim::init_host` first in
//! its `main`; one that did not is refused at start.
//!
//! **Task mode** ([`task`]): Task Scheduler runs the shim as a task's action, and its arguments end
//! in the task-mode argument, the task's id and the registration's token. It reads its own task, and
//! starts no program (a shim failure) when the task's registration is not the one it was started
//! under. Otherwise it runs the program the same way and records how the run ended. A task's stop is the
//! console close event, which reaches the program on its own, and a console program's own handler runs before the
//! shim hears it (measured on Windows Server 2025, build 26100), so the record is the program's exit whenever the
//! shim saw it exit. For a program still alive when the shim hears the stop it records the stop first, then waits
//! for the program to exit, with no cap of its own, and tears down what is left (`stop_bus::StopPlan::WaitOnly`).

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
        Some((id, token)) => task::run(&id, token),
        None => service::run(),
    }
}
