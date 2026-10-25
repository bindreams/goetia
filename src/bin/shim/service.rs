//! SCM wiring: dispatch, control handler, status reporting, and the one pass that spawns the
//! daemon `goetia.yaml` describes, waits for it and reports how it ended.
//!
//! The shim never restarts the daemon. It reports the daemon's exit as the service's exit and
//! exits; SCM's recovery actions (`backend::scm::generate`) start a new shim, which reads the
//! installed spec afresh.
//!
//! Process-tree containment and waiting belong to `cosca`; this module interrupts its blocking
//! `child.wait()` on `Stop` (`stop_bus`) and drives SCM's status protocol.

use std::ffi::OsString;
use std::os::windows::io::AsRawHandle as _;
use std::sync::Arc;
use std::time::Duration;

use cosca::identity::ProcessId;
use cosca::{Child, Fd, Stdio};
use goetia::backend::scm::exit_status::{
    ERROR_BAD_ARGUMENTS, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_INVALID_DATA, Reported,
};
use goetia::backend::scm::manager::read_spec_blob;
use goetia::spec::{DaemonSpec, Restart};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::System::Services::{
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
};

use crate::logging;
use crate::state::{self, Store};
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
    // `service_main` exits on every path; this covers the dispatcher returning anyway.
    std::process::exit(Reported::Clean.process_exit_code());
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_scm_args: Vec<OsString>) {
    // SCM's own arguments are not ours to rely on; `run` validated argv[1].
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

    let store = Store::for_service(id);
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
                &store,
                Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
            );
        }
        Err(e) => {
            logging::log_failure(id, &format!("decode metadata blob: {e}"));
            finish(
                id,
                &status_handle,
                &store,
                Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
            );
        }
    };

    log_version_skew(id, &blob.spec, &blob.version, goetia::version());
    if let Err(reported) = state::begin(id, &store) {
        finish(id, &status_handle, &store, reported);
    }
    let reported = run_daemon(
        &blob.spec,
        &stop_bus,
        id,
        || report_running(id, &status_handle),
        |survivor| state::record_survivor(id, &store, survivor),
    );
    finish(id, &status_handle, &store, reported);
}

/// Record the failure `reported` stands for, tell SCM the service stopped, then end the process.
/// SCM restarts the service or not from the status reported here, never from the process exit
/// code. The record comes first: SCM's restart can follow the report at once, and
/// `goetia daemon stop` has to find the record by then.
fn finish(id: &str, status_handle: &ServiceStatusHandle, store: &Store, reported: Reported) -> ! {
    state::record_failure(id, store, reported);
    report_stopped(id, status_handle, reported);
    std::process::exit(reported.process_exit_code());
}

/// The `STOPPED` status SCM is told for `reported`, with the exact `(win32, specific)` pair of
/// [`Reported::scm_codes`]. Built raw because `windows-service`'s `ServiceExitCode` cannot express
/// a service-specific code beside Win32 0, which is how an incomplete stop leaves a record without
/// a restart.
fn stopped_status(reported: Reported) -> SERVICE_STATUS {
    let (win32, specific) = reported.scm_codes();
    SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STOPPED,
        dwControlsAccepted: 0,
        dwWin32ExitCode: win32,
        dwServiceSpecificExitCode: specific,
        dwCheckPoint: 0,
        dwWaitHint: 0,
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

/// Report `RUNNING`. A failure is logged: SCM keeps the last state it confirmed.
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

fn report_stopped(id: &str, handle: &ServiceStatusHandle, reported: Reported) {
    let status = stopped_status(reported);
    // SAFETY: `handle` wraps the live `SERVICE_STATUS_HANDLE` that `RegisterServiceCtrlHandlerExW`
    // returned (it stays valid for the life of the process), and `status` outlives the call.
    let ok = unsafe { SetServiceStatus(handle.as_raw_handle() as SERVICE_STATUS_HANDLE, &status) };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
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
///
/// `on_survivor` is called, with the daemon's identity, only for a stop that could not kill it, and
/// before the incomplete stop is returned to be reported.
fn run_daemon(
    spec: &DaemonSpec,
    stop_bus: &Arc<StopBus>,
    id: &str,
    on_running: impl FnOnce(),
    on_survivor: impl FnOnce(ProcessId),
) -> Reported {
    let (child, waiter) = match launch(spec, stop_bus, id) {
        Ok(launched) => launched,
        Err(reported) => return reported,
    };
    on_running();

    // `wait_for_child_or_stop` kills and confirms the tree on a stop, so only `StoppingUnkillable`
    // leaves a running, unreaped daemon.
    match stop_bus.wait_for_child_or_stop(waiter, &child, id) {
        WaitOutcome::ChildExited => {
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
        // The failure is already logged; the daemon was torn down, so it is the shim that failed.
        WaitOutcome::WaitFailed { os_error } => Reported::shim_failure(os_error),
        WaitOutcome::Stopping => Reported::Clean,
        // Still reports STOPPED so the service stays stoppable and uninstallable.
        WaitOutcome::StoppingUnkillable => {
            on_survivor(child.id());
            Reported::incomplete_stop()
        }
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
/// The [`Waiter`] thread is made before the daemon is spawned, so a daemon is never running with
/// nothing to wait on it. `build_command` sits between them but never launches anything.
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
pub(crate) fn os_error_of(e: &cosca::error::Error) -> Option<i32> {
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

/// Log [`version_skew_notice`] as a warning: the daemon runs, and each start is a new shim that
/// sees the same condition until `goetia daemon install` is run again.
fn log_version_skew(id: &str, spec: &DaemonSpec, registered_by: &str, running: &str) {
    if let Some(notice) = version_skew_notice(spec, registered_by, running) {
        logging::log_warning(id, &notice);
    }
}

/// The line for a service registered by another goetia version, or `None` when there is
/// nothing to say. Recovery actions are applied at install time, so a service registered before
/// they existed (or by a version that set different ones) keeps running under this shim without
/// restarts until `goetia daemon install` is run again.
fn version_skew_notice(spec: &DaemonSpec, registered_by: &str, running: &str) -> Option<String> {
    if registered_by == running || spec.restart == Restart::Never {
        return None;
    }
    let mut notice = format!(
        "daemon {:?} was registered by goetia {registered_by} and this goetia-shim is {running}: its SCM recovery actions may be missing, so it may not restart after a failure. Run `goetia daemon install` to apply them",
        spec.name
    );
    if spec.restart == Restart::Always {
        notice.push_str(
            ", after setting `backend-specific: scm: { restart: on-failure }` in the manifest: `restart: always` is not supported for scm",
        );
    }
    Some(notice)
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
