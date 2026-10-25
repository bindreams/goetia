//! SCM wiring: dispatch, control handler, status reporting, and the one pass that spawns the
//! daemon `goetia.yaml` describes, waits for it and reports how it ended.
//!
//! The shim never restarts the daemon. It reports the daemon's exit as the service's exit and
//! exits; SCM's recovery actions (`backend::scm::generate`) start a new shim, which reads the
//! installed spec afresh.

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::OsStrExt as _;
use std::sync::Arc;

use cosca::{Child, Containment, Fd, Stdio};
use goetia::backend::scm::exit_status::{
    ERROR_BAD_ARGUMENTS, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_INVALID_DATA, Reported,
};
use goetia::backend::scm::manager::read_spec_blob;
use goetia::spec::{DaemonSpec, Restart};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus,
};

use crate::logging;
use crate::stop_bus::{StopBus, Teardown, WaitOutcome, Waiter, kill_for_stop, terminate_failed_tree};

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
                status_handle,
                Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
            );
        }
        Err(e) => {
            logging::log_failure(id, &format!("decode metadata blob: {e}"));
            finish(
                id,
                status_handle,
                Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
            );
        }
    };

    log_version_skew(id, &blob.spec, &blob.version, goetia::version());
    let reported = run_daemon(&blob.spec, &stop_bus, id, || report_running(id, status_handle));
    finish(id, status_handle, reported);
}

/// Tell SCM the service stopped, then end the process. SCM restarts the service or not from
/// the status reported here, never from the process exit code.
fn finish(id: &str, status_handle: StatusHandle, reported: Reported) -> ! {
    report_stopped(id, status_handle, reported);
    std::process::exit(reported.process_exit_code());
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

/// Registered directly rather than through `windows-service`'s `register`, which frees its closure
/// after the first `Stop`: SCM can deliver a control after that, which would reach freed memory.
fn register_control_handler(id: &str, stop_bus: &Arc<StopBus>) -> std::io::Result<StatusHandle> {
    let name: Vec<u16> = OsStr::new(id).encode_wide().chain(std::iter::once(0)).collect();
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
fn report_running(id: &str, handle: StatusHandle) {
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
        logging::log_failure(id, &format!("report SERVICE_RUNNING to SCM: {e}"));
    }
}

fn report_stopped(id: &str, handle: StatusHandle, reported: Reported) {
    let status = stopped_status(reported);
    // SAFETY: `handle` is live for the life of the process, and `status` outlives the call.
    let ok = unsafe { SetServiceStatus(handle.0, &status) };
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        logging::log_failure(id, &format!("report SERVICE_STOPPED to SCM: {e}"));
    }
}

// One pass ============================================================================================================

/// Launch the daemon, report `RUNNING` through `on_running`, wait for the daemon to end or for a
/// stop, and say how it ended.
///
/// `on_running` is called exactly once, after the daemon is spawned and never when launching failed.
/// A daemon that cannot start is therefore `STOPPED` without ever having been `RUNNING`, with a
/// non-zero Win32 code or, when no Job Object contains it, the incomplete stop. `start_via_notify`
/// reads both as a failed start under every restart policy.
fn run_daemon(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, id: &str, on_running: impl FnOnce()) -> Reported {
    let (child, waiter) = match launch(spec, stop_bus, id) {
        Ok(launched) => launched,
        Err(reported) => return reported,
    };
    on_running();

    // `wait_for_child_or_stop` kills the tree on a stop or a failed wait, so only `DaemonMayRun`
    // leaves a daemon that may be running.
    match stop_bus.wait_for_child_or_stop(waiter, &child, id) {
        WaitOutcome::ChildExited => {
            let code = match exit_status(&child) {
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
            end_tree_before_failure(stop_bus.settle(exit_report(code)), &child, id)
        }
        // The failure is already logged; the daemon's tree was terminated, so it is the shim that
        // failed.
        WaitOutcome::WaitFailed { os_error } => stop_bus.settle(Reported::shim_failure(os_error)),
        WaitOutcome::Stopping => Reported::Clean,
        // Still reports STOPPED so the service stays stoppable and uninstallable.
        WaitOutcome::DaemonMayRun => Reported::incomplete_stop(),
    }
}

/// A failure SCM restarts is reported only once the tree the daemon left is terminated, so the
/// restart does not overlap it. If it cannot be terminated, SCM must not restart: the incomplete
/// stop. Anything that is not a failure (a clean exit, or a stop that won) is returned as it is.
///
/// "Terminated" is `kill_tree` succeeding on the Job Object `launch` requires: no thread of any
/// member executes after it. The kernel may still be releasing what they held, so a restarted
/// daemon can briefly find a resource its predecessor held (a port) and fail like any other start.
fn end_tree_before_failure(settled: Reported, child: &Child, id: &str) -> Reported {
    if settled == Reported::Clean || terminate_failed_tree(child, id) {
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

/// The exit status of the daemon the bus found exited, as its own function so a test can make it fail.
fn exit_status(child: &Child) -> Result<std::process::ExitStatus, cosca::error::Error> {
    #[cfg(test)]
    test_hook::exit_status()?;
    child.wait()
}

/// The daemon, running, job-contained and already being waited on — or why not, logged.
///
/// The [`Waiter`] thread is made before the daemon is spawned, so a daemon is never running with
/// nothing to wait on it. `build_command` sits between them but never launches anything.
///
/// The `Err` is what the service reports: the OS error of the failure, `ERROR_GEN_FAILURE`, or the
/// incomplete stop for a daemon no Job Object contains.
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
        Ok(child) => require_job_object(child, spec, id).map(|child| (Arc::new(child), waiter)),
        Err(e) => {
            logging::log_failure(id, &spawn_failed(&spec.command, &e));
            Err(Reported::shim_failure(os_error_of(&e)))
        }
    }
}

/// The daemon, if a Job Object contains it. `type: simple` is stopped, and restarted after a
/// failure, only by terminating its whole tree, which `cosca` can guarantee only with a job: when
/// assigning the job fails it falls back to a process-tree walk that cannot say whether a
/// descendant was killed. Any other containment is a start failure: the daemon is killed, but
/// that kill proves nothing about its descendants, so the record is the incomplete stop whatever
/// the kill returned. SCM never restarts over it, and `start` reads it as a failed start.
fn require_job_object(child: Child, spec: &DaemonSpec, id: &str) -> Result<Child, Reported> {
    let containment = containment_of(&child);
    if containment == Containment::JobObject {
        return Ok(child);
    }
    logging::log_failure(id, &not_job_contained(&spec.command, containment));
    // Dropping the child kills it again and waits for it, with no bound when the kill is refused,
    // so a child that could not be killed is let go: the record must still be reported.
    if kill_for_stop(&child, id) == Teardown::Failed {
        child.detach();
    }
    Err(Reported::incomplete_stop())
}

/// The containment `cosca` achieved, as its own function so a test can make it differ.
fn containment_of(child: &Child) -> Containment {
    #[cfg(test)]
    if let Some(forced) = test_hook::containment() {
        return forced;
    }
    child.containment()
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

fn not_job_contained(command: &[String], containment: Containment) -> String {
    format!(
        "the daemon {command:?} was contained by {containment}, not a job object: `type: simple` needs a job object to terminate its whole tree. The daemon ran from its launch, and no kill of it shows that its descendants are gone, so it is reported as possibly still running"
    )
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
    let child = cmd.spawn()?;
    #[cfg(test)]
    test_hook::spawned_child(&child);
    Ok(child)
}

/// A stdio assignment `build_command` expects never to fail: asserted in debug builds, logged in
/// release, where the daemon then runs with that stream unredirected.
fn stdio_set<T, E: std::fmt::Display>(id: &str, assigned: Result<T, E>) {
    if let Err(e) = assigned {
        debug_assert!(false, "a stdio assignment build_command expects to succeed failed: {e}");
        logging::log_failure(id, &format!("redirect the daemon's output: {e}"));
    }
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
            // stdout/stderr, so these are expected never to fail; `stdio_set`
            // asserts that in debug builds and logs it in release.
            stdio_set(id, cmd.stdout(Stdio::from_file(file)));
            stdio_set(id, cmd.stderr(Stdio::merge(Fd::STDOUT)));
        }
        Err(e) => {
            logging::log_failure(id, &log_open_failed(&log_path, &e));
            // Same reasoning as above, for `Stdio::null()`.
            stdio_set(id, cmd.stdout(Stdio::null()));
            stdio_set(id, cmd.stderr(Stdio::null()));
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

    use cosca::identity::ProcessId;
    use cosca::{Child, Containment};

    /// The error [`exit_status_fails`] makes the exit-status query fail with.
    pub(crate) const EXIT_STATUS_UNREADABLE: &str = "the exit status could not be read (test hook)";

    thread_local! {
        static CONTAINMENT: Cell<Option<Containment>> = const { Cell::new(None) };
        static SPAWNS: Cell<usize> = const { Cell::new(0) };
        static SPAWNED: Cell<Option<ProcessId>> = const { Cell::new(None) };
        static STATUS_FAILS: Cell<bool> = const { Cell::new(false) };
    }

    /// The identity of the daemon this thread spawned last, so a test can ask the OS whether it is
    /// still running after `run_daemon` has dropped its `Child`.
    pub(crate) fn spawned() -> Option<ProcessId> {
        SPAWNED.get()
    }

    pub(super) fn spawned_child(child: &Child) {
        SPAWNED.set(Some(child.id()));
    }

    /// Make this thread's spawned daemons read as contained by `containment`, until the returned
    /// guard drops. A job assignment cannot be made to fail on demand.
    pub(crate) fn containment_is(containment: Containment) -> Degraded {
        CONTAINMENT.set(Some(containment));
        Degraded
    }

    /// See [`containment_is`].
    pub(crate) struct Degraded;

    impl Drop for Degraded {
        fn drop(&mut self) {
            CONTAINMENT.set(None);
        }
    }

    pub(super) fn containment() -> Option<Containment> {
        CONTAINMENT.get()
    }

    /// Make the exit-status query on this thread fail, until the returned guard drops.
    pub(crate) fn exit_status_fails() -> Unreadable {
        STATUS_FAILS.set(true);
        Unreadable
    }

    /// See [`exit_status_fails`].
    pub(crate) struct Unreadable;

    impl Drop for Unreadable {
        fn drop(&mut self) {
            STATUS_FAILS.set(false);
        }
    }

    pub(super) fn exit_status() -> Result<(), cosca::error::Error> {
        if STATUS_FAILS.get() {
            return Err(cosca::error::Error::Io(std::io::Error::other(EXIT_STATUS_UNREADABLE)));
        }
        Ok(())
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
