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
//! `wait_tests.rs`; [`system::SystemScmActor`] drives the real SCM.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io;

use crate::manager::budget::Deadline;

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

/// Whether a wait reached the state it was waiting for, or ran out of
/// [`Deadline`] first. An expiry is not an error: the request was issued and
/// the service may yet arrive, so what to say about it is the caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    Confirmed,
    Expired,
}

/// The granular SCM operations [`stop_via_notify`]/[`start_via_notify`] need,
/// isolated so the ordering can be unit-tested with a fake rather than a real
/// service.
pub trait ScmActor {
    /// Register a status-change notification for `want`.
    /// `NotifyServiceStatusChangeW` is single-shot, so the sequence re-arms
    /// after every non-terminal callback.
    ///
    /// `deadline` bounds the one loop here that is not the wait itself: the
    /// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` remedy, which reopens both
    /// handles and arms again. `Waited::Expired` iff the deadline ran out
    /// while the SCM kept answering that.
    fn arm(&mut self, want: WantState, deadline: Deadline) -> io::Result<Waited>;
    /// Takes no deadline, as [`Self::start`] does not: each is a single
    /// non-looping call into the SCM, so there is no iteration to bound, and
    /// what blocking they do is the SCM's own — not a wait this module
    /// created, and not one it can bound from the client side.
    fn control_stop(&mut self) -> io::Result<()>;
    fn start(&mut self) -> io::Result<()>;
    /// Block in an alertable wait until the armed notification fires and
    /// return the service's observed state from the callback buffer;
    /// `Ok(None)` iff `deadline` expired with nothing delivered.
    fn wait_callback(&mut self, deadline: Deadline) -> io::Result<Option<Observed>>;
}

/// Stop the service, gated strictly on a real `STOPPED` callback from
/// `NotifyServiceStatusChangeW`; re-arms after a non-terminal (pending)
/// callback, under the same `deadline` throughout.
pub fn stop_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    let armed = a.arm(WantState::Stopped, deadline)?;
    // Issued even when the arm expired: only the *waiting* is bounded.
    a.control_stop()?;
    if armed == Waited::Expired {
        return Ok(Waited::Expired);
    }
    loop {
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Stopped) => return Ok(Waited::Confirmed),
            // Running/Pending are non-terminal for a stop wait — re-arm and wait.
            Some(Observed::Running | Observed::Pending) => {
                if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
                    return Ok(Waited::Expired);
                }
            }
        }
    }
}

/// Start the service, gated strictly on a real `RUNNING` callback; re-arms
/// after a non-terminal callback, under the same `deadline` throughout —
/// which is what makes a service that chatters `START_PENDING` forever
/// terminate at the caller's budget instead of never.
///
/// Critical ordering: arm `RUNNING` strictly BEFORE issuing `start`, else the
/// service can reach `RUNNING` before the arm and the notification only
/// fires on the *next* entry into `RUNNING` — a hang.
pub fn start_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    let armed = a.arm(WantState::Running, deadline)?;
    // Issued even when the arm expired: only the *waiting* is bounded, and a
    // service left unstarted because goetia ran out of budget while arming
    // would be the worst of both answers.
    a.start()?;
    if armed == Waited::Expired {
        return Ok(Waited::Expired);
    }
    loop {
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Running) => return Ok(Waited::Confirmed),
            // A terminal Stopped means the service stopped instead of
            // reaching Running — a failed start, not an expiry.
            Some(Observed::Stopped) => {
                return Err(io::Error::other(
                    "service stopped before reaching Running (failed start)",
                ));
            }
            Some(Observed::Pending) => {
                if a.arm(WantState::Running, deadline)? == Waited::Expired {
                    return Ok(Waited::Expired);
                }
            }
        }
    }
}

/// Issue the start request and return, waiting for nothing — the
/// `Budget::Immediate` path, where the caller has no budget to watch the
/// service with. A named function rather than a bare [`ScmActor::start`] at
/// the call site so that what happens when we do *not* wait is covered by
/// the same fake-actor tests as everything else.
#[allow(dead_code)] // no caller on Windows either, until the manager's verbs take a `Budget`
pub fn request_start<A: ScmActor>(a: &mut A) -> io::Result<()> {
    a.start()
}

/// [`request_start`]'s mirror: issue the stop control and return.
#[allow(dead_code)] // no caller on Windows either, until the manager's verbs take a `Budget`
pub fn request_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
    a.control_stop()
}

// hoisted from `mod system` ===========================================================================================

/// The teardown half of [`system::SystemScmActor`], behind a trait for the
/// same reason [`ScmActor`] is one: the thing that matters here is an
/// *order*, and an order is only assertable if a fake can record it.
pub trait ScmHandles {
    /// Close the per-service handle and the SCM connection.
    fn close_handles(&mut self);
    /// Whether an `arm` succeeded that no `wait_callback` has consumed.
    fn registration_outstanding(&self) -> bool;
    /// Run a notification the SCM has already queued to this thread, via a
    /// zero-duration alertable wait.
    fn drain_queued_apc(&mut self);
}

/// Close both handles, THEN drain a notification the SCM may already have
/// queued.
///
/// An `arm()` that immediate-fired (the service already matched the
/// requested mask at the moment of the call) queues an APC to THIS thread
/// regardless of what happens next, and closing the handles only stops
/// *future* notifications from being queued (per
/// `NotifyServiceStatusChangeW`'s documented remarks) — it does not un-queue
/// one already pending. Left undrained — a `control_stop`/`start` that
/// errored before the wait ever ran, or a wait that expired before the APC
/// arrived — it fires later, on some unrelated alertable wait elsewhere in
/// the process, against the `LastStatus`/`SERVICE_NOTIFY_2W` boxes the actor
/// is about to free: memory corruption, and a `fired` flag set on whatever
/// occupies that address by then.
///
/// Closing first is what makes the drain final. Draining first leaves a
/// window in which the SCM can queue a fresh notification between the drain
/// returning and the handle closing — and on the expiry path, where the APC
/// has usually not been queued yet, that window is the likely case, not the
/// unlikely one.
///
/// What the fake in `wait_tests.rs` pins is exactly that SEQUENCE, and no
/// more. That `Service`'s `Drop` really closes the `SC_HANDLE`, that the SCM
/// really stops queuing once it is closed, and that `Drop for
/// SystemScmActor` reaches this function at all stay verifiable only on
/// Windows CI.
pub fn close_then_drain<H: ScmHandles>(h: &mut H) {
    h.close_handles();
    if h.registration_outstanding() {
        h.drain_queued_apc();
    }
}

/// The state a follow-up `query_status` reported — one variant per
/// `windows_service::service::ServiceState`, mirrored here so
/// [`already_running_is_recoverable`] is a decision about a plain enum and
/// can be tested off Windows.
///
/// Deliberately not [`Observed`], which collapses `StartPending` and
/// `StopPending` into a single `Pending`: telling those two apart is the
/// entire content of the decision below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueriedState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
}

/// Whether a `StartServiceW` that failed with `ERROR_SERVICE_ALREADY_RUNNING`
/// (1056) can still be resolved by the wait that is already armed, given the
/// state a follow-up query reported.
///
/// 1056 means only "`dwCurrentState` is not `SERVICE_STOPPED`", which lumps
/// together states this wait can finish from and states it cannot.
/// `Running` and `StartPending` are the two it can: `want_to_mask` arms
/// `SERVICE_NOTIFY_START_PENDING` in *both* of its `WantState::Running`
/// branches, so a service in either state immediate-fires and the wait
/// resolves normally. `StartPending` in particular is what a service is in
/// when something else started it a moment earlier — rejecting it fails a
/// start that was going to succeed. For every other state the registration
/// holds no bit that can fire, so accepting 1056 would block until the
/// deadline instead of reporting what the service is really doing.
#[must_use]
pub fn already_running_is_recoverable(queried: QueriedState) -> bool {
    matches!(queried, QueriedState::Running | QueriedState::StartPending)
}

/// `NotifyServiceStatusChangeW`'s "you are too far behind" return. Not among
/// `windows-sys`'s `Foundation` exports, so it is spelled out.
pub const ERROR_SERVICE_NOTIFY_CLIENT_LAGGING: u32 = 1294;

/// The two steps [`register_until_deadline`] drives: one attempt to register
/// the notification, and the lag remedy. Behind a trait so that the loop's
/// bound — the thing that is easy to lose — is assertable off Windows.
pub trait NotifyRegistrar {
    /// Register the notification, reporting `NotifyServiceStatusChangeW`'s
    /// raw return code (`0` on success).
    fn register(&mut self) -> io::Result<u32>;
    /// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`'s documented remedy: "close the
    /// handle to the SCM, open a new handle, and call this function again".
    fn reopen(&mut self) -> io::Result<()>;
}

/// Register the notification, following the lag remedy for as long as
/// `deadline` allows.
///
/// This is the one loop in this module that is not the wait itself, and it
/// sits inside an operation the caller bounded — a lagging SCM would
/// otherwise make `--timeout 5s` block forever. The deadline is checked at
/// the TOP of every iteration, including the first, and is the only thing
/// that ends the loop: deliberately not a retry counter, which would be a
/// bound nobody asked for, where this one the caller did ask for.
pub fn register_until_deadline<R: NotifyRegistrar>(r: &mut R, deadline: Deadline) -> io::Result<Waited> {
    loop {
        if deadline.expired() {
            return Ok(Waited::Expired);
        }
        match r.register()? {
            0 => return Ok(Waited::Confirmed),
            ERROR_SERVICE_NOTIFY_CLIENT_LAGGING => r.reopen()?, // re-arm against the fresh handle
            rc => return Err(io::Error::from_raw_os_error(rc as i32)),
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

    use windows_service::service::{Service, ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE, WAIT_IO_COMPLETION};
    use windows_sys::Win32::System::Services::{
        ControlService, NotifyServiceStatusChangeW, SERVICE_CONTROL_STOP, SERVICE_NOTIFY, SERVICE_NOTIFY_2W,
        SERVICE_NOTIFY_RUNNING, SERVICE_NOTIFY_START_PENDING, SERVICE_NOTIFY_STATUS_CHANGE,
        SERVICE_NOTIFY_STOP_PENDING, SERVICE_NOTIFY_STOPPED, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STOPPED,
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
            WantState::Running if started => {
                SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_START_PENDING
            }
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
}

#[cfg(windows)]
pub use system::SystemScmActor;

#[cfg(test)]
#[path = "wait_tests.rs"]
mod wait_tests;
