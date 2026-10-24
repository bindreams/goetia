//! SCM wiring: dispatch, control handler, status reporting, and the one pass that spawns the
//! daemon `goetia.yaml` describes, waits for it and reports how it ended.
//!
//! The shim never restarts the daemon. It reports the daemon's exit as the service's exit and
//! exits; SCM's recovery actions (`backend::scm::generate`) start a new shim, which reads the
//! installed spec afresh.
//!
//! Everything that touches the child process tree — containment, waiting,
//! stdio capture — is `cosca`'s job, not this module's: `cosca::Command`'s
//! `.contain()` is a Windows Job Object with `KILL_ON_JOB_CLOSE`, and
//! `cosca::Child::wait()`/`wait_tree()` are real, event-driven kernel
//! waits. This module's own job is narrower: interrupting a blocking
//! `child.wait()` when SCM delivers `Stop` (see `stop_bus`, the one piece
//! `cosca` does not supply, since it has no notion of an external
//! cancellation source) and driving SCM's own status protocol.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use cosca::{Child, Fd, Stdio};
use goetia::backend::scm::exit_status::{
    ERROR_BAD_ARGUMENTS, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_INVALID_DATA, Reported,
};
use goetia::backend::scm::manager::read_spec_blob;
use goetia::spec::DaemonSpec;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::{define_windows_service, service_dispatcher};

use crate::logging;
use crate::stop_bus::{StopBus, WaitOutcome, Waiter};

// Entry ===============================================================================================================

/// Called from `main()`. Reads `argv[1]` (the service id), hands control to
/// SCM, and never returns normally: `run_service` (via `service_main`)
/// calls `std::process::exit` on every path, including every failure one,
/// with a code from `goetia::backend::scm::exit_status`.
pub fn run() -> ! {
    let args: Vec<String> = std::env::args().collect();
    let Some(id) = args.get(1) else {
        eprintln!("goetia-shim: usage: goetia-shim <service-id>");
        std::process::exit(ERROR_BAD_ARGUMENTS as i32);
    };
    if let Err(e) = service_dispatcher::start(id, ffi_service_main) {
        logging::log_failure(id, &format!("connect to the SCM dispatcher: {e}"));
        std::process::exit(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32);
    }
    // Unreachable in practice — every path inside `service_main` exits the
    // process directly, and SCM only lets `StartServiceCtrlDispatcherW`'s
    // wait return once the process is already being torn down — but exit
    // cleanly here too rather than falling off `main`.
    std::process::exit(Reported::Clean.process_exit_code());
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_scm_args: Vec<OsString>) {
    // The service id argv[1] carried — re-read directly rather than
    // threaded through `service_main`'s own SCM-supplied arguments (whose
    // shape SCM controls, not this shim): `run` already confirmed argv[1]
    // is present before ever dispatching, and process argv does not change.
    let id = std::env::args()
        .nth(1)
        .expect("argv[1] was validated present in run() before service_dispatcher::start was ever called");
    run_service(&id);
}

fn run_service(id: &str) {
    let stop_bus = Arc::new(StopBus::new());
    let status_handle = match register_control_handler(id, &stop_bus) {
        Ok(h) => h,
        Err(e) => {
            logging::log_failure(id, &format!("register SCM control handler: {e}"));
            std::process::exit(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32);
        }
    };

    let blob = match read_spec_blob(id) {
        Ok(Some(b)) => b,
        Ok(None) => {
            logging::log_failure(
                id,
                "no Goetia metadata found under Services\\<id>\\Parameters (Marker absent) — this service was \
                 not created by `goetia daemon install`",
            );
            finish(
                id,
                &status_handle,
                Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
            );
        }
        Err(e) => {
            logging::log_failure(id, &format!("decode metadata blob: {e}"));
            finish(
                id,
                &status_handle,
                Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
            );
        }
    };

    let reported = run_daemon(&blob.spec, &stop_bus, id, || report_running(id, &status_handle));
    finish(id, &status_handle, reported);
}

/// Tell SCM the service stopped, then end the process. SCM restarts the service or not from
/// the status reported here, never from the process exit code.
fn finish(id: &str, status_handle: &ServiceStatusHandle, reported: Reported) -> ! {
    report_stopped(id, status_handle, service_exit_code(reported));
    std::process::exit(reported.process_exit_code());
}

/// `reported` as the `ServiceExitCode` SCM records: a daemon's own failure is service-specific,
/// everything else is a Win32 code.
fn service_exit_code(reported: Reported) -> ServiceExitCode {
    match reported.scm_codes() {
        (_, specific) if specific != 0 => ServiceExitCode::ServiceSpecific(specific),
        (win32, _) => ServiceExitCode::Win32(win32),
    }
}

fn register_control_handler(id: &str, stop_bus: &Arc<StopBus>) -> windows_service::Result<ServiceStatusHandle> {
    let handler_bus = Arc::clone(stop_bus);
    let event_handler = move |control_event| match control_event {
        ServiceControl::Stop => {
            handler_bus.request_stop();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    service_control_handler::register(id, event_handler)
}

/// `set_service_status` is the only mechanism by which SCM learns this
/// service's state; a failure here is logged like every other Win32/cosca
/// failure in this file, not silently discarded — see `logging`'s module
/// doc comment for why both the fallback log and the Windows Event Log
/// need to carry it: SCM believing the service is still in whatever state
/// it last confirmed is exactly the kind of thing a human debugging a
/// stuck `stop`/`uninstall` needs to be able to find.
fn report_running(id: &str, handle: &ServiceStatusHandle) {
    if let Err(e) = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }) {
        logging::log_failure(id, &format!("report SERVICE_RUNNING to SCM: {e}"));
    }
}

fn report_stopped(id: &str, handle: &ServiceStatusHandle, exit_code: ServiceExitCode) {
    if let Err(e) = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }) {
        logging::log_failure(id, &format!("report SERVICE_STOPPED to SCM: {e}"));
    }
}

// One pass ============================================================================================================

/// Launch the daemon, report `RUNNING` through `on_running`, wait for the daemon to end or for a
/// stop, and say how it ended. Never respawns: SCM restarts the service from the status the caller
/// reports.
///
/// `on_running` is called exactly once, after the daemon is spawned and never when launching failed.
/// A daemon that cannot start is therefore `STOPPED` with a non-zero Win32 code without ever having
/// been `RUNNING`, which `start_via_notify` reads as a failed start under every restart policy.
fn run_daemon(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, id: &str, on_running: impl FnOnce()) -> Reported {
    let (child, waiter) = match launch(spec, stop_bus, id) {
        Ok(launched) => launched,
        Err(reported) => return reported,
    };
    on_running();

    // `wait_for_child_or_stop` also performs the kill-tree-and-confirm
    // teardown on a stop (see its own doc comment for why that has to
    // happen inside the call rather than out here) — by the time it
    // returns `Stopping`, the whole tree is already confirmed dead, or,
    // if `kill_tree` itself failed and the fallback kill did not, the
    // direct child is and its descendants (if any) were unreachable.
    // Either way it is logged there, and nothing is left to reap here.
    // `StoppingUnkillable` is the one outcome where something is: both
    // kills failed, so the daemon is still running and unreaped.
    match stop_bus.wait_for_child_or_stop(waiter, &child, id) {
        WaitOutcome::ChildExited => {
            // The child has already exited — `wait()` reaps and returns
            // immediately rather than blocking.
            let code = match child.wait() {
                Ok(status) => {
                    let code = status.code();
                    if code.is_none() {
                        logging::log_failure(id, NO_EXIT_CODE);
                    }
                    code
                }
                Err(e) => {
                    logging::log_failure(id, &exit_status_unavailable(&e));
                    None
                }
            };
            exit_report(stop_bus.is_stopping(), code)
        }
        WaitOutcome::Stopping => Reported::Clean,
        // The service still stops, which is what keeps it stoppable and
        // uninstallable; the report and the two logged kill failures are
        // what say the daemon may have outlived it.
        WaitOutcome::StoppingUnkillable => Reported::incomplete_stop(),
    }
}

/// How an exited daemon is reported. A stop requested at the same moment wins: terminating the
/// daemon's job makes it exit non-zero, which a commanded stop must not report as a failure.
/// `None` is an exit status that could not be obtained, already logged by the caller.
fn exit_report(stopping: bool, code: Option<i32>) -> Reported {
    match (stopping, code) {
        (true, _) => Reported::Clean,
        (false, Some(code)) => Reported::from_daemon_code(code),
        (false, None) => Reported::shim_failure(None),
    }
}

/// The daemon, running and already being waited on — or why not, logged, with nothing launched.
///
/// Everything waiting on the daemon needs is made before the daemon is: the [`Waiter`] thread
/// first, then the spawn, and nothing between them that could launch a daemon on the way.
/// `build_command` does sit between them, and the log-file open inside it fails routinely, but
/// that failure arm falls back to null stdio and runs nothing. A daemon is never launched with
/// nothing to wait on it, because the one thing that could refuse to make that waiter — the OS,
/// asked for a thread — is asked before anything is running. See [`Waiter`] for what the reverse
/// order cost.
///
/// The `Err` is what the service reports: the OS error of the failure, or `ERROR_GEN_FAILURE`.
fn launch(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, id: &str) -> Result<(Arc<Child>, Waiter), Reported> {
    let waiter = match stop_bus.waiter(id) {
        Ok(waiter) => waiter,
        Err(e) => {
            logging::log_failure(id, &thread_refused(&spec.command, &e));
            return Err(Reported::shim_failure(e.raw_os_error()));
        }
    };
    let mut cmd = build_command(spec, id);
    match start(&mut cmd) {
        Ok(child) => Ok((Arc::new(child), waiter)),
        Err(e) => {
            logging::log_failure(id, &spawn_failed(&spec.command, &e));
            Err(Reported::shim_failure(os_error_of(&e)))
        }
    }
}

/// The OS error code behind a `cosca` failure, if it has one.
fn os_error_of(e: &cosca::error::Error) -> Option<i32> {
    match e {
        cosca::error::Error::Io(e) => e.raw_os_error(),
        cosca::error::Error::Unassessable { source, .. } => source.as_ref().and_then(std::io::Error::raw_os_error),
        _ => None,
    }
}

// Failure reports -----------------------------------------------------------------------------------------------------

const NO_EXIT_CODE: &str = "child exited with no reportable exit code";

fn exit_status_unavailable(e: &cosca::error::Error) -> String {
    format!("query child exit status: {e}")
}

fn thread_refused(command: &[String], e: &std::io::Error) -> String {
    format!("make the thread that waits on {command:?}: {e}; it was not spawned, so nothing of this daemon is running")
}

fn spawn_failed(command: &[String], e: &cosca::error::Error) -> String {
    format!("spawn {command:?}: {e}")
}

fn log_open_failed(path: &std::path::Path, e: &std::io::Error) -> String {
    format!(
        "open log file {}: {e} (this daemon's output will not be captured; it is still being run)",
        path.display()
    )
}

/// The spawn itself: [`launch`]'s last step, once the thread that will wait on the daemon has
/// already been made.
fn start(cmd: &mut cosca::Command) -> Result<Child, cosca::error::Error> {
    #[cfg(test)]
    test_hook::spawning();
    cmd.spawn()
}

/// Build the (unspawned) command for `spec`: argv, cwd, env, Job Object
/// containment, and stdout+stderr merged into the daemon's log file.
fn build_command(spec: &DaemonSpec, id: &str) -> cosca::Command {
    let mut cmd = cosca::run(spec.command.iter().cloned());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd.clone());
    }
    cmd.envs(spec.env.clone());
    cmd.contain();

    let log_path = spec.logs.clone().unwrap_or_else(|| logging::default_log_path(id));
    match logging::open_append(&log_path) {
        Ok(file) => {
            // Merges are resolved in a second pass at spawn time (`cosca`'s
            // `resolve_stdio`), not by `Command::fd` consulting whatever is
            // already in its fd map — so the two calls below are order
            // -independent. `Command::stdout`/`stderr` can fail only for a
            // slot whose target is itself ambiguous or another merge
            // (`Error::Unsupported`); neither applies to a concrete
            // `Stdio::from_file`/`Stdio::merge(Fd::STDOUT)` pair on
            // stdout/stderr, so the discarded `Result`s below are believed
            // infallible for this exact shape, not merely convenient to
            // ignore.
            let _ = cmd.stdout(Stdio::from_file(file));
            let _ = cmd.stderr(Stdio::merge(Fd::STDOUT));
        }
        Err(e) => {
            logging::log_failure(id, &log_open_failed(&log_path, &e));
            // Same infallibility reasoning as above, for `Stdio::null()`.
            let _ = cmd.stdout(Stdio::null());
            let _ = cmd.stderr(Stdio::null());
        }
    }
    cmd
}

// test_hook ===========================================================================================================

/// Counts daemon spawns, on this thread only, so a test can assert that none happened. Mirrors
/// `goetia::backend::bounded`'s own hook, which is `#[cfg(test)]` inside the library and
/// therefore invisible here — see `stop_bus::Waiter`.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;

    thread_local! {
        static SPAWNS: Cell<usize> = const { Cell::new(0) };
    }

    /// How many daemons this thread has spawned through [`super::start`], or tried to: the count
    /// moves before `cmd.spawn()`, so a spawn that launched nothing is counted too — which is what
    /// makes "nothing was launched" assertions against it strictly stronger.
    pub(crate) fn spawns() -> usize {
        SPAWNS.get()
    }

    pub(super) fn spawning() {
        SPAWNS.set(SPAWNS.get() + 1);
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
