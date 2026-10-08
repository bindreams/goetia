//! `notify2 <name>`: one handle, one thread, a scripted sequence of arms
//! against a probe service in `specific6` mode. 10s per wait is this
//! probe's own failure bound.
use std::ffi::c_void;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::SleepEx;

static STATE: AtomicU32 = AtomicU32::new(0);
static CODES: AtomicU32 = AtomicU32::new(0);
static PID: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn cb(p: *const c_void) {
    let n = unsafe { &*(p as *const SERVICE_NOTIFY_2W) };
    STATE.store(n.ServiceStatus.dwCurrentState, Ordering::SeqCst);
    PID.store(n.ServiceStatus.dwProcessId, Ordering::SeqCst);
    CODES.store(n.ServiceStatus.dwWin32ExitCode * 100 + n.ServiceStatus.dwServiceSpecificExitCode, Ordering::SeqCst);
}

const ALL: u32 = SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_START_PENDING | SERVICE_NOTIFY_STOP_PENDING
    | SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_CONTINUE_PENDING | SERVICE_NOTIFY_PAUSE_PENDING | SERVICE_NOTIFY_PAUSED;
const STOPS: u32 = SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_STOP_PENDING;

struct H { svc: SC_HANDLE, buf: Box<SERVICE_NOTIFY_2W> }

impl H {
    /// Arm `mask`, then run `between` (if any), then wait up to 10s.
    fn arm_wait(&mut self, label: &str, mask: u32, between: Option<&[&str]>) {
        STATE.store(0, Ordering::SeqCst);
        *self.buf = unsafe { std::mem::zeroed() };
        self.buf.dwVersion = SERVICE_NOTIFY_STATUS_CHANGE;
        self.buf.pfnNotifyCallback = Some(cb);
        let rc = unsafe { NotifyServiceStatusChangeW(self.svc, mask, &*self.buf) };
        if let Some(args) = between {
            let o = Command::new("sc.exe").args(args).output().unwrap();
            let _ = o;
        }
        unsafe { SleepEx(10_000, 1) };
        let s = STATE.load(Ordering::SeqCst);
        println!("{label}: arm rc={rc} -> {}", if s == 0 { "NO FIRE in 10s".to_string() } else { format!("state={s} win32*100+specific={} pid={}", CODES.load(Ordering::SeqCst), PID.load(Ordering::SeqCst)) });
    }
}

fn net(args: &[&str]) {
    let o = Command::new("net.exe").args(args).output().unwrap();
    println!("  net {:?} rc={:?}", args, o.status.code());
}

fn main() {
    let name = std::env::args().nth(1).unwrap();
    let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let svc = unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        OpenServiceW(scm, w.as_ptr(), SERVICE_QUERY_STATUS)
    };
    let mut h = H { svc, buf: Box::new(unsafe { std::mem::zeroed() }) };
    // A: snapshot of a never-started service.
    h.arm_wait("A all-states on STOPPED (never started)", ALL, None);
    // B: start it, snapshot while running.
    net(&["start", &name]);
    h.arm_wait("B all-states on RUNNING", ALL, None);
    // C: arm stops, then stop: the transition is delivered with the stop's codes.
    h.arm_wait("C stops-mask, then sc stop", STOPS, Some(&["stop", &name]));
    // D: same handle, state unchanged since C's fire: does a stops arm fire?
    h.arm_wait("D stops-mask again, no change since C", STOPS, None);
    // E: running -> stopped happens while NOT armed; then arm stops.
    net(&["start", &name]);
    net(&["stop", &name]);
    h.arm_wait("E stops-mask after an unobserved start+stop", STOPS, None);
    // F: stale snapshot then a start before the stop arm: does the stop arm wait for the new instance?
    h.arm_wait("F1 all-states on STOPPED", ALL, None);
    net(&["start", &name]);
    h.arm_wait("F2 stops-mask while the new instance runs, then sc stop", STOPS, Some(&["stop", &name]));
    // G: all-states arms only. Snapshot (running), then a stop and a start
    // both unobserved, then an all-states re-arm: does it fire with the new pid?
    h.arm_wait("G1 all-states snapshot while running", ALL, None);
    net(&["stop", &name]);
    net(&["start", &name]);
    h.arm_wait("G2 all-states after an unobserved stop+start", ALL, None);
    h.arm_wait("G3 all-states again, no change", ALL, None);
    h.arm_wait("G4 all-states, then sc stop", ALL, Some(&["stop", &name]));
}
