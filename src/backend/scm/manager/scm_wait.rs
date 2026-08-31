//! Event-driven waits for SCM state transitions via `NotifyServiceStatusChangeW`
//! — a real kernel rendezvous (an APC delivered on the actual state
//! transition, awaited in `SleepEx(INFINITE, TRUE)`), not a poll loop.
//!
//! `windows-service`'s `stop()`/`start()` return as soon as SCM has accepted
//! the request — `StartServiceW` returns once the service's `ServiceMain`
//! thread has been created, not once it reports `SERVICE_RUNNING` — and the
//! crate wraps no wait primitive of its own. The MSDN checkpoint/`dwWaitHint`
//! pattern (`QueryServiceStatusEx` in a `Sleep` loop with a hardcoded bound)
//! is exactly the sleep-poll this project forbids. This module is a
//! near-verbatim port of `~/src/hole/crates/bridge/src/cutover/scm_wait.rs`
//! (translated from the `windows` crate to `windows-sys`, and adapted to
//! reuse `windows-service`'s already-open [`Service`] handle — whose `Drop`
//! already closes the underlying `SC_HANDLE` — instead of managing raw
//! `OpenSCManagerW`/`OpenServiceW` handles by hand): the orchestration below
//! is a pure state machine over [`ScmActor`], unit-tested with a fake in
//! `scm_wait_tests.rs`; [`system::SystemScmActor`] drives the real SCM.

use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WantState {
    Stopped,
    Running,
}

/// The service state a `wait_callback` observed. Distinct from [`WantState`]:
/// a callback can report a state that is neither what the caller wants nor
/// its opposite. `Running`/`Stopped` are terminal; `Pending` is any
/// intermediate (`StartPending`/`StopPending`) and re-arms.
/// [`start_via_notify`] treats a terminal `Stopped` as a *failed* start (the
/// service stopped instead of reaching `Running`), returning `Err` rather
/// than blocking forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    Running,
    Stopped,
    Pending,
}

/// The granular SCM operations [`stop_via_notify`]/[`start_via_notify`] need,
/// isolated so the ordering can be unit-tested with a fake rather than a real
/// service.
pub trait ScmActor {
    /// Register a status-change notification for `want`.
    /// `NotifyServiceStatusChangeW` is single-shot, so the sequence re-arms
    /// after every non-terminal callback.
    fn arm(&mut self, want: WantState) -> io::Result<()>;
    fn control_stop(&mut self) -> io::Result<()>;
    fn start(&mut self) -> io::Result<()>;
    /// Block in an alertable wait until the armed notification fires; return
    /// the service's observed state from the callback buffer.
    fn wait_callback(&mut self) -> io::Result<Observed>;
}

/// Stop the service, gated strictly on a real `STOPPED` callback from
/// `NotifyServiceStatusChangeW`; re-arms after a non-terminal (pending)
/// callback.
pub fn stop_via_notify<A: ScmActor>(a: &mut A) -> io::Result<()> {
    a.arm(WantState::Stopped)?;
    a.control_stop()?;
    loop {
        match a.wait_callback()? {
            Observed::Stopped => return Ok(()),
            // Running/Pending are non-terminal for a stop wait — re-arm and wait.
            Observed::Running | Observed::Pending => a.arm(WantState::Stopped)?,
        }
    }
}

/// Start the service, gated strictly on a real `RUNNING` callback; re-arms
/// after a non-terminal callback.
///
/// Critical ordering: arm `RUNNING` strictly BEFORE issuing `start`, else the
/// service can reach `RUNNING` before the arm and the notification only
/// fires on the *next* entry into `RUNNING` — a hang.
pub fn start_via_notify<A: ScmActor>(a: &mut A) -> io::Result<()> {
    a.arm(WantState::Running)?;
    a.start()?;
    loop {
        match a.wait_callback()? {
            Observed::Running => return Ok(()),
            // A terminal Stopped means the service stopped instead of
            // reaching Running — a failed start.
            Observed::Stopped => {
                return Err(io::Error::other(
                    "service stopped before reaching Running (failed start)",
                ));
            }
            Observed::Pending => a.arm(WantState::Running)?,
        }
    }
}

// system ==============================================================================================================

#[cfg(windows)]
pub mod system {
    //! The real SCM-backed [`ScmActor`](super::ScmActor). Raw `windows-sys`
    //! FFI is sanctioned here: the alertable `SleepEx(INFINITE, TRUE)` wait
    //! is a kernel rendezvous for an SCM-delivered APC, not a timeout-poll,
    //! and `NotifyServiceStatusChangeW` has no `windows-service` wrapper.

    use std::ffi::c_void;
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use windows_service::service::{Service, ServiceAccess};
    use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE};
    use windows_sys::Win32::System::Services::{
        ControlService, NotifyServiceStatusChangeW, SERVICE_CONTROL_STOP, SERVICE_NOTIFY, SERVICE_NOTIFY_2W,
        SERVICE_NOTIFY_RUNNING, SERVICE_NOTIFY_START_PENDING, SERVICE_NOTIFY_STATUS_CHANGE,
        SERVICE_NOTIFY_STOP_PENDING, SERVICE_NOTIFY_STOPPED, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STOPPED,
    };
    use windows_sys::Win32::System::Threading::SleepEx;

    use super::{Observed, WantState};

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
            WantState::Running if started => {
                SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_START_PENDING
            }
            WantState::Running => SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_START_PENDING,
        }
    }

    /// Owns both its `WinServiceManager` (SCM) connection and its per-service
    /// [`Service`] handle (each `Drop` closes its own `SC_HANDLE`), plus the
    /// notify buffer. The `LastStatus` slot and the `SERVICE_NOTIFY_2W`
    /// buffer are heap-pinned (`Box`) so their addresses stay stable across
    /// `arm` -> `SleepEx` -> callback.
    ///
    /// Owning the SCM connection (rather than borrowing the caller's) is
    /// what makes [`Self::reopen`] able to follow `NotifyServiceStatusChangeW`'s
    /// documented `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` remedy exactly: "close
    /// the handle to the SCM, open a new handle, and call this function
    /// again" — the lag condition is tracked against the SCM connection, not
    /// the per-service handle alone.
    pub struct SystemScmActor {
        scm: WinServiceManager,
        name: String,
        service: Service,
        status: Box<LastStatus>,
        notify: Box<SERVICE_NOTIFY_2W>,
        /// The state most recently awaited, for `want_to_mask`.
        awaiting: WantState,
        /// Whether `start()` has been issued. Gates the two-phase arm mask.
        started: bool,
        /// Whether the most recent `arm()` succeeded and has not yet been
        /// drained by `wait_callback`. See `Drop`.
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
                scm,
                name: name.to_string(),
                service,
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

        /// Reopen both handles. Used on `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`
        /// — see the struct's own doc comment for why the SCM handle, not
        /// only the service handle, must be replaced. Assigning `self.scm`/
        /// `self.service` only after each `open_*` call succeeds means the
        /// old handles (closed via `Drop` when replaced) stay valid until
        /// their replacements are confirmed open.
        fn reopen(&mut self) -> io::Result<()> {
            self.scm = open_scm()?;
            self.service = open_handle(&self.scm, &self.name)?;
            Ok(())
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

    impl Drop for SystemScmActor {
        fn drop(&mut self) {
            // An `arm()` that immediate-fired (the service already matched
            // the requested mask at the moment of the call) queues an APC to
            // THIS thread regardless of what happens next — closing the
            // service handle only stops *future* notifications from being
            // queued (per `NotifyServiceStatusChangeW`'s documented
            // remarks), it does not un-queue one already pending. If a later
            // step (`control_stop`/`start`) then errors before
            // `wait_callback`'s own alertable wait ever runs and drains it,
            // that APC is still outstanding — and would otherwise fire
            // later, on some future alertable wait elsewhere in the process,
            // against the `LastStatus`/`SERVICE_NOTIFY_2W` boxes this drop
            // is about to free. Drain it here, while they are still valid.
            if self.registration_outstanding {
                // SAFETY: a zero-duration alertable wait; no pointers
                // involved. `self.status`/`self.notify` are still live at
                // this point in `drop` (Rust drops fields only after this
                // method returns).
                unsafe { SleepEx(0, 1) };
            }
        }
    }

    impl super::ScmActor for SystemScmActor {
        fn arm(&mut self, want: WantState) -> io::Result<()> {
            self.awaiting = want;
            self.status.fired.store(false, Ordering::Release);
            *self.notify = SERVICE_NOTIFY_2W {
                dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
                pfnNotifyCallback: Some(notify_callback),
                pContext: (&mut *self.status as *mut LastStatus) as *mut c_void,
                ..Default::default()
            };
            let mask = want_to_mask(want, self.started);
            loop {
                // SAFETY: `self.service.raw_handle()` was opened with
                // `SERVICE_QUERY_STATUS` access (required by this API);
                // `self.notify` is heap-pinned and outlives every wait this
                // arm can precede.
                let rc = unsafe { NotifyServiceStatusChangeW(self.service.raw_handle(), mask, &*self.notify) };
                if rc == 0 {
                    // A notification may now be queued (immediately, if the
                    // service already matched `mask`) — see `Drop`.
                    self.registration_outstanding = true;
                    return Ok(());
                }
                const ERROR_SERVICE_NOTIFY_CLIENT_LAGGING: u32 = 1294;
                if rc == ERROR_SERVICE_NOTIFY_CLIENT_LAGGING {
                    self.reopen()?;
                    continue; // re-arm against the fresh handle
                }
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
        }

        fn control_stop(&mut self) -> io::Result<()> {
            let mut status = SERVICE_STATUS::default();
            // SAFETY: `self.service.raw_handle()` was opened with
            // `SERVICE_STOP` access; `status` is a valid, aligned out-param.
            let ok = unsafe { ControlService(self.service.raw_handle(), SERVICE_CONTROL_STOP, &mut status) };
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
            match self.service.start::<&std::ffi::OsStr>(&[]) {
                Ok(()) => Ok(()),
                // Per [MS-SCMR]'s `RStartServiceW` error table, 1056 means
                // only "`dwCurrentState` is not `SERVICE_STOPPED`" — it also
                // covers StopPending/Paused/etc., none of which the
                // `RUNNING`/`START_PENDING` arm above would have
                // immediate-fired for (`NotifyServiceStatusChangeW` only
                // immediate-fires when the service already matches the
                // requested mask). Blindly treating 1056 as "already
                // running, the wait will complete" would hang forever in
                // those other cases. Confirm the real state before deciding.
                Err(windows_service::Error::Winapi(e))
                    if e.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING as i32) =>
                {
                    match self.service.query_status() {
                        Ok(status) if status.current_state == windows_service::service::ServiceState::Running => Ok(()),
                        Ok(status) => Err(io::Error::other(format!(
                            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING, but the service is actually \
                             {:?}, not Running — this wait cannot resolve from that state",
                            status.current_state
                        ))),
                        Err(query_err) => Err(to_io_error(query_err)),
                    }
                }
                Err(e) => Err(to_io_error(e)),
            }
        }

        fn wait_callback(&mut self) -> io::Result<Observed> {
            // Alertable wait: blocks until the SCM delivers the notify APC,
            // which runs `notify_callback` and sets `status.fired`. A
            // spurious early wake (an unrelated APC) re-enters the wait.
            while !self.status.fired.load(Ordering::Acquire) {
                // SAFETY: `SleepEx` takes no pointers; `true` (`TRUE`) makes
                // it alertable, which is the entire point of this wait.
                unsafe { SleepEx(u32::MAX, 1) };
            }
            // The APC that `arm()` may have queued has now been delivered
            // and processed (that is what set `fired`) — nothing from this
            // registration remains outstanding. See `Drop`.
            self.registration_outstanding = false;
            let state = self.status.current_state.load(Ordering::Acquire);
            Ok(if state == SERVICE_RUNNING {
                Observed::Running
            } else if state == SERVICE_STOPPED {
                Observed::Stopped
            } else {
                Observed::Pending
            })
        }
    }
}

#[cfg(windows)]
pub use system::SystemScmActor;

#[cfg(test)]
#[path = "scm_wait_tests.rs"]
mod scm_wait_tests;
