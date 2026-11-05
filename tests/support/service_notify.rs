//! SCM state-change notifications (`NotifyServiceStatusChangeW`) for tests that wait on a service.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use windows_service::service::Service;
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
    // SAFETY: SCM hands back the buffer registered in `arm_and_wait`, whose `pContext` is the leaked
    // `Delivery`.
    let (delivery, status) = unsafe { (&*(*notify).pContext.cast::<Delivery>(), &(*notify).ServiceStatus) };
    delivery.state.store(status.dwCurrentState, Ordering::Release);
    delivery
        .controls_accepted
        .store(status.dwControlsAccepted, Ordering::Release);
    delivery.fired.store(true, Ordering::Release);
}

/// Arm `mask` on `service`; the returned closure blocks, alertably, until SCM delivers it and
/// returns `(state, controls_accepted)` of the delivery. Armed on a state that does not match `mask`
/// yet, the delivery is the transition; a service already in a matching state delivers at once.
///
/// With a `deadline` the wait ends there and returns `None`; without one it has no bound.
/// Registration can fail (a deleted service reports 1060); the error is returned.
pub fn arm_and_wait(
    service: &Service,
    mask: u32,
    deadline: Option<Instant>,
) -> std::io::Result<impl FnOnce() -> Option<(u32, u32)>> {
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
    // SAFETY: the handle has `SERVICE_QUERY_STATUS`; `notify` is leaked and so outlives the
    // registration.
    let error = unsafe {
        use std::os::windows::io::AsRawHandle as _;
        NotifyServiceStatusChangeW(service.raw_handle(), mask, notify)
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32));
    }
    Ok(move || {
        while !delivery.fired.load(Ordering::Acquire) {
            let milliseconds = match deadline {
                None => INFINITE,
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    // Rounded up, and below INFINITE, which would mean no bound.
                    remaining.as_millis().clamp(1, u128::from(INFINITE - 1)) as u32
                }
            };
            // SAFETY: plain FFI call; an alertable wait is what lets the APC run.
            unsafe { SleepEx(milliseconds, 1) };
        }
        Some((
            delivery.state.load(Ordering::Acquire),
            delivery.controls_accepted.load(Ordering::Acquire),
        ))
    })
}
