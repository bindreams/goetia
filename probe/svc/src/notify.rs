//! `notify-probe <name>`: arm NotifyServiceStatusChangeW(STOPPED) twice on
//! one fresh handle against an already-STOPPED service and report whether
//! each fired, with what the callback buffer carried. 10s per arm is this
//! probe's own failure bound, nothing more.
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::SleepEx;

static FIRED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn cb(p: *const c_void) {
    let n = unsafe { &*(p as *const SERVICE_NOTIFY_2W) };
    let s = &n.ServiceStatus;
    println!(
        "  callback: dwNotificationStatus={} state={} win32={} specific={} pid={}",
        n.dwNotificationStatus, s.dwCurrentState, s.dwWin32ExitCode, s.dwServiceSpecificExitCode, s.dwProcessId
    );
    FIRED.store(true, Ordering::SeqCst);
}

fn main() {
    let name: Vec<u16> = std::env::args().nth(1).unwrap().encode_utf16().chain([0]).collect();
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        let svc = OpenServiceW(scm, name.as_ptr(), SERVICE_QUERY_STATUS);
        assert!(!svc.is_null(), "open service");
        for attempt in 1..=2 {
            FIRED.store(false, Ordering::SeqCst);
            let mut buf: SERVICE_NOTIFY_2W = std::mem::zeroed();
            buf.dwVersion = SERVICE_NOTIFY_STATUS_CHANGE;
            buf.pfnNotifyCallback = Some(cb);
            let rc = NotifyServiceStatusChangeW(svc, SERVICE_NOTIFY_STOPPED, &buf);
            let zero = SleepEx(0, 1);
            let fired_at_zero = FIRED.load(Ordering::SeqCst);
            if !fired_at_zero {
                SleepEx(10_000, 1);
            }
            println!(
                "  arm {attempt}: rc={rc} fired_by_SleepEx(0)={fired_at_zero} (SleepEx(0) returned {zero}) fired_within_10s={}",
                FIRED.load(Ordering::SeqCst)
            );
            if !FIRED.load(Ordering::SeqCst) {
                println!("  arm {attempt}: no fire; leaving the registration outstanding and exiting");
                break;
            }
        }
    }
}
