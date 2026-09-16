//! The real SCM-backed [`ScmActor`](super::ScmActor). Raw `windows-sys`
//! FFI is sanctioned here: the alertable `SleepEx(INFINITE, TRUE)` wait
//! is a kernel rendezvous for an SCM-delivered APC, not a timeout-poll,
//! and `NotifyServiceStatusChangeW` has no `windows-service` wrapper.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows_service::service::{Service, ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE, WAIT_IO_COMPLETION};
use windows_sys::Win32::System::Services::{
    ControlService, NotifyServiceStatusChangeW, SERVICE_CONTROL_STOP, SERVICE_NOTIFY, SERVICE_NOTIFY_2W,
    SERVICE_NOTIFY_RUNNING, SERVICE_NOTIFY_START_PENDING, SERVICE_NOTIFY_STATUS_CHANGE, SERVICE_NOTIFY_STOP_PENDING,
    SERVICE_NOTIFY_STOPPED, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STOPPED,
};
use windows_sys::Win32::System::Threading::{INFINITE, SleepEx};

use super::{Deadline, Observed, QueriedState, Waited, WantState, already_running_is_recoverable};

/// `windows-service`'s `Error` does not implement `Into<io::Error>` for
/// every variant (it also carries parse errors this module never
/// produces), so this narrows to the one case that matters here.
fn to_io_error(e: windows_service::Error) -> io::Error {
    match e {
        windows_service::Error::Winapi(e) => e,
        other => io::Error::other(other),
    }
}

/// Receives the callback-reported current state across the `SleepEx`
/// wait. Heap-pinned (its address is handed to the SCM as `pContext`).
/// Atomics: the APC writes these from the kernel's callback context, so
/// the awaiting loop must not treat them as loop-invariant.
struct LastStatus {
    current_state: AtomicU32,
    fired: AtomicBool,
}

/// `NotifyServiceStatusChangeW` delivers the new status via an APC into
/// this callback. The SCM hands back the `SERVICE_NOTIFY_2W` buffer as
/// `pparameter`; we read its `pContext` (our `*mut LastStatus`) and copy
/// out the current state. Runs on the thread that issued the alertable
/// wait.
unsafe extern "system" fn notify_callback(pparameter: *const c_void) {
    let buf = pparameter as *const SERVICE_NOTIFY_2W;
    if buf.is_null() {
        return;
    }
    // SAFETY: the SCM passes back the exact buffer registered in `arm`,
    // whose `pContext` is the live `*mut LastStatus` pinned for the wait.
    let slot = unsafe { (*buf).pContext as *mut LastStatus };
    if slot.is_null() {
        return;
    }
    let state = unsafe { (*buf).ServiceStatus.dwCurrentState };
    unsafe {
        (*slot).current_state.store(state, Ordering::Release);
        (*slot).fired.store(true, Ordering::Release);
    }
}

/// The `NotifyServiceStatusChangeW` mask for `want`, given whether
/// `start()` has already been issued (`started`).
///
/// For a start wait the `STOPPED` bit is included ONLY after `start()`:
/// the service is `Stopped` at the initial arm (a stop wait always
/// precedes a start wait in this crate's own usage), and
/// `NotifyServiceStatusChangeW` immediate-fires on the current state — so
/// arming `STOPPED` before `start()` would misclassify that pre-start
/// `Stopped` as a failed start. After `start()` the service has entered
/// `StartPending`, so a later `StartPending -> Stopped` delivers a real
/// `Stopped` callback that terminates the wait with `Err`.
fn want_to_mask(want: WantState, started: bool) -> SERVICE_NOTIFY {
    match want {
        WantState::Stopped => SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_STOP_PENDING,
        WantState::Running if started => SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_START_PENDING,
        WantState::Running => SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_START_PENDING,
    }
}

/// `windows-service`'s state onto the ungated [`QueriedState`]. One arm
/// per variant and no `_` catch-all, so a variant added upstream is a
/// compile error here rather than a silent remap — only CI can execute
/// this, so it is written to be checkable by eye.
fn queried_state(state: ServiceState) -> QueriedState {
    match state {
        ServiceState::Stopped => QueriedState::Stopped,
        ServiceState::StartPending => QueriedState::StartPending,
        ServiceState::StopPending => QueriedState::StopPending,
        ServiceState::Running => QueriedState::Running,
        ServiceState::ContinuePending => QueriedState::ContinuePending,
        ServiceState::PausePending => QueriedState::PausePending,
        ServiceState::Paused => QueriedState::Paused,
    }
}

/// Owns both its `WinServiceManager` (SCM) connection and its per-service
/// [`Service`] handle (each `Drop` closes its own `SC_HANDLE`), plus the
/// notify buffer. The `LastStatus` slot and the `SERVICE_NOTIFY_2W`
/// buffer are heap-pinned (`Box`) so their addresses stay stable across
/// `arm` -> `SleepEx` -> callback. Both handles are `Option`s so
/// [`Self::close_and_drain`] can close them *before* it drains — see
/// there for why that order is the whole point.
///
/// Owning the SCM connection (rather than borrowing the caller's) is what
/// makes [`super::NotifyRegistrar::reopen`] able to follow `NotifyServiceStatusChangeW`'s
/// documented `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` remedy exactly: "close
/// the handle to the SCM, open a new handle, and call this function
/// again" — the lag condition is tracked against the SCM connection, not
/// the per-service handle alone.
pub struct SystemScmActor {
    scm: Option<WinServiceManager>,
    name: String,
    service: Option<Service>,
    status: Box<LastStatus>,
    notify: Box<SERVICE_NOTIFY_2W>,
    /// The state most recently awaited, for `want_to_mask`.
    awaiting: WantState,
    /// Whether `start()` has been issued. Gates the two-phase arm mask.
    started: bool,
    /// Whether the most recent `arm()` succeeded and has not yet been
    /// drained by `wait_callback`. See [`Self::close_and_drain`].
    registration_outstanding: bool,
}

impl SystemScmActor {
    /// Open `name` with `QUERY_STATUS | STOP | START` — everything both
    /// [`super::stop_via_notify`] and [`super::start_via_notify`] need,
    /// so one actor serves either wait without reopening.
    pub fn open(name: &str) -> io::Result<Self> {
        let scm = open_scm()?;
        let service = open_handle(&scm, name)?;
        Ok(Self {
            scm: Some(scm),
            name: name.to_string(),
            service: Some(service),
            status: Box::new(LastStatus {
                current_state: AtomicU32::new(0),
                fired: AtomicBool::new(false),
            }),
            notify: Box::default(),
            awaiting: WantState::Stopped,
            started: false,
            registration_outstanding: false,
        })
    }

    /// The open service handle, or an error once this actor has gone
    /// inert: an expired wait closes both handles there and then (see
    /// [`Self::close_and_drain`]), so a later call reports that instead
    /// of panicking on a `None`.
    fn service(&self) -> io::Result<&Service> {
        self.service.as_ref().ok_or_else(|| {
            io::Error::other("the SCM handles were closed when this wait expired; the actor cannot be reused")
        })
    }

    /// The bounded wait ran out. Close and drain *here*, while the boxes
    /// are provably alive and this is still the one place that knows a
    /// registration is outstanding which no further wait will consume.
    /// `registration_outstanding` stays true — nothing consumed this arm.
    fn expire(&mut self) -> io::Result<Option<Observed>> {
        self.close_and_drain();
        Ok(None)
    }

    /// Both teardown paths ([`Self::expire`] and `Drop`) go through
    /// [`super::close_then_drain`], which is where the order and the
    /// reason for it live.
    fn close_and_drain(&mut self) {
        super::close_then_drain(self);
    }
}

fn open_scm() -> io::Result<WinServiceManager> {
    WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(to_io_error)
}

fn open_handle(scm: &WinServiceManager, name: &str) -> io::Result<Service> {
    scm.open_service(
        name,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START,
    )
    .map_err(to_io_error)
}

/// The three steps [`super::close_then_drain`] puts in order.
impl super::ScmHandles for SystemScmActor {
    fn close_handles(&mut self) {
        drop(self.service.take());
        drop(self.scm.take());
    }

    fn registration_outstanding(&self) -> bool {
        self.registration_outstanding
    }

    fn drain_queued_apc(&mut self) {
        // SAFETY: a zero-duration alertable wait; no pointers involved.
        // `self.status`/`self.notify` are alive at both call sites —
        // `Drop::drop`'s body runs before any field is dropped.
        unsafe { SleepEx(0, 1) };
    }
}

impl Drop for SystemScmActor {
    fn drop(&mut self) {
        self.close_and_drain();
    }
}

/// The two steps [`super::register_until_deadline`] puts under the
/// caller's deadline.
impl super::NotifyRegistrar for SystemScmActor {
    fn register(&mut self) -> io::Result<u32> {
        let mask = want_to_mask(self.awaiting, self.started);
        // SAFETY: `self.service()?.raw_handle()` was opened with
        // `SERVICE_QUERY_STATUS` access (required by this API);
        // `self.notify` is heap-pinned and outlives every wait the arm
        // it registers can precede.
        Ok(unsafe { NotifyServiceStatusChangeW(self.service()?.raw_handle(), mask, &*self.notify) })
    }

    /// Reopen both handles — see the struct's own doc comment for why the
    /// SCM handle, not only the service handle, must be replaced.
    /// Assigning both only after both opens succeed means the old handles
    /// (closed via `Drop` when replaced) stay valid until their
    /// replacements are confirmed open.
    fn reopen(&mut self) -> io::Result<()> {
        let scm = open_scm()?;
        let service = open_handle(&scm, &self.name)?;
        self.scm = Some(scm);
        self.service = Some(service);
        Ok(())
    }
}

impl super::ScmActor for SystemScmActor {
    fn arm(&mut self, want: WantState, deadline: Deadline) -> io::Result<Waited> {
        self.awaiting = want;
        self.status.fired.store(false, Ordering::Release);
        *self.notify = SERVICE_NOTIFY_2W {
            dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
            pfnNotifyCallback: Some(notify_callback),
            pContext: (&mut *self.status as *mut LastStatus) as *mut c_void,
            ..Default::default()
        };
        let waited = super::register_until_deadline(self, deadline)?;
        if waited == Waited::Confirmed {
            // A notification may now be queued (immediately, if the
            // service already matched the mask) — see `close_then_drain`.
            self.registration_outstanding = true;
        }
        Ok(waited)
    }

    fn control_stop(&mut self) -> io::Result<()> {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: `self.service()?.raw_handle()` was opened with
        // `SERVICE_STOP` access; `status` is a valid, aligned out-param.
        let ok = unsafe { ControlService(self.service()?.raw_handle(), SERVICE_CONTROL_STOP, &mut status) };
        if ok != 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // The service stopped between the caller's early-return
            // query and this control. The STOPPED arm has already
            // queued the notification, so the wait still completes —
            // benign.
            Some(code) if code as u32 == ERROR_SERVICE_NOT_ACTIVE => Ok(()),
            _ => Err(err),
        }
    }

    fn start(&mut self) -> io::Result<()> {
        self.started = true;
        match self.service()?.start::<&std::ffi::OsStr>(&[]) {
            Ok(()) => Ok(()),
            // 1056 does not say which non-STOPPED state the service is
            // in, so query it and let `already_running_is_recoverable`
            // decide — that is where the reasoning lives, and it is
            // tested on every platform.
            //
            // Accepting a queried `Running` is right here and would be
            // WRONG for a `restart` that issued its start leg without a
            // confirmed `Stopped`: a `type: simple` service reads RUNNING
            // for its whole teardown (`goetia-shim` never reports
            // STOP_PENDING), so this arm would report a successful start
            // for a service that is about to go down and stay down.
            // `restart_does_not_start_after_a_stop_that_timed_out` and
            // `restart_with_no_budget_reports_a_rejected_start` in
            // `tests/cli_dispatch.rs` keep that true.
            Err(windows_service::Error::Winapi(e))
                if e.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING as i32) =>
            {
                match self.service()?.query_status() {
                    Ok(status) if already_running_is_recoverable(queried_state(status.current_state)) => Ok(()),
                    Ok(status) => Err(io::Error::other(format!(
                        "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING, but the service is actually \
                             {:?} — neither Running nor StartPending, so this wait cannot resolve from that state",
                        status.current_state
                    ))),
                    Err(query_err) => Err(to_io_error(query_err)),
                }
            }
            Err(e) => Err(to_io_error(e)),
        }
    }

    fn wait_callback(&mut self, deadline: Deadline) -> io::Result<Option<Observed>> {
        // Alertable wait: blocks until the SCM delivers the notify APC,
        // which runs `notify_callback` and sets `status.fired`, or until
        // the deadline. Still a kernel rendezvous, not MSDN's
        // `Sleep(dwWaitHint/10)` poll: nothing is re-*asked*, and the
        // bound is the human-facing one the caller set.
        loop {
            // `None` is unbounded — `INFINITE`, exactly as before there
            // was a deadline. `Some(0)` can only mean an already-spent
            // deadline, because `remaining_millis_capped` rounds up: a
            // live deadline never converts to zero, and `SleepEx(0)` in
            // a loop is the 100%-CPU poll this module exists to avoid.
            let remaining = deadline.remaining_millis_capped();
            if remaining == Some(0) {
                return self.expire();
            }
            // SAFETY: `SleepEx` takes no pointers; `1` (`TRUE`) makes it
            // alertable, which is the entire point of this wait.
            let rc = unsafe { SleepEx(remaining.unwrap_or(INFINITE), 1) };
            if self.status.fired.load(Ordering::Acquire) {
                // The notify APC ran — whether or not the interval also
                // elapsed on the way out, the state was delivered.
                break;
            }
            if rc != WAIT_IO_COMPLETION {
                // `SleepEx`'s only other return: the interval elapsed
                // with nothing delivered. `registration_outstanding`
                // stays true — nothing consumed this arm.
                return self.expire();
            }
            // An unrelated APC woke us; re-enter on what is left.
        }
        // The APC that `arm()` may have queued has now been delivered
        // and processed (that is what set `fired`) — nothing from this
        // registration remains outstanding. See `close_and_drain`.
        self.registration_outstanding = false;
        let state = self.status.current_state.load(Ordering::Acquire);
        Ok(Some(if state == SERVICE_RUNNING {
            Observed::Running
        } else if state == SERVICE_STOPPED {
            Observed::Stopped
        } else {
            Observed::Pending
        }))
    }
}
