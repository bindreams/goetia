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
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE, WAIT_OBJECT_0};
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
/// `%ProgramData%\Goetia\logs\` default. Declared before the test's `ServiceGuard`s, so that it
/// drops after the services.
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
    assert_eq!(
        error,
        0,
        "NotifyServiceStatusChangeW: {}",
        std::io::Error::from_raw_os_error(error as i32)
    );
    move || {
        while !delivery.fired.load(Ordering::Acquire) {
            // SAFETY: plain FFI call; an alertable wait is what lets the APC run.
            unsafe { SleepEx(INFINITE, 1) };
        }
        (
            delivery.state.load(Ordering::Acquire),
            delivery.controls_accepted.load(Ordering::Acquire),
        )
    }
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

/// Removes the fallback log of `id` when dropped, on every path including a panic, and only once no
/// shim can write it: the drop stops the service if it runs, then waits for the exit of its shim and
/// of every shim [`Self::adopt`]ed. A never-started service wrote no log, which is not a failure.
///
/// Build it as soon as the test knows `id`, after `id`'s `ServiceGuard`: it drops first, while the
/// service record still names the shim. A problem is printed and the log is left in place.
pub struct FallbackLog {
    id: String,
    shims: Vec<ShimProcess>,
}

impl FallbackLog {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            shims: Vec::new(),
        }
    }

    /// Also waits for `shim`, one the test attached, which the drop would not find if SCM restarted
    /// the service.
    pub fn adopt(&mut self, shim: ShimProcess) {
        self.shims.push(shim);
    }

    /// Stops the service if it runs and returns once every shim of it is gone. The wait is on the
    /// process handles, so it needs no bound: a shim that never exits is a hung test.
    fn reap_shims(&mut self) -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| format!("connect to the service manager: {e}"))?;
        match manager.open_service(&self.id, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) {
            // Never installed, so no shim of it ever ran.
            Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) => {
            }
            Err(e) => return Err(format!("open the service: {e}")),
            Ok(service) => {
                if let Some(shim) = ShimProcess::running(&service)? {
                    // A refusal means the service is stopping or stopped; either way the wait below
                    // is for the process itself.
                    let _ = service.stop();
                    self.shims.push(shim);
                }
            }
        }
        for shim in self.shims.drain(..) {
            shim.wait_for_exit();
        }
        Ok(())
    }
}

impl Drop for FallbackLog {
    fn drop(&mut self) {
        if let Err(problem) = self.reap_shims() {
            eprintln!("FallbackLog[{}]: {problem}; its log is left in place", self.id);
            return;
        }
        let path = fallback_log_path(&self.id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("FallbackLog[{}]: remove {}: {e}", self.id, path.display()),
        }
    }
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

        Self::hold(&service, pid)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("{id} no longer reports pid {pid} as RUNNING"))
    }

    /// The shim of `service` while it is `RUNNING`, held, or `None` once it is `STOPPED`. A pending
    /// state is waited out first, since `windows-service` reports a pid only in `RUNNING`.
    fn running(service: &Service) -> Result<Option<Self>, String> {
        loop {
            let status = service.query_status().map_err(|e| format!("query the service: {e}"))?;
            match status.current_state {
                ServiceState::Stopped => return Ok(None),
                ServiceState::Running => {
                    let pid = status
                        .process_id
                        .ok_or_else(|| format!("SCM reports a running service with no process id: {status:?}"))?;
                    if let Some(shim) = Self::hold(service, pid)? {
                        return Ok(Some(shim));
                    }
                    // The process exited before SCM recorded it: read again.
                    std::thread::yield_now();
                }
                _ => {
                    arm_and_wait(service, SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED)();
                }
            }
        }
    }

    /// Holds the process `pid`, which `service` reported as its `RUNNING` process. `None` if there
    /// is no such process, or SCM no longer reports `pid` as `RUNNING` once it is held.
    fn hold(service: &Service, pid: u32) -> Result<Option<Self>, String> {
        // SAFETY: plain FFI call; the returned handle is checked and then owned.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(code) if code == ERROR_INVALID_PARAMETER as i32 => Ok(None),
                _ => Err(format!("OpenProcess({pid}): {e}")),
            };
        }
        // SAFETY: `raw` is a valid handle we own and nothing else closes.
        let process = unsafe { OwnedHandle::from_raw_handle(raw as _) };

        // The handle must name the shim that was running, not a recycled pid.
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
        // SAFETY: `process` is a live handle with SYNCHRONIZE access.
        let waited = unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            panic!(
                "WaitForSingleObject on the shim (pid {}) returned {waited:#x}, GetLastError: {}",
                self.pid,
                std::io::Error::last_os_error()
            );
        }
    }
}
