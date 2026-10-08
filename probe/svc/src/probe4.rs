//! Round-4 probe. `probe4 grant <account>` | `pin <svc>` | `race <svc> <n>` | `linger <svc>`.
//! Every wait here carries this probe's own failure bound, nothing more.
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authentication::Identity::*;
use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::*;

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

fn grant(account: &str) {
    unsafe {
        let mut sid = vec![0u8; 256];
        let mut cb = sid.len() as u32;
        let mut dom = vec![0u16; 256];
        let mut cd = dom.len() as u32;
        let mut use_: SID_NAME_USE = 0;
        let ok = LookupAccountNameW(std::ptr::null(), wide(account).as_ptr(), sid.as_mut_ptr() as _, &mut cb, dom.as_mut_ptr(), &mut cd, &mut use_);
        assert!(ok != 0, "LookupAccountNameW {account}: {}", GetLastError());
        let attrs: LSA_OBJECT_ATTRIBUTES = std::mem::zeroed();
        let mut pol: LSA_HANDLE = 0;
        let st = LsaOpenPolicy(std::ptr::null(), &attrs, (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32, &mut pol);
        assert!(st == 0, "LsaOpenPolicy {st:#x}");
        let mut right = wide("SeServiceLogonRight");
        right.pop();
        let us = LSA_UNICODE_STRING { Length: (right.len() * 2) as u16, MaximumLength: (right.len() * 2) as u16, Buffer: right.as_mut_ptr() };
        let st = LsaAddAccountRights(pol, sid.as_mut_ptr() as _, &us, 1);
        println!("grant SeServiceLogonRight to {account}: status={st:#x}");
    }
}

fn svc(name: &str) -> SC_HANDLE {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        let h = OpenServiceW(scm, wide(name).as_ptr(), SERVICE_QUERY_STATUS | SERVICE_STOP | SERVICE_START);
        assert!(!h.is_null(), "open {name}");
        h
    }
}

fn status(h: SC_HANDLE) -> SERVICE_STATUS_PROCESS {
    let mut p: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut need = 0u32;
    unsafe { QueryServiceStatusEx(h, SC_STATUS_PROCESS_INFO, &mut p as *mut _ as *mut u8, std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32, &mut need) };
    p
}

fn net(args: &[&str]) -> Option<i32> { Command::new("net.exe").args(args).output().unwrap().status.code() }

/// Open the running service's process, as goetia's pin would; report what worked.
fn open_pinned(h: SC_HANDLE, label: &str) -> HANDLE {
    let st = status(h);
    let ph = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, st.dwProcessId) };
    let err = if ph.is_null() { unsafe { GetLastError() } } else { 0 };
    let mut img = vec![0u16; 1024];
    let mut n = img.len() as u32;
    let got = if ph.is_null() { 0 } else { unsafe { QueryFullProcessImageNameW(ph, PROCESS_NAME_WIN32, img.as_mut_ptr(), &mut n) } };
    println!("{label}: state={} pid={} OpenProcess(QUERY_LIMITED|SYNCHRONIZE) ok={} err={err} image_ok={got} image={}",
        st.dwCurrentState, st.dwProcessId, !ph.is_null(), String::from_utf16_lossy(&img[..n as usize]));
    ph
}

fn stop_and_exit_code(h: SC_HANDLE, ph: HANDLE, bound_ms: u32) -> String {
    let mut s: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    unsafe { ControlService(h, SERVICE_CONTROL_STOP, &mut s) };
    if ph.is_null() { return "no process handle".into(); }
    let t0 = std::time::Instant::now();
    let w = unsafe { WaitForSingleObject(ph, bound_ms) };
    let waited = t0.elapsed();
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(ph, &mut code) };
    unsafe { CloseHandle(ph) };
    let st = status(h);
    format!("waited={waited:?} exited_within_bound={} exit_code={code:#x} ({code}) get_ok={ok} scm: state={} win32={} specific={}",
        w == WAIT_OBJECT_0, st.dwCurrentState, st.dwWin32ExitCode, st.dwServiceSpecificExitCode)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "grant" => grant(&a[2]),
        "pin" => {
            let h = svc(&a[2]);
            println!("  net start rc={:?}", net(&["start", &a[2]]));
            let ph = open_pinned(h, &format!("pin {}", a[2]));
            println!("pin {}: stop -> {}", a[2], stop_and_exit_code(h, ph, 30_000));
        }
        "race" => {
            let h = svc(&a[2]);
            let n: usize = a[3].parse().unwrap();
            let mut hist = std::collections::BTreeMap::<u32, usize>::new();
            for _ in 0..n {
                net(&["start", &a[2]]);
                let st = status(h);
                let ph = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, st.dwProcessId) };
                let r = stop_and_exit_code(h, ph, 30_000);
                let code: u32 = r.split("exit_code=").nth(1).unwrap().split(' ').next().map(|x| u32::from_str_radix(x.trim_start_matches("0x"), 16).unwrap()).unwrap();
                *hist.entry(code).or_default() += 1;
            }
            println!("race {} x{n}: process exit codes {hist:?} (service thread exits 6; main exits 0 once the dispatcher returns)", a[2]);
        }
        "linger" => {
            let h = svc(&a[2]);
            println!("  net start rc={:?}", net(&["start", &a[2]]));
            let ph = open_pinned(h, &format!("linger {}", a[2]));
            println!("linger {}: reports STOPPED(6) then never exits; within 300s -> {}", a[2], stop_and_exit_code(h, ph, 300_000));
        }
        _ => unreachable!(),
    }
}
