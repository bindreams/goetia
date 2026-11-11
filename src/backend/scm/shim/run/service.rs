//! SCM wiring: dispatch, control handler, status reporting, and the one pass that spawns the
//! daemon `goetia.yaml` describes, waits for it and reports how it ended.
//!
//! The shim never restarts the daemon. It reports the daemon's exit as the service's exit and
//! exits, once, with the code it reported; SCM's recovery actions (`backend::scm::generate`) start a
//! new shim, which reads the installed spec afresh.

use std::ffi::{OsString, c_void};
use std::sync::Arc;

use crate::backend::scm::exit_status::{
    ERROR_BAD_ARGUMENTS, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_INVALID_DATA, Reported,
};
use crate::backend::scm::manager::read_spec_blob;
use crate::blob::Blob;
use crate::error::Error;
use crate::spec::{DaemonSpec, Restart};
use cosca::error::ChildFate;
use cosca::{Containment, Fd, Stdio};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
};

use super::super::marker;
use super::exit;
use super::logging::Log;
use super::os::{Os, Real, Spawned, spawn_error_code};
use super::stop_bus::{NOT_RESTARTED, StopBus, Supervisor, WaitOutcome, Waiter, kill_for_stop, terminate_exited_tree};

// Entry ===============================================================================================================

/// The name SCM's service table carries. SCM ignores it for `SERVICE_WIN32_OWN_PROCESS`; the
/// service's real name is `service_main`'s first argument.
pub(super) const TABLE_NAME: &str = "goetia-shim";

/// Called from `main()`. Hands control to SCM, and never returns normally. [`finish`] records what
/// every path that reports `STOPPED` reported, and the dispatcher returns once SCM has processed it,
/// so this ends the process with the record (see [`exit`]); the paths that cannot report one end it
/// themselves, with a code from `crate::backend::scm::exit_status`. Reads no process arguments: the
/// service name comes from SCM.
pub(crate) fn run() -> ! {
    // Every host reaches here, and this read is what keeps its version marker in the linked binary.
    std::hint::black_box(&marker::MARKER);
    if let Err(e) = service_dispatcher::start(TABLE_NAME, ffi_service_main) {
        Log::real().failure(TABLE_NAME, &format!("connect to the SCM dispatcher: {e}"));
        exit::end_process(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32);
    }
    exit::end_when_dispatched()
}

define_windows_service!(ffi_service_main, service_main);

/// SCM's arguments to `ServiceMain`: the first is the service's name.
fn service_main(scm_args: Vec<OsString>) {
    let log = Arc::new(Log::real());
    match service_name(&log, &scm_args) {
        Ok(id) => run_service(id, log),
        Err(code) => exit::end_process(code),
    }
}

/// The service's name: SCM's first argument, which must be there, be UTF-8 and not be empty. Otherwise no spec can
/// be looked up, so the refusal is logged under [`TABLE_NAME`] and its process exit code returned;
/// no `STOPPED` is reported, because no service is known to report it for.
fn service_name<'a>(log: &Log, scm_args: &'a [OsString]) -> Result<&'a str, i32> {
    scm_args
        .first()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            log.failure(TABLE_NAME, &format!("SCM passed no usable service name: {scm_args:?}"));
            ERROR_BAD_ARGUMENTS as i32
        })
}

fn run_service(id: &str, log: Arc<Log>) {
    let supervisor = Supervisor::new(Arc::new(Real), Arc::clone(&log));
    let status_handle = match register_control_handler(id, &supervisor.bus) {
        Ok(h) => h,
        Err(e) => {
            log.failure(id, &format!("register SCM control handler: {e}"));
            exit::end_process(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32);
        }
    };

    let blob = match spec_read(id, read_spec_blob(id)) {
        Ok(blob) => blob,
        Err(stop) => {
            log.failure(id, &stop.message);
            return finish(&log, id, stop.reported, |r| report_stopped(status_handle, r));
        }
    };

    log.set_daemon_log(log.daemon_log_path(id, blob.spec.logs.as_deref()));
    log_version_skew(&log, id, &blob.spec, &blob.version, crate::version());
    let reported = run_daemon(
        &blob.spec,
        &supervisor,
        id,
        || report_running(&log, id, status_handle),
        || report_stopping(&log, id, status_handle),
    );
    finish(&log, id, reported, |r| report_stopped(status_handle, r));
}

/// Why the shim reads no spec to run, and what it tells SCM.
#[derive(Debug)]
struct SpecStop {
    message: String,
    reported: Reported,
}

/// The shim refuses ([`Reported::Refused`]) only when the service is gone, has no goetia marker or
/// has an undecodable spec; every other error is a restartable failure carrying the OS error the
/// read met, or `ERROR_INVALID_DATA`.
fn spec_read(id: &str, read: crate::error::Result<Option<Blob>>) -> Result<Blob, SpecStop> {
    let refused = |message: String| SpecStop {
        message,
        reported: Reported::Refused,
    };
    let e = match read {
        Ok(Some(blob)) => return Ok(blob),
        Ok(None) => {
            return Err(refused(format!(
                r"no Goetia marker found under Services\{id}\Parameters: this service was not created by `goetia daemon install`"
            )));
        }
        Err(e) => e,
    };
    let mut cause = e.cause();
    while let Error::Kept(kept) = cause {
        cause = kept.cause();
    }
    match cause {
        Error::NotInstalled { .. } => Err(refused(format!(
            r"Services\{id} is gone, or this service's record does not name it: no daemon was started"
        ))),
        Error::Blob { .. } | Error::Invalid { .. } => Err(refused(format!("decode metadata blob: {}", e.report()))),
        _ => Err(SpecStop {
            message: format!(r"read the metadata under Services\{id}\Parameters: {}", e.report()),
            reported: Reported::shim_failure(Some(e.raw_os_error().unwrap_or(ERROR_INVALID_DATA as i32))),
        }),
    }
}

/// Tell SCM the service stopped and return, leaving the exit to the main thread: the dispatcher
/// returns once SCM has processed `STOPPED`, and the main thread ends the process with what is
/// recorded here (see [`exit`]). When `STOPPED` cannot be reported the dispatcher is not trusted to
/// return, so the exit is here.
pub(super) fn finish(log: &Log, id: &str, reported: Reported, report: impl FnOnce(Reported) -> std::io::Result<()>) {
    exit::stopping(reported);
    if let Err(e) = report(reported) {
        log.failure(id, &stopped_report_failure(&e));
        exit::end_process(reported.process_exit_code());
    }
}

/// The `STOP_PENDING` status held while the shim tears the daemon down, accepting no control. SCM
/// then refuses a second `STOP` (1061) and a `START` (1056) itself, and a `STOPPED` that follows
/// restarts or not exactly as one that follows `RUNNING` (P18). The hint and checkpoint are those
/// P18 measured. SCM does nothing when the hint expires or the checkpoint stalls: the status, a refused
/// STOP (1061) and a refused START (1056) held for 10 s past a 1 s hint (P26).
fn stopping_status() -> SERVICE_STATUS {
    SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STOP_PENDING,
        dwControlsAccepted: 0,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 1,
        dwWaitHint: 10_000,
    }
}

/// The `STOPPED` status SCM is told for `reported`, with the exact `(win32, specific)` pair of
/// [`Reported::scm_codes`], including a service-specific code beside Win32 0, which is how an
/// incomplete stop leaves a record without a restart.
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

/// The handle `RegisterServiceCtrlHandlerExW` returned, used until the service reports `STOPPED`.
#[derive(Clone, Copy)]
struct StatusHandle(SERVICE_STATUS_HANDLE);

/// Registered directly rather than through `windows-service` 0.8.1's `register`, which frees its
/// closure after any Stop, Shutdown or Preshutdown whatever the handler returned (P16 observed it)
/// while the registration stays live: SCM can deliver a control after that, which would reach freed
/// memory. No test catches a revert; `clippy.toml` forbids the call.
fn register_control_handler(id: &str, stop_bus: &Arc<StopBus>) -> std::io::Result<StatusHandle> {
    let name = super::os::wide_null(id);
    // SAFETY: `name` is NUL-terminated and outlives the call; the context is never freed.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), leak_handler_context(stop_bus)) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(StatusHandle(handle))
}

/// The handler's context: a clone of `stop_bus`, leaked so that it outlives every control.
fn leak_handler_context(stop_bus: &Arc<StopBus>) -> *mut c_void {
    Box::into_raw(Box::new(Arc::clone(stop_bus))).cast()
}

/// # Safety
///
/// `context` comes from [`leak_handler_context`].
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    context: *mut c_void,
) -> u32 {
    // SAFETY: the caller's contract; the `Arc` it points to is never freed.
    let stop_bus = unsafe { &*context.cast::<Arc<StopBus>>() };
    handle_control(stop_bus, control)
}

/// The handler's answer to `control`. SCM acknowledges every `STOP` it delivers whatever the
/// handler answers, so a stop the bus does not accept (the exit is already decided) is answered
/// `NO_ERROR` too. The decided exit is reported as it is; SCM restarts it if the restart policy
/// covers it.
fn handle_control(stop_bus: &StopBus, control: u32) -> u32 {
    match control {
        SERVICE_CONTROL_STOP => {
            stop_bus.request_stop();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Report `RUNNING`. A failure is logged: SCM keeps the last state it confirmed.
fn report_running(log: &Log, id: &str, handle: StatusHandle) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_RUNNING,
        dwControlsAccepted: SERVICE_ACCEPT_STOP,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: `handle` is live for the life of the process, and `status` outlives the call.
    if unsafe { SetServiceStatus(handle.0, &status) } == 0 {
        let e = std::io::Error::last_os_error();
        log.failure(id, &format!("report SERVICE_RUNNING to SCM: {e}"));
    }
}

/// Report `STOP_PENDING`. A failure is logged: the teardown goes on and `STOPPED` is still reported.
fn report_stopping(log: &Log, id: &str, handle: StatusHandle) {
    let status = stopping_status();
    // SAFETY: `handle` is live for the life of the process, and `status` outlives the call.
    if unsafe { SetServiceStatus(handle.0, &status) } == 0 {
        let e = std::io::Error::last_os_error();
        log.failure(id, &format!("report SERVICE_STOP_PENDING to SCM: {e}"));
    }
}

fn report_stopped(handle: StatusHandle, reported: Reported) -> std::io::Result<()> {
    let status = stopped_status(reported);
    // SAFETY: `handle` is live for the life of the process, and `status` outlives the call.
    if unsafe { SetServiceStatus(handle.0, &status) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The shim exits after this without a `STOPPED` report. SCM is expected to read that as a crash
/// (1067) and may apply the restart policy, even after a commanded stop; exits without a report were
/// measured from `START_PENDING`, after `RUNNING` and after connecting, not from `STOP_PENDING`.
fn stopped_report_failure(e: &std::io::Error) -> String {
    format!(
        "report SERVICE_STOPPED to SCM: {e}; the shim exits anyway; SCM is expected to record 1067 (a crash) and may apply the restart policy"
    )
}

// One pass ============================================================================================================

/// Launch the daemon, report `RUNNING` through `on_running`, wait for the daemon to end or for a
/// stop, and say how it ended.
///
/// `on_stopping` is called exactly once, after `on_running` and before the daemon's tree is torn
/// down, whatever ends the run; never when `on_running` was not. The service reads `STOP_PENDING`
/// through the teardown, so a `start` is refused rather than answered "running" for a service that
/// is about to stop.
///
/// `on_running` is called exactly once, after the daemon is spawned and never when launching failed.
/// A daemon that cannot start is therefore `STOPPED` without ever having been `RUNNING`, with a
/// non-zero Win32 code or, when no Job Object contains it or cosca cannot say a failed spawn ended
/// it, the incomplete stop. `start_via_notify` reads both as a failed start under every restart
/// policy.
fn run_daemon(
    spec: &DaemonSpec,
    supervisor: &Supervisor,
    id: &str,
    on_running: impl FnOnce(),
    on_stopping: impl FnOnce(),
) -> Reported {
    let log = &*supervisor.log;
    let (child, waiter) = match launch(spec, supervisor, id) {
        Ok(launched) => launched,
        Err(reported) => return reported,
    };
    on_running();

    // `wait_for_child_or_stop` kills the tree on a stop or a failed wait, so only `DaemonMayRun`
    // leaves a daemon that may be running, and `TreeMayRun` its tree.
    match supervisor.wait_for_child_or_stop(waiter, &child, id, on_stopping) {
        WaitOutcome::ChildExited => {
            let code = match child.wait() {
                Ok(status) => {
                    let code = status.code();
                    if code.is_none() {
                        log.failure(id, NO_EXIT_CODE);
                    }
                    code
                }
                Err(e) => {
                    log.failure(id, &exit_status_unavailable(&e));
                    None
                }
            };
            end_tree_before_report(supervisor.bus.settle(exit_report(code)), child.as_ref(), log, id)
        }
        // The failure is already logged; the daemon's tree was terminated, so it is the shim that
        // failed.
        WaitOutcome::WaitFailed { os_error } => supervisor.bus.settle(Reported::shim_failure(os_error)),
        // `wait_for_child_or_stop` terminated the tree before returning this.
        WaitOutcome::Stopping => Reported::Clean,
        // Still reports STOPPED so the service stays stoppable and uninstallable.
        WaitOutcome::DaemonMayRun | WaitOutcome::TreeMayRun => Reported::incomplete_stop(),
    }
}

/// An exited daemon is reported only once the tree it left is terminated, whatever `settled` is: a
/// `start` that lands once SCM reads `STOPPED` must not run beside survivors, and a failure SCM
/// restarts must not either. If the tree cannot be terminated, SCM must not restart and the service
/// must not read as cleanly stopped: the incomplete stop.
///
/// "Terminated" is `kill_tree` succeeding on the Job Object `launch` requires, which means
/// `TerminateJobObject` succeeded. Windows does not document that as synchronous with the members'
/// teardown, so they may still be exiting when STOPPED is reported, and a restarted daemon can
/// briefly overlap them or find a resource they held (a port) and fail like any other start.
fn end_tree_before_report(settled: Reported, child: &dyn Spawned, log: &Log, id: &str) -> Reported {
    if terminate_exited_tree(child, log, id) {
        settled
    } else {
        Reported::incomplete_stop()
    }
}

/// How an exited daemon is reported, before [`StopBus::settle`] lets a stop win. `None` is an exit
/// status that could not be obtained, already logged by the caller.
fn exit_report(code: Option<i32>) -> Reported {
    match code {
        Some(code) => Reported::from_daemon_code(code),
        None => Reported::shim_failure(None),
    }
}

/// The daemon, running, job-contained and already being waited on — or why not, logged.
///
/// The [`Waiter`] thread is made before the daemon is spawned, so a daemon is never running with
/// nothing to wait on it. `build_command` sits between them but never launches anything.
///
/// The `Err` is what the service reports: the OS error of the failure, `ERROR_GEN_FAILURE`, or the
/// incomplete stop for a daemon no Job Object contains or a failed spawn may have left running.
fn launch(spec: &DaemonSpec, supervisor: &Supervisor, id: &str) -> Result<(Arc<dyn Spawned>, Waiter), Reported> {
    let log = &*supervisor.log;
    let waiter = match supervisor.waiter(id) {
        Ok(waiter) => waiter,
        Err(e) => {
            log.failure(id, &thread_refused(&spec.command, &e));
            return Err(Reported::shim_failure(e.raw_os_error()));
        }
    };
    // A waiter that is not handed the daemon is stood down, not dropped: see `Waiter::stand_down`.
    match supervisor.os.spawn(build_command(spec, &*supervisor.os, log, id)) {
        Ok(child) => match require_job_object(child, spec, log, id) {
            Ok(child) => Ok((Arc::from(child), waiter)),
            Err(reported) => {
                waiter.stand_down();
                Err(reported)
            }
        },
        Err(e) if may_still_run(&e) => {
            log.failure(id, &spawn_failed_daemon_may_run(&spec.command, &e));
            waiter.stand_down();
            Err(Reported::incomplete_stop())
        }
        Err(e) => {
            log.failure(id, &spawn_failed(&spec.command, &e));
            waiter.stand_down();
            Err(Reported::shim_failure(spawn_error_code(&e)))
        }
    }
}

/// Whether a failed spawn may have left the daemon running: its error says so, and cosca did not end
/// the child. SCM restarts a failed start, so reporting one as such would start a second copy.
fn may_still_run(e: &cosca::error::Error) -> bool {
    e.fate()
        .is_some_and(|fate| !matches!(fate, ChildFate::Reaped | ChildFate::Killed | ChildFate::Gone))
}

/// The daemon, if a Job Object contains it. `type: simple` is stopped, and restarted after a
/// failure, only by terminating its whole tree, which `cosca` can guarantee only with a job: when
/// assigning the job fails it falls back to a process-tree walk that cannot say whether a
/// descendant was killed. Any other containment is a start failure: the daemon is killed, but
/// that kill proves nothing about its descendants, so the record is the incomplete stop whatever
/// the kill returned. SCM never restarts over it, and `start` reads it as a failed start.
fn require_job_object(
    child: Box<dyn Spawned>,
    spec: &DaemonSpec,
    log: &Log,
    id: &str,
) -> Result<Box<dyn Spawned>, Reported> {
    let containment = child.containment();
    if containment == Containment::JobObject {
        return Ok(child);
    }
    log.failure(id, &not_job_contained(&spec.command, containment));
    // Dropping the child tries the kill again, and a refusal is not waited on.
    kill_for_stop(child.as_ref(), log, id);
    Err(Reported::incomplete_stop())
}

// Failure reports -----------------------------------------------------------------------------------------------------

const NO_EXIT_CODE: &str = "child exited with no reportable exit code";

fn exit_status_unavailable(e: &cosca::error::Error) -> String {
    format!("query child exit status: {e}")
}

fn thread_refused(command: &[String], e: &std::io::Error) -> String {
    format!("make the thread that waits on {command:?}: {e}; it was not spawned, so nothing of this daemon is running")
}

fn not_job_contained(command: &[String], containment: Containment) -> String {
    format!(
        "the daemon {command:?} was contained by {containment}, not a job object: `type: simple` needs a job object to terminate its whole tree. The daemon ran from its launch, and no kill of it shows that its descendants are gone, so it is reported as possibly still running"
    )
}

fn spawn_failed(command: &[String], e: &cosca::error::Error) -> String {
    format!("spawn {command:?}: {e}")
}

fn spawn_failed_daemon_may_run(command: &[String], e: &cosca::error::Error) -> String {
    format!(
        "{}; the daemon may still be running{NOT_RESTARTED}",
        spawn_failed(command, e)
    )
}

/// Log [`version_skew_notice`] as a warning: the daemon runs, and each start is a new shim that
/// sees the same condition until `goetia daemon install` is run again.
fn log_version_skew(log: &Log, id: &str, spec: &DaemonSpec, registered_by: &str, running: &str) {
    if let Some(notice) = version_skew_notice(id, spec, registered_by, running) {
        log.warning(id, &notice);
    }
}

/// The line for a service registered by another goetia version, or `None` when there is
/// nothing to say. Recovery actions are applied at install time, so a service registered before
/// they existed (or by a version that set different ones) keeps running under this shim without
/// restarts until `goetia daemon install` is run again.
fn version_skew_notice(id: &str, spec: &DaemonSpec, registered_by: &str, running: &str) -> Option<String> {
    if registered_by == running || spec.restart == Restart::Never {
        return None;
    }
    let remedy = if spec.restart == Restart::Always {
        format!(
            "After setting `backend-specific: scm: {{ restart: on-failure }}` in the manifest, run `goetia daemon uninstall {id}` and `goetia daemon install` to apply them: `restart: always` is not supported for scm, so `install` refuses this service as it is"
        )
    } else {
        "Run `goetia daemon install` to apply them".to_string()
    };
    Some(format!(
        "daemon {:?} was registered by goetia {registered_by} and this goetia-shim is {running}: its SCM recovery actions may be missing, so it may not restart after a failure. {remedy}",
        spec.name
    ))
}

fn log_open_failed(path: &std::path::Path, e: &std::io::Error) -> String {
    format!(
        "open log file {}: {e} (this daemon's output will not be captured; it is still being run)",
        path.display()
    )
}

/// A stdio assignment `build_command` expects never to fail: asserted in debug builds, logged in
/// release, where the daemon then runs with that stream unredirected.
fn stdio_set<T, E: std::fmt::Display>(log: &Log, id: &str, assigned: Result<T, E>) {
    if let Err(e) = assigned {
        debug_assert!(false, "a stdio assignment build_command expects to succeed failed: {e}");
        log.failure(id, &format!("redirect the daemon's output: {e}"));
    }
}

/// Build the (unspawned) command for `spec`: argv, cwd, env, Job Object
/// containment, and stdout+stderr merged into the daemon's log file, which `os` opens. `log`
/// names that file when the spec does not, and reports a failed open or redirect.
pub(crate) fn build_command(spec: &DaemonSpec, os: &dyn Os, log: &Log, id: &str) -> cosca::Command {
    let mut cmd = cosca::run(spec.command.iter().cloned());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd.clone());
    }
    cmd.envs(spec.env.clone());
    cmd.contain();

    let log_path = log.daemon_log_path(id, spec.logs.as_deref());
    match os.open_log(&log_path) {
        Ok(file) => {
            // Merges are resolved in a second pass at spawn time (`cosca`'s
            // `resolve_stdio`), not by `Command::fd` consulting whatever is
            // already in its fd map — so the two calls below are order
            // -independent. `Command::stdout`/`stderr` can fail only for a
            // slot whose target is itself ambiguous or another merge
            // (`Error::Unsupported`); neither applies to a concrete
            // `Stdio::from_file`/`Stdio::merge(Fd::STDOUT)` pair on
            // stdout/stderr, so these are expected never to fail; `stdio_set`
            // asserts that in debug builds and logs it in release.
            stdio_set(log, id, cmd.stdout(Stdio::from_file(file)));
            stdio_set(log, id, cmd.stderr(Stdio::merge(Fd::STDOUT)));
        }
        Err(e) => {
            log.failure(id, &log_open_failed(&log_path, &e));
            // Same reasoning as above, for `Stdio::null()`.
            stdio_set(log, id, cmd.stdout(Stdio::null()));
            stdio_set(log, id, cmd.stderr(Stdio::null()));
        }
    }
    cmd
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
