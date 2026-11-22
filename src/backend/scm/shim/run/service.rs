//! SCM wiring: dispatch, control handler, status reporting, and the one pass that spawns the
//! daemon `goetia.yaml` describes, waits for it and reports how it ended.
//!
//! The shim never restarts the daemon. It reports the daemon's exit as the service's exit and
//! exits, once, with the code it reported; SCM's recovery actions (`backend::scm::generate`) start a
//! new shim, which reads the installed spec afresh.

use std::ffi::{OsString, c_void};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use crate::backend::scm::exit_status::{
    ERROR_BAD_ARGUMENTS, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_INVALID_DATA, Reported,
};
use crate::backend::scm::manager::read_spec_blob;
use crate::backend::scm::wide::wide_null;
use crate::backend::scm::win32::winapi_io;
use crate::blob::Blob;
use crate::error::{Chain, Error};
use crate::spec::{DaemonSpec, Restart, StopAction};
use cosca::error::ChildFate;
use cosca::{ContainMode, Containment, Fd, Stdio};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, FALSE, NO_ERROR};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_PRESHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
};

use super::exit;
use super::logging::Log;
use super::os::{LogKind, Os, Real, Spawned, spawn_error_code};
use super::stop_bus::{
    NOT_RESTARTED, RootExit, StopBus, StopEvent, StopPlan, Supervisor, WaitEnd, WaitOutcome, Waiter, kill_for_stop,
    terminate_exited_tree,
};

// Entry ===============================================================================================================

/// The name SCM's service table carries. SCM ignores it for `SERVICE_WIN32_OWN_PROCESS`; the
/// service's real name is `service_main`'s first argument.
pub(super) const TABLE_NAME: &str = "goetia-shim";

/// Called from `main()`. Hands control to SCM, and never returns normally. [`finish`] records what
/// every path that reports `STOPPED` reported, and the dispatcher returns once SCM has processed it,
/// so this ends the process with the record (see [`exit`]); the paths that cannot report one end it
/// themselves, with a code from `crate::backend::scm::exit_status`. Reads no process arguments itself
/// (`super::run` did, to choose this mode): the service name comes from SCM.
pub(crate) fn run() -> ! {
    if let Err(e) = service_dispatcher::start(TABLE_NAME, ffi_service_main) {
        Log::real(Arc::default()).failure(TABLE_NAME, &dispatcher_failure(e));
        exit::end_process(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32);
    }
    exit::end_when_dispatched()
}

define_windows_service!(ffi_service_main, service_main);

/// SCM's arguments to `ServiceMain`: the first is the service's name.
fn service_main(scm_args: Vec<OsString>) {
    // One `Real` for the run: the supervisor's `Os` and the log's channels share the handle held on the default log.
    let real = Arc::new(Real::default());
    let log = Arc::new(Log::real(Arc::clone(&real)));
    exit::keep_log(Arc::clone(&log));
    match service_name(&log, &scm_args) {
        Ok(id) => run_service(id, log, real),
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

fn run_service(id: &str, log: Arc<Log>, real: Arc<Real>) {
    // Before the control handler, and so before the first status this service reports: an install that changes
    // the account leaves the log's permissions as they are while SCM reads this service `START_PENDING`, and
    // narrows them once it reads any other state, since a handle opened now keeps working.
    log.hold_default(id);
    exit::name_run(id);
    let supervisor = Supervisor::new(real, Arc::clone(&log));
    let status_handle = match register_control_handler(id, &supervisor.bus) {
        Ok(h) => h,
        Err(e) => {
            log.failure(id, &format!("register SCM control handler: {}", Chain(&e)));
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

    set_daemon_log(&log, id, &blob.spec);
    log_version_skew(&log, id, &blob.spec, &blob.version, crate::version());
    let plan = StopPlan::of(&blob.spec);
    if let Err(stop) = refuse_unready_host(&blob.spec, plan, cosca::helpers_initialized()) {
        log.failure(id, &stop.message);
        return finish(&log, id, stop.reported, |r| report_stopped(status_handle, r));
    }
    if plan.asks() {
        clear_ctrl_c_ignored(&log, id);
    }
    let mut progress = StopProgress::default();
    let reported = run_daemon(
        &blob.spec,
        plan,
        &supervisor,
        id,
        || report_running(&log, id, status_handle),
        // Progress is reported from this thread, between the slices of the stop's own waits: nothing is reported
        // while the ask itself is being made, which cosca's `terminate_tree` bounds by nothing.
        |event| {
            let status = match event {
                StopEvent::Began(end) => progress.begin(stop_hint(blob.spec.stop_action(), end)),
                StopEvent::Progress => progress.advance(),
            };
            report_stopping(&log, id, status_handle, &status);
        },
    );
    finish(&log, id, reported, |r| report_stopped(status_handle, r));
}

/// From now on the shim's failures go to the daemon's own log, interleaved with its output: the spec's
/// `logs`, tried before the default. Call right after the spec decodes.
pub(super) fn set_daemon_log(log: &Log, id: &str, spec: &DaemonSpec) {
    if let Some((path, LogKind::Own)) = log.daemon_log(id, spec.common().logs.as_deref()) {
        log.set_daemon_log(path);
    }
}

/// Whether this host can run `spec`'s stop, which `plan` is: a stop that asks goes through cosca's proxy, which is a
/// copy of the host started with the proxy's arguments, so the host's `main` must have called `init_helpers` first.
pub(super) fn refuse_unready_host(spec: &DaemonSpec, plan: StopPlan, initialised: bool) -> Result<(), SpecStop> {
    if initialised || !plan.asks() {
        return Ok(());
    }
    Err(SpecStop {
        message: format!(
            "this host did not call `goetia::shim::init_host()` first in its `main`: a stop of {:?} asks the daemon to \
             exit through a copy of the host that cosca starts as a console-event proxy, which runs the host's `main` and \
             cannot do it without that call. No daemon was started. Call it before anything else in `main`; \
             `goetia-shim` does",
            spec.common().name
        ),
        reported: Reported::Refused,
    })
}

/// Why the shim reads no spec to run, and what it reports for it.
#[derive(Debug)]
pub(super) struct SpecStop {
    pub(super) message: String,
    pub(super) reported: Reported,
}

/// The shim refuses ([`Reported::Refused`]) only when the service is gone, has no goetia marker, has an
/// undecodable spec or holds a task; every other error is a restartable failure carrying the OS error the
/// read met, or `ERROR_INVALID_DATA`.
fn spec_read(id: &str, read: crate::error::Result<Option<Blob>>) -> Result<Blob, SpecStop> {
    let refused = |message: String| SpecStop {
        message,
        reported: Reported::Refused,
    };
    let e = match read {
        Ok(Some(blob)) if matches!(blob.spec, DaemonSpec::Task(_)) => {
            return Err(refused(
                "this is a task's blob; the service host runs only services".to_string(),
            ));
        }
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

/// What a stop that ended the wait with `end` tells SCM it can take, as `STOP_PENDING`'s wait hint: the stop action's
/// longest wait ([`StopAction::longest_stop`]; the largest the field holds for none), or, for the daemon's own exit,
/// the teardown alone. Only a service reports a hint: task mode reports to no SCM.
fn stop_hint(action: &StopAction, end: WaitEnd) -> Duration {
    match end {
        WaitEnd::Stop => action.longest_stop().unwrap_or(Duration::MAX),
        WaitEnd::Exit => StopAction::TEARDOWN_MARGIN,
    }
}

/// `hint` as the wait hint field holds it: milliseconds, at most `u32::MAX`, which is what a stop with no `timeout`
/// reports.
fn wait_hint_millis(hint: Duration) -> u32 {
    u32::try_from(hint.as_millis()).unwrap_or(u32::MAX)
}

/// The `STOP_PENDING` status held while the shim stops the daemon, accepting no control. SCM
/// then refuses a second `STOP` (1061) and a `START` (1056) itself, and a `STOPPED` that follows
/// restarts or not exactly as one that follows `RUNNING` (measured). The hint is the stop's longest wait
/// ([`stop_hint`]): the `timeout` of a stop that asks, and the teardown. The checkpoint is [`StopProgress`]'s.
/// Measured: SCM does nothing when the hint expires (the status, a refused STOP (1061) and a refused START (1056) held
/// for 10 s past a 1 s hint), nor does it with a checkpoint that moves, held for 150 s on a plain `STOP`.
fn stopping_status(hint: Duration, checkpoint: u32) -> SERVICE_STATUS {
    SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STOP_PENDING,
        dwControlsAccepted: 0,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint_millis(hint),
    }
}

/// The `STOP_PENDING` reports of one stop: the first, and each one after it that says the stop is still going on,
/// with the checkpoint one higher and the hint the stop began with. Windows ends a service at a shutdown whose
/// checkpoint stays put for about 45 s, and waits for as long as it moves (measured in Server Core containers of
/// Windows Server 2025, build 26100); SCM does nothing with it on any other stop.
#[derive(Default)]
struct StopProgress {
    hint: Duration,
    checkpoint: u32,
}

impl StopProgress {
    /// The first report, of a stop that can take `hint`: checkpoint 1.
    fn begin(&mut self, hint: Duration) -> SERVICE_STATUS {
        self.hint = hint;
        self.checkpoint = 1;
        stopping_status(hint, self.checkpoint)
    }

    /// The report that the stop is still going on.
    fn advance(&mut self) -> SERVICE_STATUS {
        self.checkpoint = self.checkpoint.wrapping_add(1);
        stopping_status(self.hint, self.checkpoint)
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
/// closure after any Stop, Shutdown or Preshutdown whatever the handler returned (measured)
/// while the registration stays live: SCM can deliver a control after that, which would reach freed
/// memory. No test catches a revert; `clippy.toml` forbids the call.
fn register_control_handler(id: &str, stop_bus: &Arc<StopBus>) -> std::io::Result<StatusHandle> {
    let name = wide_null(id)?;
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

/// The handler's answer to `control`. A system shutdown's `PRESHUTDOWN` is a stop: one path, whichever control began it.
/// SCM acknowledges every `STOP` it delivers whatever the
/// handler answers, so a stop the bus does not accept (the exit is already decided) is answered
/// `NO_ERROR` too. The decided exit is reported as it is; SCM restarts it if the restart policy
/// covers it.
fn handle_control(stop_bus: &StopBus, control: u32) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_PRESHUTDOWN => {
            stop_bus.request_stop();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// The `RUNNING` status. `PRESHUTDOWN` is accepted beside `STOP`: a service that accepts it is sent it when the system
/// shuts down, and is waited for as long as its pre-shutdown time ([`crate::backend::scm::generate`] gives it);
/// one that does not is sent nothing and ended within seconds (measured in Server Core containers of Windows
/// Server 2025, build 26100).
fn running_status() -> SERVICE_STATUS {
    SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_RUNNING,
        dwControlsAccepted: SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PRESHUTDOWN,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    }
}

/// Report `RUNNING`. A failure is logged: SCM keeps the last state it confirmed.
fn report_running(log: &Log, id: &str, handle: StatusHandle) {
    let status = running_status();
    // SAFETY: `handle` is live for the life of the process, and `status` outlives the call.
    if unsafe { SetServiceStatus(handle.0, &status) } == 0 {
        let e = std::io::Error::last_os_error();
        log.failure(id, &format!("report SERVICE_RUNNING to SCM: {}", Chain(&e)));
    }
}

/// Clear this process's own "ignore Ctrl+C" state, which a child created without a process group of its own
/// inherits: with it set the event the stop sends never arrives (measured). A failure is logged and the
/// daemon is still run; its stop then ends it when its `timeout` is over.
fn clear_ctrl_c_ignored(log: &Log, id: &str) {
    // SAFETY: a null handler routine with `FALSE` clears the calling process's own Ctrl+C-ignore state.
    if unsafe { SetConsoleCtrlHandler(None, FALSE) } == 0 {
        let e = std::io::Error::last_os_error();
        log.warning(
            id,
            &format!(
                "clear the shim's own Ctrl+C-ignore state: {}; a stop's Ctrl+C may not reach the daemon",
                Chain(&e)
            ),
        );
    }
}

/// Report `STOP_PENDING`, as `status` ([`StopProgress`]) says. A failure is logged: the teardown goes on and
/// `STOPPED` is still reported.
fn report_stopping(log: &Log, id: &str, handle: StatusHandle, status: &SERVICE_STATUS) {
    // SAFETY: `handle` is live for the life of the process, and `status` outlives the call.
    if unsafe { SetServiceStatus(handle.0, status) } == 0 {
        let e = std::io::Error::last_os_error();
        log.failure(id, &format!("report SERVICE_STOP_PENDING to SCM: {}", Chain(&e)));
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

fn dispatcher_failure(e: windows_service::Error) -> String {
    format!("connect to the SCM dispatcher: {}", Chain(&winapi_io(e)))
}

/// The shim exits after this without a `STOPPED` report. SCM is expected to read that as a crash
/// (1067) and may apply the restart policy, even after a commanded stop; exits without a report were
/// measured from `START_PENDING`, after `RUNNING` and after connecting, not from `STOP_PENDING`.
fn stopped_report_failure(e: &std::io::Error) -> String {
    format!(
        "report SERVICE_STOPPED to SCM: {}; the shim exits anyway; SCM is expected to record 1067 (a crash) and may apply the restart policy",
        Chain(e)
    )
}

// One pass ============================================================================================================

/// Launch the daemon, report `RUNNING` through `on_running`, wait for the daemon to end or for a
/// stop, and tell SCM how it ended: [`run_pass`], reported.
pub(super) fn run_daemon(
    spec: &DaemonSpec,
    plan: StopPlan,
    supervisor: &Supervisor,
    id: &str,
    on_running: impl FnOnce(),
    observe: impl FnMut(StopEvent),
) -> Reported {
    run_pass(spec, plan, supervisor, id, on_running, observe).reported()
}

/// How one pass ended, before it is read as anything: what the service tells SCM
/// ([`PassOutcome::reported`]) and what a task records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PassOutcome {
    /// The daemon exited on its own, with its tree terminated; `None` when its exit status could not
    /// be had, which is already logged.
    Exited(Option<i32>),
    /// A stop was accepted, and the daemon's tree was terminated. It wins over the exit code the
    /// teardown made the daemon leave, and over a failed wait. The code is the daemon's own exit, when the shim had its
    /// exit status because the daemon had exited by itself, before the teardown; `None` when the teardown ended it
    /// (or the wait failed, or the status could not be had). A service ignores it; a task's record is that exit.
    Stopped(Option<i32>),
    /// Something of the daemon's tree may still be running, after a stop, a failure, or an exit
    /// whose tree could not be terminated.
    IncompleteStop,
    /// The wait on the daemon failed, with this OS error if it has one, and the tree was terminated.
    WaitFailed(Option<i32>),
    /// Launching failed, with what [`launch`] reports for it: a failure of the shim's, or an
    /// incomplete stop for a daemon that may have started.
    NotRun(Reported),
}

impl PassOutcome {
    /// What the service tells SCM, and what the pass returned before it was an outcome.
    pub(super) fn reported(self) -> Reported {
        match self {
            PassOutcome::Exited(code) => exit_report(code),
            PassOutcome::Stopped(_) => Reported::Clean,
            PassOutcome::IncompleteStop => Reported::incomplete_stop(),
            PassOutcome::WaitFailed(os_error) => Reported::shim_failure(os_error),
            PassOutcome::NotRun(reported) => reported,
        }
    }
}

/// Launch the daemon, report `RUNNING` through `on_running`, wait for the daemon to end or for a
/// stop, and say how it ended. `plan` is the run's [`StopPlan`]: the daemon is spawned for it, and it is the stop the
/// supervisor runs.
///
/// `observe` is told [`StopEvent::Began`] exactly once, after `on_running` and before anything is asked of the daemon
/// or its tree is torn down, whatever ends the run; never when `on_running` was not. It says by a [`WaitEnd`] whether
/// an accepted stop or the daemon's own end ended the wait. The service reads `STOP_PENDING` through the teardown, so
/// a `start` is refused rather than answered "running" for a service that is about to stop. Any
/// [`StopEvent::Progress`] follows it ([`Supervisor::wait_for_child_or_stop`]).
///
/// `on_running` is called exactly once, after the daemon is spawned and never when launching failed.
/// A daemon that cannot start is therefore `STOPPED` without ever having been `RUNNING`, with a
/// non-zero Win32 code or, when no Job Object contains it or cosca cannot say a failed spawn ended
/// it, the incomplete stop. `start_via_notify` reads both as a failed start under every restart
/// policy.
///
/// An exited daemon is an outcome only once the tree it left is terminated: a `start` that lands
/// once SCM reads `STOPPED` must not run beside survivors, and a failure SCM restarts must not
/// either. If the tree cannot be terminated the outcome is the incomplete stop. "Terminated" is
/// `send_kill_tree` succeeding on the Job Object `launch` requires: the kill was sent, not waited
/// out (see `Supervisor::wait_for_child_or_stop`).
pub(super) fn run_pass(
    spec: &DaemonSpec,
    plan: StopPlan,
    supervisor: &Supervisor,
    id: &str,
    on_running: impl FnOnce(),
    observe: impl FnMut(StopEvent),
) -> PassOutcome {
    let log = &*supervisor.log;
    let (child, waiter) = match launch(spec, plan, supervisor, id) {
        Ok(launched) => launched,
        Err(reported) => return PassOutcome::NotRun(reported),
    };
    on_running();

    // `wait_for_child_or_stop` kills the tree on a stop or a failed wait, so only `DaemonMayRun`
    // leaves a daemon that may be running, and `TreeMayRun` its tree.
    match supervisor.wait_for_child_or_stop(waiter, &child, id, plan, observe) {
        WaitOutcome::ChildExited => {
            let code = exit_code(child.as_ref(), log, id);
            // A stop accepted before this decision wins over a failure: `settle` then says something
            // other than the candidate. A clean exit and a stop accepted after the wait ended read
            // alike.
            let candidate = exit_report(code);
            let stop_won = supervisor.bus.settle(candidate) != candidate;
            if !terminate_exited_tree(child.as_ref(), log, id) {
                PassOutcome::IncompleteStop
            } else if stop_won {
                PassOutcome::Stopped(code)
            } else {
                PassOutcome::Exited(code)
            }
        }
        // The failure is already logged; the daemon's tree was terminated, so it is the shim that
        // failed, unless a stop was accepted first.
        WaitOutcome::WaitFailed { os_error } => {
            let candidate = Reported::shim_failure(os_error);
            if supervisor.bus.settle(candidate) == candidate {
                PassOutcome::WaitFailed(os_error)
            } else {
                PassOutcome::Stopped(None)
            }
        }
        // `wait_for_child_or_stop` terminated the tree before returning this. A daemon that exited by itself first
        // has its exit status: the one a read of the process gave, or, when none did, the one the waiter's wait
        // returned (as in the `ChildExited` arm).
        WaitOutcome::Stopping(root) => PassOutcome::Stopped(match root {
            RootExit::Status(status) => code_of(status, log, id),
            RootExit::Unread => exit_code(child.as_ref(), log, id),
            RootExit::NotSeen => None,
        }),
        // Still reports STOPPED so the service stays stoppable and uninstallable.
        WaitOutcome::DaemonMayRun | WaitOutcome::TreeMayRun => PassOutcome::IncompleteStop,
    }
}

/// The exit code of a daemon that has exited and been waited for, or `None` when its exit status could not be had or
/// carries no code: told in the log.
fn exit_code(child: &dyn Spawned, log: &Log, id: &str) -> Option<i32> {
    match child.wait() {
        Ok(status) => code_of(status, log, id),
        Err(e) => {
            log.failure(id, &exit_status_unavailable(&e));
            None
        }
    }
}

/// The code of an exit status, or `None` when it carries none: told in the log.
fn code_of(status: ExitStatus, log: &Log, id: &str) -> Option<i32> {
    let code = status.code();
    if code.is_none() {
        log.failure(id, NO_EXIT_CODE);
    }
    code
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
fn launch(
    spec: &DaemonSpec,
    plan: StopPlan,
    supervisor: &Supervisor,
    id: &str,
) -> Result<(Arc<dyn Spawned>, Waiter), Reported> {
    let log = &*supervisor.log;
    let waiter = match supervisor.waiter(id) {
        Ok(waiter) => waiter,
        Err(e) => {
            log.failure(id, &thread_refused(&spec.common().command, &e));
            return Err(Reported::shim_failure(e.raw_os_error()));
        }
    };
    // A waiter that is not handed the daemon is stood down, not dropped: see `Waiter::stand_down`.
    match supervisor.os.spawn(build_command(spec, plan, &*supervisor.os, log, id)) {
        Ok(child) => match require_job_object(child, spec, log, id) {
            Ok(child) => Ok((Arc::from(child), waiter)),
            Err(reported) => {
                waiter.stand_down();
                Err(reported)
            }
        },
        Err(e) if may_still_run(&e) => {
            log.failure(id, &spawn_failed_daemon_may_run(&spec.common().command, &e));
            waiter.stand_down();
            Err(Reported::incomplete_stop())
        }
        Err(e) => {
            log.failure(id, &spawn_failed(&spec.common().command, &e));
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

/// The daemon, if a Job Object contains it. `protocol: plain` is stopped, and restarted after a
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
    log.failure(id, &not_job_contained(&spec.common().command, containment));
    // Dropping the child tries the kill again, and a refusal is not waited on.
    kill_for_stop(child.as_ref(), log, id);
    Err(Reported::incomplete_stop())
}

// Failure reports -----------------------------------------------------------------------------------------------------

const NO_EXIT_CODE: &str = "child exited with no reportable exit code";

fn exit_status_unavailable(e: &cosca::error::Error) -> String {
    format!("query child exit status: {}", Chain(e))
}

fn thread_refused(command: &[String], e: &std::io::Error) -> String {
    format!(
        "make the thread that waits on {command:?}: {}; it was not spawned, so nothing of this daemon is running",
        Chain(e)
    )
}

fn not_job_contained(command: &[String], containment: Containment) -> String {
    format!(
        "the daemon {command:?} was contained by {containment}, not a job object: `protocol: plain` needs a job object to terminate its whole tree. The daemon ran from its launch, and no kill of it shows that its descendants are gone, so it is reported as possibly still running"
    )
}

fn spawn_failed(command: &[String], e: &cosca::error::Error) -> String {
    format!("spawn {command:?}: {}", Chain(e))
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
    let DaemonSpec::Service(s) = spec else {
        // `spec_read` refuses a task's blob before this runs.
        debug_assert!(false, "a task reached the service host's version-skew notice");
        return None;
    };
    if registered_by == running || s.restart == Restart::Never {
        return None;
    }
    let remedy = if s.restart == Restart::Always {
        format!(
            "After setting `backend-specific: scm: {{ restart: on-failure }}` in the manifest, run `goetia daemon uninstall {id}` and `goetia daemon install` to apply them: `restart: always` is not supported for scm, so `install` refuses this service as it is"
        )
    } else {
        "Run `goetia daemon install` to apply them".to_string()
    };
    Some(format!(
        "daemon {:?} was registered by goetia {registered_by} and this goetia-shim is {running}: its SCM recovery actions may be missing, so it may not restart after a failure. {remedy}",
        s.common.name
    ))
}

/// Said when the daemon has no `logs:` and the machine's `%ProgramData%` could not be located, so no
/// default log file exists either.
const NO_LOG_FILE: &str =
    "no log file for this daemon's output (no `logs:`, and no default log directory); it is still being run";

/// A stdio assignment `build_command` expects never to fail: asserted in debug builds, logged in
/// release, where the daemon then runs with that stream unredirected.
fn stdio_set<T, E: std::error::Error + 'static>(log: &Log, id: &str, assigned: Result<T, E>) {
    if let Err(e) = assigned {
        debug_assert!(
            false,
            "a stdio assignment build_command expects to succeed failed: {}",
            Chain(&e)
        );
        log.failure(id, &format!("redirect the daemon's output: {}", Chain(&e)));
    }
}

/// Build the (unspawned) command for `spec`: argv, cwd, env, Job Object
/// containment, the stop settings `plan` decides ([`StopPlan::spawn_setup`]), and stdout+stderr merged into the
/// daemon's log file, which `os` opens. `log`
/// names that file when the spec does not, and reports a failed open or redirect.
///
/// A service with no `logs` writes the default log. A task with none sends its output to `NUL`
/// and says nothing of it: that is what such a task asked for.
pub(crate) fn build_command(spec: &DaemonSpec, plan: StopPlan, os: &dyn Os, log: &Log, id: &str) -> cosca::Command {
    let mut cmd = cosca::run(spec.common().command.iter().cloned());
    if let Some(cwd) = &spec.common().cwd {
        cmd.current_dir(cwd.clone());
    }
    cmd.envs(spec.common().env.clone());
    // Best effort, so a daemon no Job Object contains is spawned and then refused by `require_job_object`,
    // which records the incomplete stop. The strongest request would fail the spawn instead.
    cmd.contain_with(ContainMode::BestEffort);
    let setup = plan.spawn_setup();
    if let Some(own_group) = setup.process_group {
        cmd.new_process_group(own_group);
    }
    if let Some(event) = setup.ask_with {
        cmd.graceful_stop(move |target| cosca::stop::send_console_event_via_proxy(target, event));
    }

    let logs = spec.common().logs.as_deref();
    // `Err(None)`: no file, by design, or a failed open that `Log::open_failed` has told.
    let opened = match (spec, logs) {
        (DaemonSpec::Task(_), None) => Err(None),
        (_, logs) => match log.daemon_log(id, logs) {
            Some((path, kind)) => os.open_log(&path, kind).map_err(|e| {
                log.open_failed(id, &path, &e);
                None
            }),
            None => Err(Some(NO_LOG_FILE.to_string())),
        },
    };
    match opened {
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
        Err(message) => {
            if let Some(message) = message {
                log.failure(id, &message);
            }
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
