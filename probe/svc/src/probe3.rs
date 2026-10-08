//! Round-3 probe: notify capture time, ControlServiceExW's reply, what
//! ControlService fills per error, and whether notify counts changes.
//! 10s per alertable wait is this probe's own failure bound.
use std::ffi::c_void;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, SleepEx};

const ALL: u32 = SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_START_PENDING | SERVICE_NOTIFY_STOP_PENDING
    | SERVICE_NOTIFY_RUNNING | SERVICE_NOTIFY_CONTINUE_PENDING | SERVICE_NOTIFY_PAUSE_PENDING | SERVICE_NOTIFY_PAUSED;
const STOPS: u32 = SERVICE_NOTIFY_STOPPED | SERVICE_NOTIFY_STOP_PENDING;
const SENTINEL: u32 = 0xEEEE_EEEE;

#[derive(Default)]
struct Slot { fired: AtomicU32, status: AtomicU32, state: AtomicU32, pid: AtomicU32, w32: AtomicU32, ss: AtomicU32 }

unsafe extern "system" fn cb(p: *const c_void) {
    let n = unsafe { &*(p as *const SERVICE_NOTIFY_2W) };
    let s = unsafe { &*(n.pContext as *const Slot) };
    let st = &n.ServiceStatus;
    s.status.store(n.dwNotificationStatus, Ordering::SeqCst);
    s.state.store(st.dwCurrentState, Ordering::SeqCst);
    s.pid.store(st.dwProcessId, Ordering::SeqCst);
    s.w32.store(st.dwWin32ExitCode, Ordering::SeqCst);
    s.ss.store(st.dwServiceSpecificExitCode, Ordering::SeqCst);
    s.fired.store(1, Ordering::SeqCst);
}

struct Notifier { h: SC_HANDLE, buf: Box<SERVICE_NOTIFY_2W>, slot: Box<Slot> }

impl Notifier {
    fn new(h: SC_HANDLE) -> Self { Self { h, buf: Box::new(unsafe { std::mem::zeroed() }), slot: Box::default() } }
    fn arm(&mut self, mask: u32) -> u32 {
        self.slot.fired.store(0, Ordering::SeqCst);
        *self.buf = unsafe { std::mem::zeroed() };
        self.buf.dwVersion = SERVICE_NOTIFY_STATUS_CHANGE;
        self.buf.pfnNotifyCallback = Some(cb);
        self.buf.pContext = &*self.slot as *const Slot as *mut c_void;
        unsafe { NotifyServiceStatusChangeW(self.h, mask, &*self.buf) }
    }
    fn wait(&self) -> String {
        unsafe { SleepEx(10_000, 1) };
        let s = &self.slot;
        if s.fired.load(Ordering::SeqCst) == 0 { return "NO FIRE in 10s".into(); }
        format!("notifStatus={} state={} pid={} win32={} specific={}", s.status.load(Ordering::SeqCst),
            s.state.load(Ordering::SeqCst), s.pid.load(Ordering::SeqCst), s.w32.load(Ordering::SeqCst), s.ss.load(Ordering::SeqCst))
    }
}

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

fn open(name: &str) -> SC_HANDLE {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        let h = OpenServiceW(scm, wide(name).as_ptr(), SERVICE_QUERY_STATUS | SERVICE_STOP | SERVICE_START | SERVICE_PAUSE_CONTINUE);
        assert!(!h.is_null(), "open {name}");
        h
    }
}

fn run(prog: &str, args: &[&str]) { let o = Command::new(prog).args(args).output().unwrap(); println!("  {prog} {args:?} rc={:?}", o.status.code()); }

fn query(h: SC_HANDLE) -> String {
    let mut p: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut need = 0u32;
    let ok = unsafe { QueryServiceStatusEx(h, SC_STATUS_PROCESS_INFO, &mut p as *mut _ as *mut u8, std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32, &mut need) };
    format!("query ok={ok} state={} pid={}", p.dwCurrentState, p.dwProcessId)
}

fn ctl(label: &str, h: SC_HANDLE, code: u32) {
    let mut s = SERVICE_STATUS { dwServiceType: SENTINEL, dwCurrentState: SENTINEL, dwControlsAccepted: SENTINEL,
        dwWin32ExitCode: SENTINEL, dwServiceSpecificExitCode: SENTINEL, dwCheckPoint: SENTINEL, dwWaitHint: SENTINEL };
    let ok = unsafe { ControlService(h, code, &mut s) };
    let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
    println!("{label}: ControlService({code}) ok={ok} err={err} -> state={:#x} accepted={:#x} win32={:#x} specific={:#x} type={:#x}",
        s.dwCurrentState, s.dwControlsAccepted, s.dwWin32ExitCode, s.dwServiceSpecificExitCode, s.dwServiceType);
}

fn ctl_ex(label: &str, h: SC_HANDLE, code: u32, reason: u32) -> u32 {
    let mut p: SERVICE_CONTROL_STATUS_REASON_PARAMSW = unsafe { std::mem::zeroed() };
    p.dwReason = reason;
    p.ServiceStatus.dwCurrentState = SENTINEL;
    p.ServiceStatus.dwProcessId = SENTINEL;
    p.ServiceStatus.dwWin32ExitCode = SENTINEL;
    p.ServiceStatus.dwServiceSpecificExitCode = SENTINEL;
    let ok = unsafe { ControlServiceExW(h, code, SERVICE_CONTROL_STATUS_REASON_INFO, &mut p as *mut _ as *mut c_void) };
    let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
    let s = &p.ServiceStatus;
    println!("{label}: ControlServiceExW({code}, reason={reason:#x}) ok={ok} err={err} -> state={:#x} pid={:#x} win32={:#x} specific={:#x}",
        s.dwCurrentState, s.dwProcessId, s.dwWin32ExitCode, s.dwServiceSpecificExitCode);
    err
}

fn gate(name: &str) -> HANDLE {
    let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide(&format!("Global\\goetiaprobe-gate-{name}")).as_ptr()) };
    println!("  gate {name}: created={} err={}", !ev.is_null(), unsafe { GetLastError() });
    ev
}

fn release(name: &str, ev: HANDLE) {
    let ok = unsafe { SetEvent(ev) };
    println!("  release gate {name}: set={ok}");
}

fn h1() {
    let name = "goetiap3-h1";
    println!("=== H1: is a queued notification's status captured at registration or at delivery?");
    run("net.exe", &["start", name]);
    let (to_b, from_a) = mpsc::channel::<()>();
    let (to_a, from_b) = mpsc::channel::<()>();
    let b = std::thread::spawn(move || {
        from_a.recv().unwrap();
        let h = open(name);
        let mut n = Notifier::new(h);
        let rc = n.arm(STOPS);
        ctl("H1 thread B", h, SERVICE_CONTROL_STOP);
        println!("H1 thread B: arm rc={rc}; observed {}", n.wait());
        println!("H1 thread B: after confirmed stop, {}", query(h));
        to_a.send(()).unwrap();
    });
    let h = open(name);
    let mut n = Notifier::new(h);
    println!("H1 thread A: before arm, {}", query(h));
    let rc = n.arm(ALL);
    println!("H1 thread A: armed Every rc={rc} on the RUNNING service; not alertable until B confirms the stop");
    to_b.send(()).unwrap();
    from_b.recv().unwrap();
    println!("H1 thread A: now alertable -> {}", n.wait());
    b.join().unwrap();
}

fn h2() {
    let name = "goetiap3-h2";
    println!("=== H2: ControlServiceExW(STOP, SERVICE_CONTROL_STATUS_REASON_INFO)");
    for (label, reason) in [
        ("planned/none/none", SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_NONE | SERVICE_STOP_REASON_MINOR_NONE),
        ("planned/other/other", SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_OTHER | SERVICE_STOP_REASON_MINOR_OTHER),
    ] {
        run("net.exe", &["start", name]);
        let h = open(name);
        println!("H2 {label}: before, {}", query(h));
        let mut n = Notifier::new(h);
        n.arm(STOPS);
        let err = ctl_ex(&format!("H2 {label}"), h, SERVICE_CONTROL_STOP, reason);
        if err == 0 { println!("H2 {label}: stop observed {}", n.wait()); }
    }
    run("net.exe", &["start", name]);
    let h = open(name);
    let mut n = Notifier::new(h);
    n.arm(STOPS);
    ctl("H2 plain", h, SERVICE_CONTROL_STOP);
    println!("H2 plain: stop observed {}", n.wait());
}

fn m1() {
    println!("=== M1: what ControlService(Ex) fills, per outcome");
    let name = "goetiap3-m1";
    let g = gate(name);
    run("net.exe", &["start", name]);
    let h = open(name);
    println!("M1: {}", query(h));
    ctl("M1 RUNNING, PAUSE not accepted", h, SERVICE_CONTROL_PAUSE);
    ctl_ex("M1 RUNNING, PAUSE not accepted (Ex)", h, SERVICE_CONTROL_PAUSE, SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_NONE | SERVICE_STOP_REASON_MINOR_NONE);
    let mut n = Notifier::new(h);
    n.arm(STOPS);
    ctl("M1 RUNNING, STOP", h, SERVICE_CONTROL_STOP);
    println!("M1: after STOP, observed {}", n.wait());
    ctl("M1 STOP_PENDING, STOP again", h, SERVICE_CONTROL_STOP);
    ctl_ex("M1 STOP_PENDING, STOP again (Ex)", h, SERVICE_CONTROL_STOP, SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_NONE | SERVICE_STOP_REASON_MINOR_NONE);
    n.arm(STOPS);
    release(name, g);
    println!("M1: after release, observed {}", n.wait());
    ctl("M1 STOPPED, STOP", h, SERVICE_CONTROL_STOP);
    ctl_ex("M1 STOPPED, STOP (Ex)", h, SERVICE_CONTROL_STOP, SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_NONE | SERVICE_STOP_REASON_MINOR_NONE);

    let name = "goetiap3-m1s";
    let g = gate(name);
    run("sc.exe", &["start", name]);
    let h = open(name);
    let mut n = Notifier::new(h);
    n.arm(ALL);
    println!("M1s: snapshot {}", n.wait());
    println!("M1s: {}", query(h));
    ctl("M1s START_PENDING, STOP", h, SERVICE_CONTROL_STOP);
    ctl_ex("M1s START_PENDING, STOP (Ex)", h, SERVICE_CONTROL_STOP, SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_NONE | SERVICE_STOP_REASON_MINOR_NONE);
    n.arm(ALL);
    release(name, g);
    println!("M1s: after release, observed {}", n.wait());
    run("net.exe", &["stop", name]);
}

fn m3() {
    println!("=== M3: does notify count changes?");
    let name = "goetiap3-m3";
    let h = open(name);
    let mut n = Notifier::new(h);
    n.arm(ALL);
    println!("M3 snapshot: {}", n.wait());
    for _ in 0..2 { run("net.exe", &["start", name]); run("net.exe", &["stop", name]); }
    for i in 1..=3 { let rc = n.arm(ALL); println!("M3 arm {i} after two unobserved start+stop cycles: rc={rc} {}", n.wait()); }
}

fn main() {
    m1();
    let h = open("goetiap3-m3");
    println!("=== M1: a never-started service");
    ctl("M1n STOPPED (never started), STOP", h, SERVICE_CONTROL_STOP);
    ctl_ex("M1n STOPPED (never started), STOP (Ex)", h, SERVICE_CONTROL_STOP, SERVICE_STOP_REASON_FLAG_PLANNED | SERVICE_STOP_REASON_MAJOR_NONE | SERVICE_STOP_REASON_MINOR_NONE);
}
