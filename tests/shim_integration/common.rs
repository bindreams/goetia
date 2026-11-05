//! Shared helpers for the `type: simple` integration tests.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use goetia::backend::scm::exit_status::RecordedExit;
use goetia::backend::scm::manager::Scm;
use goetia::spec::{DaemonSpec, Kind};
use windows_service::service::{Service, ServiceAccess, ServiceState, ServiceStatus};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Services::{
    NotifyServiceStatusChangeW, SERVICE_NOTIFY_2W, SERVICE_NOTIFY_RUNNING, SERVICE_NOTIFY_STATUS_CHANGE,
    SERVICE_RUNNING,
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

/// Cleanup for a shim-hosted service `id`, in this order:
/// 1. the product's `uninstall`, which disables the service, waits out a start that has begun,
///    stops it and deletes it. Its wait has no bound: a shim that never stops is a hung test, which
///    the CI watchdog reports;
/// 2. a [`support::ServiceGuard`]'s `sc stop` and `sc delete`, for what `uninstall` does not own;
/// 3. [`LeftoverLog`]: the file the shim writes when it fails before it has read the daemon's config.
///
/// Step 1 is this type's `drop`; steps 2 and 3 are its fields, which drop in declaration order. A
/// failure prints a line and never panics; when nothing confirmed that no shim still writes the
/// log, an existing log is kept and named.
pub struct ShimGuard {
    id: String,
    _sc: support::ServiceGuard,
    log: LeftoverLog,
}

impl ShimGuard {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            _sc: support::ServiceGuard::new(id),
            log: LeftoverLog {
                id: id.to_string(),
                path: support::shim_default_log(&support::program_data(), id),
                fate: Fate::Remove,
            },
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Step 1, and what it decides for the log.
    fn uninstall(&self) -> Fate {
        use goetia::error::Error;
        use goetia::manager::ServiceManager as _;

        let fail = |step: &str, e: &Error| {
            eprintln!("ShimGuard[{}]: {step}: {}", self.id, e.report());
            Fate::Keep(format!("{step} failed"))
        };
        let id = match goetia::spec::Id::try_from(self.id.as_str()) {
            Ok(id) => id,
            Err(e) => return fail("parse the id", &e),
        };
        match Scm::new().uninstall(&id) {
            Ok(()) | Err(Error::NotInstalled { .. }) => Fate::Remove,
            // Not goetia's, so nothing confirms its shim stopped; the `sc` guard owns the service.
            Err(Error::Foreign { .. }) => Fate::Keep("the service is not goetia's".to_string()),
            Err(e) => fail("uninstall", &e),
        }
    }
}

impl Drop for ShimGuard {
    fn drop(&mut self) {
        self.log.fate = self.uninstall();
    }
}

/// What [`LeftoverLog`] does with its file.
enum Fate {
    Remove,
    /// Nothing confirmed that no shim still writes the file: `uninstall` failed, or the service is
    /// not goetia's.
    Keep(String),
}

/// The default log of the shim of `id`, which [`ShimGuard`]'s `drop` decides the [`Fate`] of.
struct LeftoverLog {
    id: String,
    path: PathBuf,
    fate: Fate,
}

impl Drop for LeftoverLog {
    fn drop(&mut self) {
        let problem = match &self.fate {
            Fate::Keep(why) => self
                .path
                .exists()
                .then(|| format!("left {}: {why}", self.path.display())),
            Fate::Remove => support::service_guard::remove_path_retrying(&self.path, false),
        };
        if let Some(problem) = problem {
            eprintln!("ShimGuard[{}]: {problem}", self.id);
        }
    }
}

/// A temp dir the test owns, for everything its daemons write: their logs, which the shims' own
/// failure lines join once they have read the spec.
///
/// Removed on drop, retried for [`support::service_guard::CLEANUP_BOUND`] and then let go with a
/// warning: a daemon may still hold its log while its service stops. Declare it before the test's
/// [`ShimGuard`]s.
pub struct LogDir(PathBuf);

impl LogDir {
    pub fn new() -> Self {
        // `keep` hands the removal to the `Drop` below, which does not ignore a failure.
        Self(tempfile::tempdir().expect("tempdir for the daemons' logs").keep())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Where the daemon `id` logs: `<dir>/<id>.log`.
    pub fn path_for(&self, id: &str) -> PathBuf {
        self.0.join(format!("{id}.log"))
    }
}

impl Drop for LogDir {
    fn drop(&mut self) {
        if let Some(problem) = support::service_guard::remove_path_retrying(&self.0, true) {
            eprintln!("LogDir: {problem}");
        }
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
            .unwrap_or_else(|| panic!("no numeric `{name}` in:\n{query}"))
    };
    RecordedExit {
        win32: field("WIN32_EXIT_CODE"),
        specific: field("SERVICE_EXIT_CODE"),
    }
}

/// `taskkill /F` on `pid`: the way a test ends a shim or a daemon without telling either.
pub fn kill_process(pid: u32) {
    support::cmd::run("taskkill.exe", &["/F", "/PID", &pid.to_string()]).expect_ok();
}

// SCM state-change notifications --------------------------------------------------------------------------------------

/// What one `NotifyServiceStatusChangeW` delivery said, written by the APC and read by the waiter
/// on the same thread.
struct Delivery {
    fired: AtomicBool,
    state: AtomicU32,
    controls_accepted: AtomicU32,
}

unsafe extern "system" fn record_delivery(parameter: *const c_void) {
    let notify = parameter.cast::<SERVICE_NOTIFY_2W>();
    // SAFETY: SCM hands back the buffer registered in `arm_and_wait`, whose `pContext` is the leaked
    // `Delivery`.
    let (delivery, status) = unsafe { (&*(*notify).pContext.cast::<Delivery>(), &(*notify).ServiceStatus) };
    delivery.state.store(status.dwCurrentState, Ordering::Release);
    delivery
        .controls_accepted
        .store(status.dwControlsAccepted, Ordering::Release);
    delivery.fired.store(true, Ordering::Release);
}

/// The SCM connection and the service handle of one registration, closed together when it ends.
struct Handles {
    _manager: ServiceManager,
    service: Service,
}

impl Handles {
    fn open(id: &str) -> std::io::Result<Self> {
        let io_error = |e: windows_service::Error| match e {
            windows_service::Error::Winapi(e) => e,
            other => std::io::Error::other(other),
        };
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(io_error)?;
        let service = manager
            .open_service(id, ServiceAccess::QUERY_STATUS)
            .map_err(io_error)?;
        Ok(Self {
            _manager: manager,
            service,
        })
    }
}

/// `NotifyServiceStatusChangeW`'s "you are too far behind" return; not among `windows-sys`'s exports.
const ERROR_SERVICE_NOTIFY_CLIENT_LAGGING: u32 = 1294;

/// Arm `mask` on service `id`; the returned closure blocks, alertably, until SCM delivers it and
/// returns `(state, controls_accepted)` of the delivery. Armed on a state that does not match `mask`
/// yet, the delivery is the transition; a service already in a matching state delivers at once.
/// There is no bound: a delivery that never comes is a hung test, which the CI watchdog reports.
///
/// On `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` it reopens the SCM and service handles and arms again,
/// as the documented remedy and `ScmActor::arm` do. Any other registration failure (a deleted
/// service reports 1060) is returned.
pub fn arm_and_wait(id: &str, mask: u32) -> std::io::Result<impl FnOnce() -> (u32, u32)> {
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
    let mut handles = Handles::open(id)?;
    loop {
        // SAFETY: the handle has `SERVICE_QUERY_STATUS`; `notify` is leaked and so outlives the
        // registration.
        let error = unsafe { NotifyServiceStatusChangeW(handles.service.raw_handle(), mask, notify) };
        match error {
            0 => break,
            ERROR_SERVICE_NOTIFY_CLIENT_LAGGING => handles = Handles::open(id)?,
            error => return Err(std::io::Error::from_raw_os_error(error as i32)),
        }
    }
    // The handles cancel the registration when they close, so the closure owns them.
    Ok(move || {
        let _handles = handles;
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
        let (state, _) = arm_and_wait(id, SERVICE_NOTIFY_RUNNING).expect("arm the RUNNING notification")();
        assert_eq!(state, SERVICE_RUNNING, "the notification for RUNNING");
        let running = service
            .query_status()
            .unwrap_or_else(|e| panic!("query status of {id}: {e}"));
        assert_eq!(running.current_state, ServiceState::Running, "{running:?}");
        let pid = running
            .process_id
            .unwrap_or_else(|| panic!("SCM reports a running service with no process id: {running:?}"));

        // SAFETY: plain FFI call; the returned handle is checked and then owned.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            panic!("OpenProcess({pid}): {}", std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a valid handle we own and nothing else closes.
        let process = unsafe { OwnedHandle::from_raw_handle(raw as _) };

        // The handle must name the shim that was running, not a recycled pid.
        let again = scm_status(id);
        assert!(
            matches!(again.current_state, ServiceState::Running | ServiceState::StartPending),
            "{again:?}"
        );
        assert_eq!(again.process_id, Some(pid), "{again:?}");
        Self { process, pid }
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
