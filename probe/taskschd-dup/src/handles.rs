// M5. `handles <task> <event> <schedule-service-pid> <runs> <actlog>`
//
// The task's action is `tsprobe act hold <actlog> <event> 0`: it waits on a named event. Per run: Run the task, open
// the action's process ourselves (that handle gives us the kernel object's address in the system handle table),
// list who else holds a handle to the same process object, signal the event so the action exits, and then watch
// (a tight loop, no waits) the holders and the listing until nothing is listed. A line is printed whenever either changes.
use crate::util::*;
use std::collections::BTreeSet;
use std::ffi::c_void;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Security::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::Variant::VARIANT;

#[link(name = "ntdll")]
extern "system" {
    fn NtQuerySystemInformation(class: u32, buf: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    object: usize,
    pid: usize,
    handle: usize,
    access: u32,
    backtrace: u16,
    type_index: u16,
    attrs: u32,
    reserved: u32,
}

struct Snap {
    words: Vec<u64>,
}

impl Snap {
    fn take() -> Snap {
        let mut words = vec![0u64; 4 * 1024 * 1024 / 8];
        loop {
            let mut ret = 0u32;
            // SAFETY: the buffer is live and `len` bytes long.
            let st = unsafe { NtQuerySystemInformation(64, words.as_mut_ptr().cast(), (words.len() * 8) as u32, &mut ret) };
            if st == 0 {
                return Snap { words };
            }
            if st as u32 == 0xC000_0004 {
                let n = words.len() * 2;
                words.resize(n, 0);
                continue;
            }
            panic!("NtQuerySystemInformation(SystemExtendedHandleInformation) = 0x{:08X}", st as u32);
        }
    }
    fn entries(&self) -> &[Entry] {
        let base = self.words.as_ptr().cast::<u8>();
        // SAFETY: a successful call wrote a count and then that many 40-byte entries.
        unsafe {
            let n = *(base as *const usize);
            std::slice::from_raw_parts(base.add(16).cast::<Entry>(), n)
        }
    }
}

/// The image name of a process we cannot open (a protected one), or of one that is still alive but unnamed otherwise.
fn toolhelp_name(pid: u32) -> String {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    // SAFETY: the snapshot handle is closed before return; the entry struct is live and sized.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return "?".into();
        };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = String::from("?(exited or unlisted)");
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            if e.th32ProcessID == pid {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                found = String::from_utf16_lossy(&e.szExeFile[..n]);
                break;
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

fn enable_debug_privilege() -> bool {
    // SAFETY: plain calls; the token handle is closed.
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), SE_DEBUG_NAME, &mut luid).is_err() {
            return false;
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let ok = AdjustTokenPrivileges(tok, false, Some(&tp), 0, None, None).is_ok();
        let _ = CloseHandle(tok);
        ok
    }
}

pub fn named_event(name: &str) -> HANDLE {
    // SAFETY: the security descriptor lives across the call; a NULL DACL grants everyone access.
    unsafe {
        let mut sd = SECURITY_DESCRIPTOR::default();
        let psd = PSECURITY_DESCRIPTOR((&mut sd as *mut SECURITY_DESCRIPTOR).cast());
        windows::Win32::Security::InitializeSecurityDescriptor(psd, 1).expect("InitializeSecurityDescriptor");
        windows::Win32::Security::SetSecurityDescriptorDacl(psd, true, None, false).expect("SetSecurityDescriptorDacl");
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd.0, bInheritHandle: BOOL(0) };
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        CreateEventW(Some(&sa), true, false, PCWSTR(wide.as_ptr())).expect("CreateEventW named")
    }
}

pub fn run(a: &[String]) -> i32 {
    com_init();
    let path = a[0].as_str();
    let evname = a[1].as_str();
    let sched_pid: usize = a[2].parse().unwrap();
    let runs: usize = a[3].parse().unwrap();
    let actlog = a[4].as_str();
    let dbg = enable_debug_privilege();
    println!("##### handles task={path} schedule-service-pid={sched_pid} SeDebugPrivilege-enabled={dbg}");
    let ev = named_event(evname);
    let t = task(path).expect("task");
    let me = std::process::id() as usize;
    let mut name_cache = std::collections::BTreeMap::<usize, String>::new();
    let mut name = |pid: usize| -> String {
        name_cache
            .entry(pid)
            .or_insert_with(|| {
                let img = image_of(pid as u32);
                let base = if img == "?" { toolhelp_name(pid as u32) } else { img.rsplit('\\').next().unwrap_or("?").to_owned() };
                format!("{pid}:{base}")
            })
            .clone()
    };
    let _ = actlog;
    for i in 1..=runs {
        // SAFETY: plain calls.
        unsafe {
            let _ = ResetEvent(ev);
        }
        let rt = unsafe { t.Run(&VARIANT::default()) }.expect("Run");
        let guid = unsafe { rt.InstanceGuid() }.map(|b| b.to_string()).unwrap_or_default();
        let pid = unsafe { rt.EnginePID() }.unwrap_or(0);
        // Our own handle to the action's process, to find its kernel object.
        let hproc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) };
        let Ok(hproc) = hproc else {
            println!("run {i}: pid {pid}: OpenProcess failed");
            unsafe {
                let _ = SetEvent(ev);
            }
            continue;
        };
        let snap = Snap::take();
        let obj = snap.entries().iter().find(|e| e.pid == me && e.handle == hproc.0 as usize).map(|e| e.object).unwrap_or(0);
        println!("run {i}: guid={guid} action pid={pid} object=0x{obj:X} (0 means the kernel address was hidden)");
        if obj == 0 {
            unsafe {
                let _ = SetEvent(ev);
                WaitForSingleObject(hproc, INFINITE);
                let _ = CloseHandle(hproc);
            }
            continue;
        }
        let holders_of = |s: &Snap| -> BTreeSet<usize> { s.entries().iter().filter(|e| e.object == obj && e.pid != me).map(|e| e.pid).collect() };
        let h0 = holders_of(&snap);
        let desc = |s: &BTreeSet<usize>, name: &mut dyn FnMut(usize) -> String| s.iter().map(|p| name(*p)).collect::<Vec<_>>().join(", ");
        println!("  while alive: holders other than the probe: [{}] (schedule service pid {sched_pid} holds: {})", desc(&h0, &mut name), h0.contains(&sched_pid));
        let t_rel = now_ft();
        unsafe {
            let _ = SetEvent(ev);
        }
        let mut last_h = h0.clone();
        let mut last_listed = true;
        let mut ended_seen = false;
        let mut drop_t = None;
        loop {
            let s = Snap::take();
            let hs = holders_of(&s);
            let ts = now_ft();
            let listed = instances(&t).map(|l| l.iter().any(|x| same_guid(&x.guid, &guid))).unwrap_or(true);
            let exited = unsafe { WaitForSingleObject(hproc, 0) } == WAIT_OBJECT_0;
            if exited && !ended_seen {
                ended_seen = true;
                println!("  +{:.1} ms: the process is signalled (exited); listed={listed}; holders [{}]", ms(t_rel, ts), desc(&hs, &mut name));
            }
            if hs != last_h || listed != last_listed {
                println!(
                    "  +{:.1} ms: listed={listed} exited={exited} holders [{}] schedule-service-holds={}",
                    ms(t_rel, ts),
                    desc(&hs, &mut name),
                    hs.contains(&sched_pid)
                );
                last_h = hs;
                last_listed = listed;
            }
            if !listed {
                drop_t.get_or_insert(ts);
                // One more snapshot after the drop is the one just taken; stop.
                println!("  +{:.1} ms: dropped from the listing; holders at the previous snapshot [{}]", ms(t_rel, ts), desc(&last_h, &mut name));
                let after = holders_of(&Snap::take());
                println!("  after the drop, a fresh snapshot: holders [{}]", desc(&after, &mut name));
                break;
            }
        }
        unsafe {
            let _ = CloseHandle(hproc);
        }
    }
    0
}
