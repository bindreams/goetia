//! `notify_callback`'s decoding of a delivery, driven by hand with no SCM call.

use std::sync::atomic::{AtomicBool, AtomicU32};

use windows_sys::Win32::Foundation::{ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOTIFY_CLIENT_LAGGING};

use super::*;

fn slot() -> Box<LastStatus> {
    Box::new(LastStatus {
        current_state: AtomicU32::new(0),
        exit_win32: AtomicU32::new(0),
        exit_specific: AtomicU32::new(0),
        wait_hint: AtomicU32::new(0),
        controls_accepted: AtomicU32::new(0),
        notification_status: AtomicU32::new(0),
        fired: AtomicBool::new(false),
    })
}

/// Deliver a notification to `slot` as SCM does: the buffer's `pContext` is the slot.
fn deliver(slot: &mut LastStatus, notification_status: u32, state: u32) {
    let mut buf = SERVICE_NOTIFY_2W::default();
    buf.pContext = (slot as *mut LastStatus).cast();
    buf.dwNotificationStatus = notification_status;
    buf.ServiceStatus.dwCurrentState = state;
    // SAFETY: `buf` is a live `SERVICE_NOTIFY_2W` whose `pContext` is the live `slot`.
    unsafe { notify_callback((&raw const buf).cast()) };
}

/// A delivery with a non-zero `dwNotificationStatus` reports that error and no state, whatever its
/// `ServiceStatus` holds; the next delivery with a zero one reports its state again.
#[skuld::test]
fn a_failed_delivery_is_decoded_as_failed_and_a_later_zero_status_as_a_state() {
    let mut slot = slot();
    for code in [ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOTIFY_CLIENT_LAGGING, 5] {
        deliver(&mut slot, code, SERVICE_RUNNING);
        assert!(slot.fired.load(Ordering::Acquire), "{code}");
        assert_eq!(observed(&slot), Observed::Failed(code));
    }
    deliver(&mut slot, 0, SERVICE_STOPPED);
    assert_eq!(observed(&slot), Observed::Stopped);
}
