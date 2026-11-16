// What a task's action does. `tsprobe act <mode> ...`
//   noop                                   exit 0 (the pid churn of L1)
//   rec <log> <exit> [emit=<token>] [spin_ms=<n>]
//                                          append pid/creation/time to <log>; with an `emit` token file that it can
//                                          delete, write an Application event first (an EventTrigger turns it into a
//                                          second start); spin <n> ms; exit
//   hold <log> <event> <exit>              record, then wait on the named event, then exit
//   ctrl <log> <spec> [child=plain|job|jobkill]
//                                          a console action with a CTRL_CLOSE_EVENT handler (M6), see `Spec`
//   ctrlchild <log> <self-deadline-secs>   the shim's child: same console, ignores every console event
//   self <out> [task=<path>] [rundir=<dir>]
//                                          report the token, env, cwd, argv, priority, COM reads, a run-record write (M8, M9, M11)
//   prio <out>                             report the priority class and exit
use crate::util::*;
use std::sync::OnceLock;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::*;
use windows::Win32::System::Console::*;
use windows::Win32::System::Environment::GetCommandLineW;
use windows::Win32::System::EventLog::*;
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::CommandLineToArgvW;

pub fn run(a: &[String]) -> i32 {
    match a.first().map(String::as_str) {
        Some("noop") => 0,
        Some("rec") => rec(&a[1..]),
        Some("hold") => hold(&a[1..]),
        Some("ctrl") => ctrl(&a[1..]),
        Some("ctrlchild") => ctrlchild(&a[1..]),
        Some("self") => self_report(&a[1..]),
        Some("prio") => {
            let out = &a[1];
            append(out, &format!("child-priority-class=0x{:X}", unsafe { GetPriorityClass(GetCurrentProcess()) }));
            0
        }
        _ => 2,
    }
}

fn record(log: &str, what: &str) {
    append(log, &format!("pid={} ct={} t={} {what}", std::process::id(), self_ct(), now_ft()));
}

fn spin_ms(n: f64) {
    let t0 = now_ft();
    while ms(t0, now_ft()) < n {
        std::hint::spin_loop();
    }
}

fn rec(a: &[String]) -> i32 {
    let log = &a[0];
    let code: u32 = a[1].parse().unwrap_or(0);
    record(log, "");
    for opt in &a[2..] {
        if let Some(tok) = opt.strip_prefix("emit=") {
            if std::fs::remove_file(tok).is_ok() {
                emit_event();
            }
        }
        if let Some(n) = opt.strip_prefix("spin_ms=") {
            spin_ms(n.parse().unwrap_or(0.0));
        }
    }
    code as i32
}

fn emit_event() {
    // SAFETY: the source handle is deregistered before return; the string array is live.
    unsafe {
        let Ok(h) = RegisterEventSourceW(PCWSTR::null(), w!("GoetiaProbe")) else {
            return;
        };
        let s = w!("goetia probe event");
        let _ = ReportEventW(h, EVENTLOG_INFORMATION_TYPE, 0, 1000, None, 0, Some(&[s]), None);
        let _ = DeregisterEventSource(h);
    }
}

fn hold(a: &[String]) -> i32 {
    let (log, ev, code) = (&a[0], &a[1], a[2].parse::<u32>().unwrap_or(0));
    record(log, "");
    // SAFETY: the event name is a NUL-terminated wide string made here.
    unsafe {
        let name: Vec<u16> = ev.encode_utf16().chain(std::iter::once(0)).collect();
        match OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) {
            Ok(h) => {
                WaitForSingleObject(h, INFINITE);
            }
            Err(e) => append(log, &format!("open event failed {}", hr(&e))),
        }
    }
    code as i32
}

// ---- M6 ---------------------------------------------------------------------------------------------------------

static LOG: OnceLock<String> = OnceLock::new();
static SPEC: OnceLock<String> = OnceLock::new();
static STOPREQ: OnceLock<isize> = OnceLock::new();
static NEVER: OnceLock<isize> = OnceLock::new();

fn mk_event() -> HANDLE {
    // SAFETY: an unnamed manual-reset event.
    unsafe { CreateEventW(None, true, false, PCWSTR::null()).expect("CreateEventW") }
}

fn h(v: isize) -> HANDLE {
    HANDLE(v as *mut core::ffi::c_void)
}

unsafe extern "system" fn handler(ev: u32) -> BOOL {
    let log = LOG.get().unwrap();
    record(log, &format!("handler-enter event={ev}"));
    if ev != 2 {
        return BOOL(0);
    }
    let spec = SPEC.get().unwrap().clone();
    let parts: Vec<&str> = spec.split(':').collect();
    match parts[0] {
        "block" => {
            spin_sleep(parts[1].parse().unwrap_or(0));
            record(log, "handler-return-TRUE");
            BOOL(1)
        }
        "exit" => {
            spin_sleep(parts[1].parse().unwrap_or(0));
            let code: u32 = parts[2].parse().unwrap_or(0);
            record(log, &format!("handler-ExitProcess({code})"));
            ExitProcess(code)
        }
        "never" => {
            // Ask the run thread to end the process, then never return.
            let _ = SetEvent(h(*STOPREQ.get().unwrap()));
            WaitForSingleObject(h(*NEVER.get().unwrap()), INFINITE);
            BOOL(1)
        }
        _ => BOOL(0),
    }
}

/// The handler's own blocking time is the independent variable of the experiment, not a synchronisation.
fn spin_sleep(secs: u64) {
    // SAFETY: plain call.
    unsafe { Sleep((secs * 1000) as u32) }
}

fn ctrl(a: &[String]) -> i32 {
    let log = a[0].clone();
    let spec = a[1].clone();
    LOG.set(log.clone()).ok();
    SPEC.set(spec.clone()).ok();
    STOPREQ.set(mk_event().0 as isize).ok();
    NEVER.set(mk_event().0 as isize).ok();
    // SAFETY: the handler is a plain fn that outlives the process.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
    record(&log, &format!("start spec={spec}"));
    for opt in &a[2..] {
        if let Some(kind) = opt.strip_prefix("child=") {
            spawn_child(&log, kind);
        }
    }
    record(&log, "ready");
    let parts: Vec<&str> = spec.split(':').collect();
    // SAFETY: plain waits on events owned here.
    unsafe {
        if parts[0] == "never" {
            WaitForSingleObject(h(*STOPREQ.get().unwrap()), INFINITE);
            let code: u32 = parts[2].parse().unwrap_or(0);
            record(&log, &format!("run-thread-ends-process how={} code={code}", parts[1]));
            match parts[1] {
                "exitprocess" => ExitProcess(code),
                "terminate" => {
                    let _ = TerminateProcess(GetCurrentProcess(), code);
                    WaitForSingleObject(h(*NEVER.get().unwrap()), INFINITE);
                }
                _ => std::process::exit(code as i32),
            }
        } else {
            WaitForSingleObject(mk_event(), INFINITE);
        }
    }
    0
}

fn spawn_child(log: &str, kind: &str) {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().unwrap();
    // CREATE_NEW_PROCESS_GROUP, as the shim's child is created.
    let child = std::process::Command::new(exe).args(["act", "ctrlchild", log, "60"]).creation_flags(0x0000_0200).spawn();
    let Ok(child) = child else {
        record(log, "child spawn failed");
        return;
    };
    record(log, &format!("child-spawned pid={} kind={kind}", child.id()));
    if kind != "plain" {
        // SAFETY: the job handle is intentionally kept for the life of the process.
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).expect("job");
            if kind == "jobkill" {
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let _ = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of_val(&info) as u32,
                );
            }
            let r = AssignProcessToJobObject(job, HANDLE(child.as_raw_handle()));
            record(log, &format!("child-in-job {kind} {}", if r.is_ok() { "ok" } else { "failed" }));
        }
    }
    std::mem::forget(child);
}

unsafe extern "system" fn child_handler(ev: u32) -> BOOL {
    record(LOG.get().unwrap(), &format!("child-handler-enter event={ev} (returns TRUE: ignored)"));
    BOOL(1)
}

fn ctrlchild(a: &[String]) -> i32 {
    LOG.set(a[0].clone()).ok();
    let secs: u32 = a[1].parse().unwrap_or(60);
    // SAFETY: the handler is a plain fn.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(child_handler), true);
    }
    record(&a[0], "child-start");
    // The child's own bound, so the probe cannot hang: it exits with 77 if nobody ended it.
    // SAFETY: plain wait on an event owned here.
    unsafe {
        WaitForSingleObject(mk_event(), secs * 1000);
    }
    record(&a[0], "child-self-exit 77");
    77
}

// ---- M8, M9, M11 --------------------------------------------------------------------------------------------------

fn self_report(a: &[String]) -> i32 {
    let out = a[0].clone();
    let o = |s: String| {
        println!("{s}");
        append(&out, &s);
    };
    let mut task_path = String::new();
    let mut rundir = String::new();
    for opt in &a[1..] {
        if let Some(v) = opt.strip_prefix("task=") {
            task_path = v.to_owned();
        }
        if let Some(v) = opt.strip_prefix("rundir=") {
            rundir = v.to_owned();
        }
    }
    o(format!("== self pid={} ==", std::process::id()));
    // argv: three readings.
    let args: Vec<String> = std::env::args().collect();
    o(format!("argc={}", args.len()));
    for (i, s) in args.iter().enumerate() {
        o(format!("argv[{i}]=<{s}>"));
    }
    // SAFETY: GetCommandLineW returns a process-lifetime NUL-terminated string; CommandLineToArgvW's result is
    // read within the same unsafe block and left to the OS to reclaim at exit.
    unsafe {
        let cl = GetCommandLineW();
        o(format!("rawcmdline=<{}>", cl.to_string().unwrap_or_default()));
        let mut n = 0i32;
        let v = CommandLineToArgvW(cl, &mut n);
        for i in 0..n as usize {
            o(format!("CommandLineToArgvW[{i}]=<{}>", (*v.add(i)).to_string().unwrap_or_default()));
        }
    }
    o(format!("cwd={}", std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|e| format!("err {e}"))));
    o(format!("exe={}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()));
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.sort();
    o(format!("envcount={}", env.len()));
    if !a.iter().any(|x| x == "noenv") {
        for (k, v) in &env {
            o(format!("env:{k}={v}"));
        }
    }
    // Priority: this process and a child.
    // SAFETY: plain calls.
    unsafe {
        o(format!("priority-class=0x{:X}", GetPriorityClass(GetCurrentProcess())));
    }
    let exe = std::env::current_exe().unwrap();
    match std::process::Command::new(exe).args(["act", "prio", &out]).status() {
        Ok(s) => o(format!("child-status={s}")),
        Err(e) => o(format!("child-spawn-failed {e}")),
    }
    token(&o);
    if !task_path.is_empty() {
        com_reads(&o, &task_path);
    }
    if !rundir.is_empty() {
        write_record(&o, &rundir);
    }
    o("== done ==".into());
    0
}

fn token(o: &dyn Fn(String)) {
    // SAFETY: every buffer is sized by the preceding query; the token handle is closed.
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            o("token: OpenProcessToken failed".into());
            return;
        }
        let mut need = 0u32;
        let _ = GetTokenInformation(tok, TokenUser, None, 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        if GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr().cast()), need, &mut need).is_ok() {
            let tu = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s = PWSTR::null();
            if ConvertSidToStringSidW(tu.User.Sid, &mut s).is_ok() {
                o(format!("token-user-sid={}", s.to_string().unwrap_or_default()));
            }
        }
        let mut el = TOKEN_ELEVATION::default();
        if GetTokenInformation(tok, TokenElevation, Some((&mut el as *mut TOKEN_ELEVATION).cast()), 4, &mut need).is_ok() {
            o(format!("token-elevated={}", el.TokenIsElevated));
        }
        let mut ty = TOKEN_ELEVATION_TYPE(0);
        if GetTokenInformation(tok, TokenElevationType, Some((&mut ty as *mut TOKEN_ELEVATION_TYPE).cast()), 4, &mut need).is_ok() {
            o(format!("token-elevation-type={} (1 default, 2 full, 3 limited)", ty.0));
        }
        let mut need = 0u32;
        let _ = GetTokenInformation(tok, TokenIntegrityLevel, None, 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        if GetTokenInformation(tok, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), need, &mut need).is_ok() {
            let l = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let cnt = *GetSidSubAuthorityCount(l.Label.Sid);
            o(format!("token-integrity-rid=0x{:X}", *GetSidSubAuthority(l.Label.Sid, (cnt - 1) as u32)));
        }
        let mut sid = PSID::default();
        if ConvertStringSidToSidW(w!("S-1-5-32-544"), &mut sid).is_ok() {
            let mut member = BOOL(0);
            let _ = CheckTokenMembership(None, sid, &mut member);
            o(format!("token-administrators-enabled={}", member.as_bool()));
        }
        let _ = CloseHandle(tok);
    }
}

fn com_reads(o: &dyn Fn(String), path: &str) {
    use windows::Win32::System::Variant::VARIANT;
    com_init();
    let (folder_path, name) = path.rsplit_once('\\').unwrap();
    let svc = match connect() {
        Ok(s) => s,
        Err(e) => {
            o(format!("com: Connect failed {}", hr(&e)));
            return;
        }
    };
    o("com: Connect ok".into());
    // SAFETY: plain COM calls.
    unsafe {
        let folder = match svc.GetFolder(&BSTR::from(folder_path)) {
            Ok(f) => {
                o(format!("com: GetFolder({folder_path}) ok"));
                f
            }
            Err(e) => {
                o(format!("com: GetFolder({folder_path}) failed {}", hr(&e)));
                return;
            }
        };
        match folder.GetTask(&BSTR::from(name)) {
            Ok(t) => {
                o(format!("com: GetTask({name}) ok"));
                match t.Definition() {
                    Ok(d) => {
                        let mut data = BSTR::new();
                        match d.Data(&mut data) {
                            Ok(()) => {
                                let s = data.to_string();
                                o(format!("com: Definition.Data len={} head=<{}>", s.chars().count(), s.chars().take(24).collect::<String>()));
                            }
                            Err(e) => o(format!("com: Definition.Data failed {}", hr(&e))),
                        }
                    }
                    Err(e) => o(format!("com: Definition failed {}", hr(&e))),
                }
                match t.Xml() {
                    Ok(x) => o(format!("com: Xml len={}", x.to_string().chars().count())),
                    Err(e) => o(format!("com: Xml failed {}", hr(&e))),
                }
                match instances(&t) {
                    Ok(l) => {
                        let me = std::process::id();
                        let mine: Vec<_> = l.iter().filter(|x| x.pid == me).collect();
                        o(format!(
                            "com: GetInstances count={} own-by-pid={} guid={}",
                            l.len(),
                            mine.len(),
                            mine.first().map(|m| m.guid.clone()).unwrap_or_default()
                        ));
                    }
                    Err(e) => o(format!("com: GetInstances failed {}", hr(&e))),
                }
                o(format!(
                    "com: LastTaskResult={:?} NextRunTime={:?}",
                    t.LastTaskResult().map(|v| format!("0x{:08X}", v as u32)).map_err(|e| hr(&e)),
                    t.NextRunTime().map_err(|e| hr(&e))
                ));
            }
            Err(e) => o(format!("com: GetTask({name}) failed {}", hr(&e))),
        }
        match folder.GetTasks(1) {
            Ok(c) => o(format!("com: folder.GetTasks(hidden) ok count={:?}", c.Count().map_err(|e| hr(&e)))),
            Err(e) => o(format!("com: folder.GetTasks(hidden) failed {}", hr(&e))),
        }
        match folder.GetSecurityDescriptor(0x4 /* DACL */) {
            Ok(s) => o(format!("com: folder SDDL(dacl)={}", s)),
            Err(e) => o(format!("com: folder GetSecurityDescriptor failed {}", hr(&e))),
        }
        match svc.GetFolder(&BSTR::from("\\")).and_then(|r| r.GetTasks(0)) {
            Ok(c) => o(format!("com: root GetTasks count={:?}", c.Count().map_err(|e| hr(&e)))),
            Err(e) => o(format!("com: root GetTasks failed {}", hr(&e))),
        }
        match folder.CreateFolder(&BSTR::from("probe-sub"), &VARIANT::default()) {
            Ok(_) => {
                o("com: CreateFolder(probe-sub) ok".into());
                let _ = folder.DeleteFolder(&BSTR::from("probe-sub"), 0);
            }
            Err(e) => o(format!("com: CreateFolder(probe-sub) failed {}", hr(&e))),
        }
    }
}

fn write_record(o: &dyn Fn(String), dir: &str) {
    let tmp = format!("{dir}\\probe-{}.tmp", std::process::id());
    let fin = format!("{dir}\\probe-{}.json", std::process::id());
    let r = |what: &str, r: std::io::Result<()>| match r {
        Ok(()) => o(format!("record: {what} ok")),
        Err(e) => o(format!("record: {what} failed os={:?} {e}", e.raw_os_error())),
    };
    r("write temp", std::fs::write(&tmp, b"{\"x\":1}"));
    r("rename temp to final", std::fs::rename(&tmp, &fin));
    r("read back", std::fs::read(&fin).map(|_| ()));
    match std::fs::read_dir(dir) {
        Ok(d) => o(format!("record: list dir ok entries={}", d.count())),
        Err(e) => o(format!("record: list dir failed os={:?}", e.raw_os_error())),
    }
    r("delete own file", std::fs::remove_file(&fin));
}
