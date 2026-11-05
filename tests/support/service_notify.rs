//! SCM state-change notifications (`NotifyServiceStatusChangeW`) for tests that wait on a service.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use windows_service::service::{Service, ServiceAccess};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::System::Services::{
    NotifyServiceStatusChangeW, SERVICE_NOTIFY_2W, SERVICE_NOTIFY_STATUS_CHANGE,
};
use windows_sys::Win32::System::Threading::{INFINITE, SleepEx};

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

/// A registration made by [`arm`]: it owns the handles it was made on, which cancel it when they
/// close.
struct Armed {
    delivery: &'static Delivery,
    _handles: Handles,
}

/// Registers `mask` on a fresh connection to service `id`. On `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` it reopens the SCM and service handles
/// and arms again, as the documented remedy and `ScmActor::arm` do. With a `deadline` that loop
/// ends there, with the lag error; without one it has no cap.
fn arm(id: &str, mask: u32, deadline: Option<Instant>) -> std::io::Result<Armed> {
    // Leaked: SCM may still hold the registration if the test panics, or the deadline passes,
    // before the delivery.
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
        let error = unsafe {
            use std::os::windows::io::AsRawHandle as _;
            NotifyServiceStatusChangeW(handles.service.raw_handle(), mask, notify)
        };
        match error {
            0 => {
                return Ok(Armed {
                    delivery,
                    _handles: handles,
                });
            }
            ERROR_SERVICE_NOTIFY_CLIENT_LAGGING if deadline.is_none_or(|deadline| Instant::now() < deadline) => {
                handles = Handles::open(id)?;
            }
            error => return Err(std::io::Error::from_raw_os_error(error as i32)),
        }
    }
}

impl Armed {
    /// Blocks, alertably, until SCM delivers.
    fn wait(&self) -> (u32, u32) {
        while !self.delivery.fired.load(Ordering::Acquire) {
            // SAFETY: plain FFI call; an alertable wait is what lets the APC run.
            unsafe { SleepEx(INFINITE, 1) };
        }
        self.delivered()
    }

    /// [`Self::wait`], ending with `None` once `deadline` passes.
    fn wait_until(&self, deadline: Instant) -> Option<(u32, u32)> {
        while !self.delivery.fired.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // Rounded up, and below INFINITE, which would mean no bound.
            let milliseconds = remaining.as_millis().clamp(1, u128::from(INFINITE - 1)) as u32;
            // SAFETY: plain FFI call; an alertable wait is what lets the APC run.
            unsafe { SleepEx(milliseconds, 1) };
        }
        Some(self.delivered())
    }

    fn delivered(&self) -> (u32, u32) {
        (
            self.delivery.state.load(Ordering::Acquire),
            self.delivery.controls_accepted.load(Ordering::Acquire),
        )
    }
}

/// Arm `mask` on service `id`; the returned closure blocks, alertably, until SCM delivers it and
/// returns `(state, controls_accepted)` of the delivery. Armed on a state that does not match `mask`
/// yet, the delivery is the transition; a service already in a matching state delivers at once.
///
/// Registration can fail (a deleted service reports 1060); the error is returned.
pub fn arm_and_wait(id: &str, mask: u32) -> std::io::Result<impl FnOnce() -> (u32, u32)> {
    let armed = arm(id, mask, None)?;
    Ok(move || armed.wait())
}

/// [`arm_and_wait`] with a `deadline`: arming and the wait end there, the wait with `None`.
pub fn arm_and_wait_until(
    id: &str,
    mask: u32,
    deadline: Instant,
) -> std::io::Result<impl FnOnce() -> Option<(u32, u32)>> {
    let armed = arm(id, mask, Some(deadline))?;
    Ok(move || armed.wait_until(deadline))
}
