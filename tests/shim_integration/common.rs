//! Shared helpers for the `type: simple` integration tests.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use goetia::backend::scm::exit_status::RecordedExit;
use goetia::spec::{DaemonSpec, Kind};
use tempfile::TempDir;
use windows_service::service::{Service, ServiceAccess, ServiceState, ServiceStatus};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Services::{
    NotifyServiceStatusChangeW, SERVICE_NOTIFY_2W, SERVICE_NOTIFY_RUNNING, SERVICE_NOTIFY_STATUS_CHANGE,
    SERVICE_NOTIFY_STOPPED, SERVICE_RUNNING,
};
use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, SleepEx, WaitForSingleObject};

use crate::fixture;
use crate::support;

pub fn fixture_command(mode: &str, args: &[&str]) -> Vec<String> {
    let mut v = vec![
        support::current_exe_str(),
        fixture::FIXTURE.to_string(),
        mode.to_string(),
    ];
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

/// A temp dir the test owns, for its daemons' logs: no shim test writes to the
/// `%ProgramData%\Goetia\logs\` default.
pub struct LogDir(TempDir);

impl LogDir {
    pub fn new() -> Self {
        Self(tempfile::tempdir().expect("tempdir for the daemons' logs"))
    }

    /// Where the daemon `id` logs: `<dir>/<id>.log`.
    pub fn path_for(&self, id: &str) -> PathBuf {
        self.0.path().join(format!("{id}.log"))
    }
}

/// [`support::spec`] with `type: simple`, logging to a path in `logs` (a dir the test owns) rather
/// than the `%ProgramData%` default.
pub fn simple_spec(id: &str, command: Vec<String>, logs: &LogDir) -> DaemonSpec {
    DaemonSpec {
        kind: Kind::Simple,
        logs: Some(logs.path_for(id)),
        ..support::spec(id, command)
    }
}

/// The `mk` [`goetia::manager::conformance::run`] needs: a fresh, valid `type: simple` spec for any
/// `id`, logging to `logs.path_for(id)`. The port is dummy — the fixture reports in on a best-effort
/// basis regardless of whether anything listens, and no conformance scenario reads the report.
pub fn conformance_mk(logs: &LogDir, id: &str) -> DaemonSpec {
    simple_spec(id, fixture_command("report", &["1"]), logs)
}

// SCM records and the shim process ====================================================================================

/// SCM's own record of `id`: state, exit code, and the pid of the process hosting it.
pub fn scm_status(id: &str) -> ServiceStatus {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .expect("connect to the service manager");
    manager
        .open_service(id, ServiceAccess::QUERY_STATUS)
        .unwrap_or_else(|e| panic!("open service {id}: {e}"))
        .query_status()
        .unwrap_or_else(|e| panic!("query status of {id}: {e}"))
}

/// What one `NotifyServiceStatusChangeW` delivery said, written by the APC and read by the waiter
/// on the same thread.
struct Delivery {
    fired: AtomicBool,
    state: AtomicU32,
    controls_accepted: AtomicU32,
}

unsafe extern "system" fn record_delivery(parameter: *const c_void) {
    let notify = parameter.cast::<SERVICE_NOTIFY_2W>();
    // SAFETY: SCM hands back the buffer registered in `arm`, whose `pContext` is the leaked
    // `Delivery`.
    let (delivery, status) = unsafe { (&*(*notify).pContext.cast::<Delivery>(), &(*notify).ServiceStatus) };
    delivery.state.store(status.dwCurrentState, Ordering::Release);
    delivery
        .controls_accepted
        .store(status.dwControlsAccepted, Ordering::Release);
    delivery.fired.store(true, Ordering::Release);
}

/// Arm `mask` on `service` and block, alertably, until SCM delivers it. Returns
/// `(state, controls_accepted)` of the delivery. Armed on a state that does not match `mask` yet,
/// so the delivery is the transition.
pub fn arm_and_wait(service: &Service, mask: u32) -> impl FnOnce() -> (u32, u32) {
    try_arm_and_wait(service, mask).unwrap_or_else(|e| panic!("{e}"))
}

/// [`arm_and_wait`] that reports a failed registration instead of panicking.
pub fn try_arm_and_wait(service: &Service, mask: u32) -> Result<impl FnOnce() -> (u32, u32), String> {
    // Leaked: SCM may still hold the registration if the test panics before the delivery.
    let delivery: &'static Delivery = Box::leak(Box::new(Delivery {
        fired: AtomicBool::new(false),
        state: AtomicU32::new(0),
        controls_accepted: AtomicU32::new(0),
    }));
    let notify: &'static mut SERVICE_NOTIFY_2W = Box::leak(Box::new(SERVICE_NOTIFY_2W {
        dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
        pfnNotifyCallback: Some(record_delivery),
        pContext: (delivery as *const Delivery as *mut Delivery).cast(),
        ..Default::default()
    }));
    // SAFETY: the handle has `SERVICE_QUERY_STATUS`; `notify` is leaked and so outlives the
    // registration.
    let error = unsafe { NotifyServiceStatusChangeW(service.raw_handle(), mask, notify) };
    if error != 0 {
        return Err(format!(
            "NotifyServiceStatusChangeW: {}",
            std::io::Error::from_raw_os_error(error as i32)
        ));
    }
    Ok(move || {
        while !delivery.fired.load(Ordering::Acquire) {
            // SAFETY: plain FFI call; an alertable wait is what lets the APC run.
            unsafe { SleepEx(INFINITE, 1) };
        }
        (
            delivery.state.load(Ordering::Acquire),
            delivery.controls_accepted.load(Ordering::Acquire),
        )
    })
}

/// Both exit codes SCM holds for `id`, from `sc query`. `ServiceStatus::exit_code` cannot show a
/// service-specific code beside Win32 0, which is exactly what tells a clean stop from an
/// incomplete one.
pub fn recorded_exit(id: &str) -> RecordedExit {
    let query = support::cmd::run("sc.exe", &["query", id]).expect_ok();
    let field = |name: &str| -> u32 {
        query
            .stdout
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == name).then(|| value.split_whitespace().next()?.parse().ok())?
            })
            .unwrap_or_else(|| {
                panic!(
                    "no numeric `{name}` in:
{query}"
                )
            })
    };
    RecordedExit {
        win32: field("WIN32_EXIT_CODE"),
        specific: field("SERVICE_EXIT_CODE"),
    }
}

/// The shim's fallback log for `id`, `%ProgramData%\Goetia\logs\<id>.log`: where it reports what it
/// cannot report to the daemon's own log, and the daemon's output when the spec sets no `logs`.
pub fn fallback_log(id: &str) -> String {
    let path = fallback_log_path(id);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fallback log {}: {e}", path.display()))
}

fn fallback_log_path(id: &str) -> PathBuf {
    let programdata = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    programdata.join("Goetia").join("logs").join(format!("{id}.log"))
}

/// Owns the whole teardown of the service `id` and its shim. Build it as soon as the test knows
/// `id`, before the write that creates the service.
///
/// The drop never panics. In order, each failure printed as `ShimGuard[id]: <step>: <error>`:
/// 1. Open the service. If it does not exist, skip 2 and the deletion in 4.
/// 2. Stop it. A service that cannot take a stop yet (`START_PENDING`) is waited on for `RUNNING` or
///    `STOPPED` and sent the stop again, until one is accepted or the service is not active. Any
///    other error, or a shim that was not identified before the stop, leaves the service and the
///    log in place. The shim that was running is held before the stop is sent: `windows-service`
///    reports a pid only in `RUNNING`.
/// 3. Wait for the exit of that shim and of every [`Self::adopt`]ed one.
/// 4. Delete the service, then remove the shim's fallback log.
///
/// Every wait follows an accepted stop, so it has an end and needs no bound.
///
/// Declare the test's [`LogDir`] before the guard: it then drops after the shim and the daemon are
/// gone, while the daemon still holds its log. A test that uninstalls the service itself must
/// `adopt` its shim first, since the drop cannot find the shim of a service that is gone.
pub struct ShimGuard {
    id: String,
    shims: Vec<ShimProcess>,
}

impl ShimGuard {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            shims: Vec::new(),
        }
    }

    /// Also waits for `shim`, one the test attached, which the drop would not find if SCM restarted
    /// the service or the test uninstalled it.
    pub fn adopt(&mut self, shim: ShimProcess) {
        self.shims.push(shim);
    }

    fn report(&self, step: &str, error: &dyn std::fmt::Display) {
        eprintln!("ShimGuard[{}]: {step}: {error}", self.id);
    }
}

impl Drop for ShimGuard {
    fn drop(&mut self) {
        let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
            Ok(manager) => manager,
            Err(e) => return self.report("connect to the service manager", &e),
        };
        let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
        let service = match manager.open_service(&self.id, access) {
            Ok(service) => Some(service),
            // Never installed, or the test uninstalled it.
            Err(e) if win32_code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => None,
            Err(e) => return self.report("open the service", &e),
        };
        if let Some(service) = &service {
            match stop_and_hold_shim(service) {
                Ok(shim) => self.shims.extend(shim),
                Err(e) => return self.report("stop the service", &e),
            }
        }
        for shim in std::mem::take(&mut self.shims) {
            if let Err(e) = shim.try_wait_for_exit() {
                return self.report("wait for the shim", &e);
            }
        }
        if let Some(service) = service {
            match service.delete() {
                Ok(()) => {}
                // The record is only marked for deletion while a handle to it is open; it goes with
                // the last one.
                Err(e)
                    if matches!(
                        win32_code(&e),
                        Some(ERROR_SERVICE_DOES_NOT_EXIST | ERROR_SERVICE_MARKED_FOR_DELETE)
                    ) => {}
                Err(e) => self.report("delete the service", &e),
            }
        }
        if let Some(problem) = support::service_guard::remove_path(&fallback_log_path(&self.id), false) {
            eprintln!("ShimGuard[{}]: {problem}", self.id);
        }
    }
}

fn win32_code(error: &windows_service::Error) -> Option<u32> {
    match error {
        windows_service::Error::Winapi(e) => e.raw_os_error().map(|code| code as u32),
        _ => None,
    }
}

/// Sends `service` a stop and returns the shim that was running just before it, if one was. Fails
/// if the stop could not be sent, or was accepted with no shim identified while the service still
/// ran: waiting for a process nobody holds could end too early.
fn stop_and_hold_shim(service: &Service) -> Result<Option<ShimProcess>, String> {
    let mut shim = None;
    let stopped = loop {
        shim = ShimProcess::running(service)?.or(shim);
        match service.stop() {
            Ok(_) => break false,
            Err(e) if win32_code(&e) == Some(ERROR_SERVICE_NOT_ACTIVE) => break true,
            Err(e) if win32_code(&e) == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL) => {
                let status = service.query_status().map_err(|e| format!("query the service: {e}"))?;
                match status.current_state {
                    ServiceState::Stopped => break true,
                    // A stop is already in flight.
                    ServiceState::StopPending => break false,
                    // It reached `RUNNING` after the refusal: read the shim and send the stop again.
                    ServiceState::Running => {}
                    _ => {
                        try_arm_and_wait(service, SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED)?();
                    }
                }
            }
            Err(e) => return Err(format!("send the stop: {e}")),
        }
    };
    if shim.is_none() && !stopped {
        return Err("the stop was accepted, but the shim was not running when it was read".to_string());
    }
    Ok(shim)
}

/// `taskkill /F` on `pid`: the way a test ends a shim or a daemon without telling either.
pub fn kill_process(pid: u32) {
    support::cmd::run("taskkill.exe", &["/F", "/PID", &pid.to_string()]).expect_ok();
}

/// A handle on the shim process of a started service, taken while it runs so that the test can
/// wait for the process to be gone. SCM has the final `STOPPED` record by then: the shim reports it
/// before it exits.
///
/// An open handle also keeps the pid from being reused, so a test that holds the handles of the
/// shims it has seen can tell a new shim from an old one by pid.
pub struct ShimProcess {
    process: OwnedHandle,
    pid: u32,
}

impl ShimProcess {
    /// Waits for `id` to report `RUNNING`, then holds its shim. `windows-service` reports a
    /// process id only in `Running`, and nothing orders the shim's report before the daemon's
    /// connect-back, so the status is read after the notification, never before it.
    pub fn attach(id: &str) -> Self {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .expect("connect to the service manager");
        let service = manager
            .open_service(id, ServiceAccess::QUERY_STATUS)
            .unwrap_or_else(|e| panic!("open service {id}: {e}"));
        // Armed before the read; a service that already runs delivers at once.
        let (state, _) = arm_and_wait(&service, SERVICE_NOTIFY_RUNNING)();
        assert_eq!(state, SERVICE_RUNNING, "the notification for RUNNING");
        let running = service
            .query_status()
            .unwrap_or_else(|e| panic!("query status of {id}: {e}"));
        assert_eq!(running.current_state, ServiceState::Running, "{running:?}");
        let pid = running
            .process_id
            .unwrap_or_else(|| panic!("SCM reports a running service with no process id: {running:?}"));

        let process = open_process(pid).unwrap_or_else(|e| panic!("OpenProcess({pid}): {e}"));
        Self::confirm(&service, pid, process)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("{id} no longer reports pid {pid} as RUNNING"))
    }

    /// The shim of `service` while it is `RUNNING`, held. `None` if the service is in any other
    /// state, or if its shim has exited while SCM still says `RUNNING`: no wait on SCM catching up,
    /// since a restart in between would leave it waiting for the next stop.
    fn running(service: &Service) -> Result<Option<Self>, String> {
        loop {
            let status = service.query_status().map_err(|e| format!("query the service: {e}"))?;
            if status.current_state != ServiceState::Running {
                return Ok(None);
            }
            let pid = status
                .process_id
                .ok_or_else(|| format!("SCM reports a running service with no process id: {status:?}"))?;
            match open_process(pid) {
                Ok(process) => {
                    if let Some(shim) = Self::confirm(service, pid, process)? {
                        return Ok(Some(shim));
                    }
                    // SCM moved on while the process was opened: read it again.
                }
                Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => return Ok(None),
                Err(e) => return Err(format!("OpenProcess({pid}): {e}")),
            }
        }
    }

    /// Holds `process`, which `service` reported as its `RUNNING` process `pid`. `None` if SCM no
    /// longer reports `pid` as `RUNNING`: the handle must name the shim that was running, not a
    /// recycled pid.
    fn confirm(service: &Service, pid: u32, process: OwnedHandle) -> Result<Option<Self>, String> {
        let again = service.query_status().map_err(|e| format!("query the service: {e}"))?;
        Ok(
            (again.current_state == ServiceState::Running && again.process_id == Some(pid))
                .then_some(Self { process, pid }),
        )
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Blocks until the shim process has exited. No bound: a shim that never exits is a hung test,
    /// which the CI watchdog reports.
    pub fn wait_for_exit(self) {
        self.try_wait_for_exit().unwrap_or_else(|e| panic!("{e}"));
    }

    /// [`Self::wait_for_exit`] that reports a failed wait instead of panicking.
    pub fn try_wait_for_exit(self) -> Result<(), String> {
        // SAFETY: `process` is a live handle with SYNCHRONIZE access.
        let waited = unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err(format!(
                "WaitForSingleObject on the shim (pid {}) returned {waited:#x}, GetLastError: {}",
                self.pid,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

/// A handle on the process `pid` that can be waited on.
fn open_process(pid: u32) -> std::io::Result<OwnedHandle> {
    // SAFETY: plain FFI call; the returned handle is checked and then owned.
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a valid handle we own and nothing else closes.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw as _) })
}
