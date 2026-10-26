//! THROWAWAY (issue #59, phase A): measures how the Windows SCM treats a
//! service's reported exit, so the shim's design rests on measurements.
//! Round 14: P27 (a re-arm on an unchanged state that is in the mask). Earlier rounds' probes stay in the file, unused. Never merges. Every observation is a `PROBE ...` line in the job log.
//!
//! This binary is also the service host: `<exe> --probe-host <name> <port> <scripts>`
//! where `<scripts>` is `|`-separated, one script per run of the service
//! (run index kept in `%ProgramData%\<name>.runs`; the last script repeats).
//! A script is `,`-separated steps: `pending`, `run`, `connect`, `wait-stop`,
//! `stopped:w32:<n>`, `stopped:spec:<n>`, `rawstopped:<w32>:<spec>`, `exit:<n>`,
//! `preexit:<n>` (as the run's only step: connect back, then exit before `StartServiceCtrlDispatcherW`),
//! `hold:<ms>` (stay in the state reported so far, e.g. START_PENDING), `stoppending[:<hint_ms>]`
//! (report STOP_PENDING, checkpoint 1, hint 10000 ms unless given, tracing whether
//! `SetServiceStatus` succeeded), `signal-wait` (block
//! until the probe fires the named event `Global\<name>-go`).
//!
//! Absence is shown with a bound (a one-off attended measurement; the result
//! is read by a human as "no restart within N s", not as proof).

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    if host::run_if_requested() {
        return;
    }
    probes::run_all();
}

#[cfg(windows)]
mod host {
    use std::ffi::{OsString, c_void};
    use std::io::Write as _;
    use std::net::{Ipv4Addr, TcpStream};
    use std::os::windows::ffi::OsStrExt as _;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use windows_service::{define_windows_service, service_dispatcher};
    use windows_sys::Win32::Foundation;
    use windows_sys::Win32::System::Services;
    use windows_sys::Win32::System::Threading;

    pub const HOST: &str = "--probe-host";

    /// Standard access right (`windows-sys` has it only behind the FileSystem feature).
    const SYNCHRONIZE: u32 = 0x0010_0000;

    static REFUSE_ONE_STOP: AtomicBool = AtomicBool::new(false);

    /// This run's index, taken in `run_if_requested` (before the dispatcher is started).
    static RUN_IDX: AtomicUsize = AtomicUsize::new(0);

    /// `Global\<name>-go`, NUL-terminated UTF-16: the event a `signal-wait` step blocks on.
    pub fn signal_name(name: &str) -> Vec<u16> {
        std::ffi::OsStr::new(&format!("Global\\{name}-go"))
            .encode_wide()
            .chain([0])
            .collect()
    }

    pub fn run_if_requested() -> bool {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) != Some(HOST) {
            return false;
        }
        let idx = next_run_index(&args[2]);
        let scripts: Vec<&str> = args[4].split('|').collect();
        if let Some(n) = scripts[idx.min(scripts.len() - 1)].strip_prefix("preexit:") {
            trace(&args[2], &format!("run={idx} preexit, exit {n}"));
            connect_back(args[3].parse().expect("port"), idx);
            std::process::exit(code(n) as i32);
        }
        RUN_IDX.store(idx, Ordering::SeqCst);
        service_dispatcher::start(&args[2], ffi_service_main).expect("connect to the SCM dispatcher");
        true
    }

    fn connect_back(port: u16, idx: usize) {
        if let Ok(mut s) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            let _ = write!(s, "run={idx} pid={}", std::process::id());
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
    }

    define_windows_service!(ffi_service_main, service_main);

    fn next_run_index(name: &str) -> usize {
        let dir = std::env::var_os("ProgramData").expect("%ProgramData%");
        let path = std::path::PathBuf::from(dir).join(format!("{name}.runs"));
        let idx = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        std::fs::write(&path, (idx + 1).to_string()).expect("write run counter");
        idx
    }

    /// `%ProgramData%\<name>.trace`: one line per host event, with Unix-epoch microseconds, so the
    /// probe can see what the host did and when, including a panic.
    pub fn trace_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::var_os("ProgramData").expect("%ProgramData%");
        std::path::PathBuf::from(dir).join(format!("{name}.trace"))
    }

    fn trace(name: &str, msg: &str) {
        let us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(trace_path(name))
        {
            let _ = writeln!(f, "us={us} pid={} {msg}", std::process::id());
        }
    }

    fn code(s: &str) -> u32 {
        match s.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).expect("hex code"),
            None => s.parse().expect("decimal code"),
        }
    }

    /// Never freed: a control may arrive at any time up to process exit (the `windows-service`
    /// crate frees its handler closure after a STOP, so a later control would reach freed memory).
    struct HandlerCtx {
        name: String,
        idx: usize,
        stop_tx: Mutex<mpsc::Sender<()>>,
    }

    unsafe extern "system" fn on_control(
        control: u32,
        _event_type: u32,
        _event_data: *mut c_void,
        ctx: *mut c_void,
    ) -> u32 {
        // SAFETY: `ctx` is the leaked `HandlerCtx` registered in `service_main`.
        let ctx = unsafe { &*(ctx as *const HandlerCtx) };
        let result = match control {
            // As goetia-shim refuses a stop once it has decided on a failure.
            Services::SERVICE_CONTROL_STOP if REFUSE_ONE_STOP.swap(false, Ordering::SeqCst) => {
                Foundation::ERROR_SERVICE_CANNOT_ACCEPT_CTRL
            }
            Services::SERVICE_CONTROL_STOP => {
                if let Ok(tx) = ctx.stop_tx.lock() {
                    let _ = tx.send(());
                }
                Foundation::NO_ERROR
            }
            Services::SERVICE_CONTROL_INTERROGATE => Foundation::NO_ERROR,
            _ => Foundation::ERROR_CALL_NOT_IMPLEMENTED,
        };
        trace(
            &ctx.name,
            &format!("run={} handler control={control} -> {result}", ctx.idx),
        );
        result
    }

    /// A raw `SetServiceStatus`; the win32 and service-specific codes are independent.
    fn report(
        handle: Services::SERVICE_STATUS_HANDLE,
        state: u32,
        accepted: u32,
        (w32, spec): (u32, u32),
        checkpoint: u32,
        wait_hint_ms: u32,
    ) -> Result<(), std::io::Error> {
        let raw = Services::SERVICE_STATUS {
            dwServiceType: Services::SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: w32,
            dwServiceSpecificExitCode: spec,
            dwCheckPoint: checkpoint,
            dwWaitHint: wait_hint_ms,
        };
        // SAFETY: `handle` came from `RegisterServiceCtrlHandlerExW`; `raw` is a valid SERVICE_STATUS.
        if unsafe { Services::SetServiceStatus(handle, &raw) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn service_main(_scm_arguments: Vec<OsString>) {
        let args: Vec<String> = std::env::args().collect();
        let name = args[2].clone();
        let port: u16 = args[3].parse().expect("port");
        let scripts: Vec<&str> = args[4].split('|').collect();
        let idx = RUN_IDX.load(Ordering::SeqCst);
        let script = scripts[idx.min(scripts.len() - 1)];
        let hook_name = name.clone();
        std::panic::set_hook(Box::new(move |info| {
            trace(&hook_name, &format!("run={idx} PANIC {info}"))
        }));
        trace(&name, &format!("run={idx} service_main script={script}"));

        let (stop_tx, stop_rx) = mpsc::channel();
        let ctx: &'static HandlerCtx = Box::leak(Box::new(HandlerCtx {
            name: name.clone(),
            idx,
            stop_tx: Mutex::new(stop_tx),
        }));
        let wide_name: Vec<u16> = std::ffi::OsStr::new(&name).encode_wide().chain([0]).collect();
        // SAFETY: `wide_name` is NUL-terminated; `ctx` is never freed.
        let handle = unsafe {
            Services::RegisterServiceCtrlHandlerExW(
                wide_name.as_ptr(),
                Some(on_control),
                (ctx as *const HandlerCtx).cast_mut().cast(),
            )
        };
        assert!(
            !handle.is_null(),
            "register control handler: {}",
            std::io::Error::last_os_error()
        );

        for step in script.split(',') {
            trace(&name, &format!("run={idx} step {step}"));
            let parts: Vec<&str> = step.split(':').collect();
            match parts.as_slice() {
                ["pending"] => {
                    report(handle, Services::SERVICE_START_PENDING, 0, (0, 0), 1, 10_000).expect("report START_PENDING")
                }
                ["run"] => report(
                    handle,
                    Services::SERVICE_RUNNING,
                    Services::SERVICE_ACCEPT_STOP,
                    (0, 0),
                    0,
                    0,
                )
                .expect("report RUNNING"),
                ["stoppending"] | ["stoppending", _] => {
                    let hint = parts.get(1).map_or(10_000, |ms| code(ms));
                    let r = report(handle, Services::SERVICE_STOP_PENDING, 0, (0, 0), 1, hint);
                    trace(
                        &name,
                        &format!("run={idx} SetServiceStatus(STOP_PENDING, checkpoint 1, waithint {hint}) -> {r:?}"),
                    );
                }
                ["signal-wait"] => {
                    // SAFETY: a NUL-terminated name; the handle is closed right after the wait.
                    let ev = unsafe { Threading::OpenEventW(SYNCHRONIZE, 0, signal_name(&name).as_ptr()) };
                    assert!(!ev.is_null(), "OpenEventW: {}", std::io::Error::last_os_error());
                    let rc = unsafe { Threading::WaitForSingleObject(ev, Threading::INFINITE) };
                    unsafe { Foundation::CloseHandle(ev) };
                    trace(&name, &format!("run={idx} signal-wait -> WaitForSingleObject {rc:#x}"));
                }
                ["connect"] => connect_back(port, idx),
                ["wait-stop"] => stop_rx.recv().expect("stop control"),
                ["refuse-one-stop"] => REFUSE_ONE_STOP.store(true, Ordering::SeqCst),
                ["hold", ms] => std::thread::sleep(Duration::from_millis(u64::from(code(ms)))),
                ["stopped", kind, n] => {
                    let n = code(n);
                    let exit_code = match *kind {
                        "w32" => (n, 0),
                        // What `ServiceExitCode::ServiceSpecific` sends: 1066 plus the code.
                        "spec" => (Foundation::ERROR_SERVICE_SPECIFIC_ERROR, n),
                        other => panic!("bad kind {other}"),
                    };
                    report(handle, Services::SERVICE_STOPPED, 0, exit_code, 0, 0).expect("report STOPPED");
                }
                // A raw pair `stopped` cannot say (win32 code and service-specific code independent).
                ["rawstopped", w32, spec] => {
                    report(handle, Services::SERVICE_STOPPED, 0, (code(w32), code(spec)), 0, 0)
                        .expect("report STOPPED");
                }
                ["exit", n] => std::process::exit(code(n) as i32),
                other => panic!("bad step {other:?}"),
            }
        }
        trace(&name, &format!("run={idx} service_main end"));
    }
}

#[cfg(windows)]
#[allow(dead_code, unused_imports)]
mod probes {
    use std::ffi::c_void;
    use std::io::Read as _;
    use std::net::{Ipv4Addr, TcpListener};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows_service::service::{
        Service, ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::System::Services;
    use windows_sys::Win32::System::Threading::SleepEx;
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    use super::host::HOST;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Failure bound for an expected observation (a restart that should
    /// happen); not a synchronization.
    const EXPECTED_BOUND: Duration = Duration::from_secs(90);

    fn absence_bound(delay: Duration) -> Duration {
        delay * 3 + Duration::from_secs(30)
    }

    fn sc(args: &[&str]) -> (Option<i32>, String) {
        let out = Command::new("sc.exe").args(args).output().expect("spawn sc.exe");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// `t=<ms since t0>ms wall=<UTC ISO-8601>`, so a PROBE line can be lined
    /// up with an event-log `SystemTime`.
    fn stamp(t0: Instant) -> String {
        format!("t={}ms wall={}", t0.elapsed().as_millis(), iso_now())
    }

    fn iso_now() -> String {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let secs = d.as_secs() as i64;
        let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60,
            d.subsec_millis()
        )
    }

    #[derive(Clone)]
    struct Connect {
        at: Duration,
        body: String,
    }

    // NotifyServiceStatusChangeW ======================================================================================

    /// Leaked: the SCM may still hold a pointer to it when its thread ends.
    struct NotifyCtx {
        tag: String,
        t0: Instant,
        /// State last delivered; the next registration masks it out (a mask
        /// that includes the current state fires at once, forever).
        last: AtomicU32,
        fired: AtomicBool,
        /// `dwNotificationStatus` of a delivery that was an error.
        err: AtomicU32,
        count: AtomicU32,
        /// Nanoseconds since `t0` at which the last delivery ran.
        at_ns: AtomicU64,
    }

    unsafe extern "system" fn on_notify(p: *const c_void) {
        // SAFETY: the SCM hands back the SERVICE_NOTIFY_2W registered below,
        // whose pContext is the leaked `NotifyCtx`.
        let n = unsafe { &*(p as *const Services::SERVICE_NOTIFY_2W) };
        let ctx = unsafe { &*(n.pContext as *const NotifyCtx) };
        ctx.at_ns.store(ctx.t0.elapsed().as_nanos() as u64, Ordering::SeqCst);
        let s = &n.ServiceStatus;
        let seen = ctx.count.load(Ordering::SeqCst);
        // A re-arm storm (P20) would flood the log: print the first 20 only.
        if seen == 20 {
            println!(
                "PROBE {} {} notify: further deliveries are not printed",
                ctx.tag,
                stamp(ctx.t0)
            );
        }
        if seen < 20 {
            println!(
                "PROBE {} {} notify #{} notification_status={} triggered={:#x} state={} win32={} specific={} checkpoint={} waithint={} pid={}",
                ctx.tag,
                stamp(ctx.t0),
                ctx.count.load(Ordering::SeqCst) + 1,
                n.dwNotificationStatus,
                n.dwNotificationTriggered,
                s.dwCurrentState,
                s.dwWin32ExitCode,
                s.dwServiceSpecificExitCode,
                s.dwCheckPoint,
                s.dwWaitHint,
                s.dwProcessId
            );
        }
        if n.dwNotificationStatus != 0 {
            ctx.err.store(n.dwNotificationStatus, Ordering::SeqCst);
        } else {
            ctx.last.store(s.dwCurrentState, Ordering::SeqCst);
        }
        ctx.count.fetch_add(1, Ordering::SeqCst);
        ctx.fired.store(true, Ordering::SeqCst);
    }

    fn spawn_notifier(ctx: &'static NotifyCtx, name: String, run: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let Ok(svc) = manager().open_service(&name, ServiceAccess::QUERY_STATUS) else {
                return;
            };
            let buf: *mut Services::SERVICE_NOTIFY_2W = Box::into_raw(Box::default());
            while run.load(Ordering::SeqCst) {
                let last = ctx.last.load(Ordering::SeqCst);
                // All seven states, minus the one just delivered.
                let mask = if (1..=7).contains(&last) {
                    0x7f & !(1u32 << (last - 1))
                } else {
                    0x7f
                };
                ctx.fired.store(false, Ordering::SeqCst);
                // SAFETY: not registered at this point (the last delivery has
                // been consumed); leaked, so it outlives any registration.
                unsafe {
                    buf.write(Services::SERVICE_NOTIFY_2W {
                        dwVersion: Services::SERVICE_NOTIFY_STATUS_CHANGE,
                        pfnNotifyCallback: Some(on_notify),
                        pContext: (ctx as *const NotifyCtx).cast_mut().cast(),
                        ..Default::default()
                    });
                }
                let rc = unsafe { Services::NotifyServiceStatusChangeW(svc.raw_handle(), mask, buf) };
                if rc != 0 {
                    println!(
                        "PROBE {} {} notify register error {rc} (mask {mask:#x})",
                        ctx.tag,
                        stamp(ctx.t0)
                    );
                    return;
                }
                // The delivery is an APC, run inside this alertable wait; the
                // short wait only lets the thread notice shutdown.
                while !ctx.fired.load(Ordering::SeqCst) {
                    if !run.load(Ordering::SeqCst) {
                        return;
                    }
                    unsafe { SleepEx(100, 1) };
                }
                if ctx.err.load(Ordering::SeqCst) != 0 {
                    return;
                }
            }
        })
    }

    // System-log tailer ===============================================================================================

    fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
        let from = s.find(open)? + open.len();
        let len = s[from..].find(close)?;
        Some(&s[from..from + len])
    }

    /// `2026-10-24T21:23:21.3623361Z` as ms since the epoch.
    fn iso_to_ms(t: &str) -> Option<i64> {
        let (date, time) = t.trim_end_matches('Z').split_once('T')?;
        let mut d = date.split('-').map(|x| x.parse::<i64>());
        let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
        let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
        let mut h = hms.split(':').map(|x| x.parse::<i64>());
        let (hh, mm, ss) = (h.next()?.ok()?, h.next()?.ok()?, h.next()?.ok()?);
        let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
        // Howard Hinnant's days_from_civil.
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms)
    }

    /// Polls the System log back to back and logs each Service Control
    /// Manager event of this service the first time it is visible, with how
    /// long after its `TimeCreated` that was (the log's visibility lag).
    fn spawn_log_tailer(
        tag: String,
        t0: Instant,
        name: String,
        display: String,
        run: Arc<AtomicBool>,
        stopped_logged: Arc<AtomicU32>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let query =
                "*[System[Provider[@Name='Service Control Manager'] and TimeCreated[timediff(@SystemTime) <= 600000]]]";
            let mut seen = std::collections::HashSet::new();
            while run.load(Ordering::SeqCst) {
                let Ok(o) = Command::new("wevtutil")
                    .args(["qe", "System", &format!("/q:{query}"), "/f:xml", "/rd:false"])
                    .output()
                else {
                    println!("PROBE {tag} {} logseen wevtutil spawn error", stamp(t0));
                    return;
                };
                let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                for e in text.split("</Event>").filter(|s| s.contains("<Event")) {
                    if !(e.contains(&display) || e.contains(&name)) {
                        continue;
                    }
                    let record = between(e, "<EventRecordID>", "</EventRecordID>")
                        .unwrap_or("?")
                        .to_string();
                    if !seen.insert(record.clone()) {
                        continue;
                    }
                    let id = e
                        .find("<EventID")
                        .and_then(|i| between(&e[i..], ">", "</EventID>"))
                        .unwrap_or("?");
                    let created = between(e, "SystemTime='", "'").unwrap_or("?");
                    let lag = iso_to_ms(created).map(|c| now_ms - c);
                    if id == "7036" && e.contains(">stopped</Data>") {
                        stopped_logged.fetch_add(1, Ordering::SeqCst);
                    }
                    let flat = e.split_whitespace().collect::<Vec<_>>().join(" ");
                    println!(
                        "PROBE {tag} {} logseen id={id} record={record} created={created} lag_ms={lag:?} EVENT {flat}</Event>",
                        stamp(t0)
                    );
                }
            }
        })
    }

    // Probe ===========================================================================================================

    /// One service under test, with a connect-back listener, a status tracer,
    /// a notification watcher, and cleanup on drop.
    struct Probe {
        tag: String,
        name: String,
        display: String,
        t0: Instant,
        connects: mpsc::Receiver<Connect>,
        seen: Vec<Connect>,
        tracing: Arc<AtomicBool>,
        tracer: Option<thread::JoinHandle<()>>,
        notify: &'static NotifyCtx,
        notifier: Option<thread::JoinHandle<()>>,
        /// Tails the System log; counts this service's 7036 "stopped" events.
        logger: Option<thread::JoinHandle<()>>,
        stopped_logged: Arc<AtomicU32>,
    }

    fn manager() -> ServiceManager {
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS).expect("open SCM")
    }

    impl Probe {
        fn p(&self, msg: impl std::fmt::Display) {
            println!("PROBE {} {} {}", self.tag, stamp(self.t0), msg);
        }

        /// `scripts` as in the module doc; `actions` is the `sc failure`
        /// actions list (goetia writes a single `restart/<ms>`).
        fn new(tag: &str, scripts: &str, actions: &str) -> Probe {
            Probe::with_observers(tag, scripts, actions, true)
        }

        /// `observe == false` starts no status tracer and no notification
        /// watcher, so the probe holds no service handle of its own.
        fn with_observers(tag: &str, scripts: &str, actions: &str, observe: bool) -> Probe {
            let t0 = Instant::now();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let name = format!("goetia-probe-{nanos:x}-{n}");
            // Different from the name, so an event that carries only one of
            // them shows which.
            let display = format!("GoetiaDisplay-{nanos:x}-{n}");

            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = mpsc::channel();
            let ltag = tag.to_string();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut body = String::new();
                    let _ = stream.read_to_string(&mut body);
                    let at = t0.elapsed();
                    println!("PROBE {ltag} {} connect-back {body}", stamp(t0));
                    if tx.send(Connect { at, body }).is_err() {
                        return;
                    }
                }
            });

            let exe = std::env::current_exe().expect("current_exe");
            let info = ServiceInfo {
                name: name.clone().into(),
                display_name: display.clone().into(),
                service_type: ServiceType::OWN_PROCESS,
                start_type: ServiceStartType::OnDemand,
                error_control: ServiceErrorControl::Normal,
                executable_path: exe,
                launch_arguments: vec![
                    HOST.into(),
                    name.clone().into(),
                    port.to_string().into(),
                    scripts.into(),
                ],
                dependencies: vec![],
                account_name: None,
                account_password: None,
            };
            manager()
                .create_service(&info, ServiceAccess::QUERY_STATUS)
                .expect("create service");

            // The same sc.exe calls `apply_failure_actions` makes.
            let (c1, o1) = sc(&["failure", &name, "reset=", "86400", "actions=", actions]);
            let (c2, o2) = sc(&["failureflag", &name, "1"]);
            let (_, q1) = sc(&["qfailure", &name]);
            let (_, q2) = sc(&["qfailureflag", &name]);

            let tracing = Arc::new(AtomicBool::new(true));
            let notify: &'static NotifyCtx = Box::leak(Box::new(NotifyCtx {
                tag: tag.to_string(),
                t0,
                last: AtomicU32::new(0),
                fired: AtomicBool::new(false),
                err: AtomicU32::new(0),
                count: AtomicU32::new(0),
                at_ns: AtomicU64::new(0),
            }));
            let mut probe = Probe {
                tag: tag.to_string(),
                name: name.clone(),
                display: display.clone(),
                t0,
                connects: rx,
                seen: Vec::new(),
                tracing: tracing.clone(),
                tracer: None,
                notify,
                notifier: None,
                logger: None,
                stopped_logged: Arc::new(AtomicU32::new(0)),
            };
            probe.p(format_args!(
                "setup name={name} display={display} port={port} scripts={scripts} actions={actions}"
            ));
            probe.p(format_args!("setup sc failure -> {c1:?} {o1}"));
            probe.p(format_args!("setup sc failureflag -> {c2:?} {o2}"));
            probe.p(format_args!("setup qfailure: {q1}"));
            probe.p(format_args!("setup qfailureflag: {q2}"));

            if !observe {
                return probe;
            }
            probe.notifier = Some(spawn_notifier(notify, name.clone(), tracing.clone()));
            probe.logger = Some(spawn_log_tailer(
                tag.to_string(),
                t0,
                name.clone(),
                display.clone(),
                tracing.clone(),
                probe.stopped_logged.clone(),
            ));

            let ttag = tag.to_string();
            let tname = name;
            let tracer = thread::spawn(move || {
                // Observation only: records every change in what SCM reports.
                let Ok(svc) = manager().open_service(&tname, ServiceAccess::QUERY_STATUS) else {
                    return;
                };
                let mut last = None;
                while tracing.load(Ordering::SeqCst) {
                    match raw_query(&svc) {
                        Ok(s) => {
                            let cur = fmt_raw(&s);
                            if last.as_ref() != Some(&cur) {
                                println!("PROBE {ttag} {} scm {cur}", stamp(t0));
                                last = Some(cur);
                            }
                        }
                        Err(e) => {
                            println!("PROBE {ttag} {} scm query error {e}", stamp(t0));
                            return;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });
            probe.tracer = Some(tracer);
            probe
        }

        /// Stop the tracer and the notifier (and so close their handles).
        fn quiesce(&mut self) {
            self.tracing.store(false, Ordering::SeqCst);
            if let Some(t) = self.tracer.take() {
                let _ = t.join();
            }
            if let Some(t) = self.notifier.take() {
                let _ = t.join();
            }
            if let Some(t) = self.logger.take() {
                let _ = t.join();
            }
        }

        /// Until the tailer has seen `n` "stopped" (7036) events of this
        /// service. 120 s is the failure bound for an event that never comes.
        fn wait_logged_stopped(&self, n: u32) {
            let deadline = Instant::now() + Duration::from_secs(120);
            while self.stopped_logged.load(Ordering::SeqCst) < n {
                if Instant::now() >= deadline {
                    self.p(format_args!(
                        "log tailer never saw {n} stopped events (failure bound 120s)"
                    ));
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn service(&self, access: ServiceAccess) -> Service {
            manager().open_service(&self.name, access).expect("open service")
        }

        fn start(&self) -> Result<(), String> {
            let t = Instant::now();
            let r = self
                .service(ServiceAccess::START)
                .start::<&str>(&[])
                .map_err(|e| format!("{e:?}"));
            self.p(format_args!(
                "StartService returned {r:?} after {}ms",
                t.elapsed().as_millis()
            ));
            r
        }

        /// The n-th (0-based) connect-back, waiting at most `bound`.
        fn connect(&mut self, n: usize, bound: Duration) -> Option<&Connect> {
            let deadline = Instant::now() + bound;
            while self.seen.len() <= n {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.connects.recv_timeout(left) {
                    Ok(c) => self.seen.push(c),
                    Err(_) => return None,
                }
            }
            self.seen.get(n)
        }

        /// The service reaching `state`, waiting at most `bound`.
        fn wait_state(&self, state: ServiceState, bound: Duration) -> bool {
            let svc = self.service(ServiceAccess::QUERY_STATUS);
            let deadline = Instant::now() + bound;
            loop {
                if svc.query_status().is_ok_and(|s| s.current_state == state) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        /// QueryServiceStatusEx, both exit-code fields raw.
        fn status(&self) -> String {
            match raw_query(&self.service(ServiceAccess::QUERY_STATUS)) {
                Ok(s) => fmt_raw(&s),
                Err(e) => format!("query error {e}"),
            }
        }

        fn result(&self, msg: impl std::fmt::Display) {
            self.p(format_args!("RESULT {msg}"));
        }

        /// Every System-log "Service Control Manager" event from this probe's
        /// lifetime, as one XML line each (EventID, Data, Security, time).
        fn dump_scm_events(&self, label: &str) {
            let age_ms = self.t0.elapsed().as_millis() + 5000;
            let query = format!(
                "*[System[Provider[@Name='Service Control Manager'] and TimeCreated[timediff(@SystemTime) <= {age_ms}]]]"
            );
            let out = Command::new("wevtutil")
                .args(["qe", "System", &format!("/q:{query}"), "/f:xml", "/rd:false"])
                .output();
            match out {
                Err(e) => self.p(format_args!("events[{label}] wevtutil spawn error {e}")),
                Ok(o) => {
                    let text = String::from_utf8_lossy(&o.stdout).to_string();
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    let events: Vec<&str> = text.split("</Event>").filter(|s| s.contains("<Event")).collect();
                    self.p(format_args!(
                        "events[{label}] wevtutil status={:?} count={} stderr={err:?}",
                        o.status.code(),
                        events.len()
                    ));
                    for e in events {
                        let flat = e.split_whitespace().collect::<Vec<_>>().join(" ");
                        self.p(format_args!("events[{label}] EVENT {flat}</Event>"));
                    }
                }
            }
        }
    }

    impl Probe {
        /// This service's System-log "Service Control Manager" events from the probe's lifetime,
        /// counted by EventID (7036 split into `7036running` and `7036stopped`).
        fn event_counts(&self) -> std::collections::BTreeMap<String, u32> {
            let age_ms = self.t0.elapsed().as_millis() + 5000;
            let query = format!(
                "*[System[Provider[@Name='Service Control Manager'] and TimeCreated[timediff(@SystemTime) <= {age_ms}]]]"
            );
            let mut counts = std::collections::BTreeMap::new();
            let Ok(o) = Command::new("wevtutil")
                .args(["qe", "System", &format!("/q:{query}"), "/f:xml", "/rd:false"])
                .output()
            else {
                counts.insert("wevtutil-spawn-error".to_string(), 1);
                return counts;
            };
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            for e in text.split("</Event>").filter(|s| s.contains("<Event")) {
                if !(e.contains(&self.display) || e.contains(&self.name)) {
                    continue;
                }
                let id = e
                    .find("<EventID")
                    .and_then(|i| between(&e[i..], ">", "</EventID>"))
                    .unwrap_or("?");
                let key = match id {
                    "7036" if e.contains(">running</Data>") => "7036running".to_string(),
                    "7036" if e.contains(">stopped</Data>") => "7036stopped".to_string(),
                    other => other.to_string(),
                };
                *counts.entry(key).or_default() += 1;
            }
            counts
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            // Clear the actions first so no pending restart outlives the test.
            sc(&["failure", &self.name, "reset=", "0", "actions=", ""]);
            sc(&["failureflag", &self.name, "0"]);
            let pid = manager()
                .open_service(&self.name, ServiceAccess::QUERY_STATUS)
                .ok()
                .and_then(|svc| svc.query_status().ok())
                .and_then(|s| s.process_id);
            sc(&["stop", &self.name]);
            if let Some(pid) = pid.filter(|p| *p != 0) {
                let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
            }
            sc(&["delete", &self.name]);
            self.tracing.store(false, Ordering::SeqCst);
            if let Some(t) = self.tracer.take() {
                let _ = t.join();
            }
            if let Some(t) = self.notifier.take() {
                let _ = t.join();
            }
            if let Some(t) = self.logger.take() {
                let _ = t.join();
            }
            if let Some(dir) = std::env::var_os("ProgramData") {
                let _ = std::fs::remove_file(std::path::PathBuf::from(dir).join(format!("{}.runs", self.name)));
            }
        }
    }

    fn raw_query(svc: &Service) -> Result<Services::SERVICE_STATUS_PROCESS, std::io::Error> {
        let mut s: Services::SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
        let mut needed = 0u32;
        let ok = unsafe {
            Services::QueryServiceStatusEx(
                svc.raw_handle(),
                Services::SC_STATUS_PROCESS_INFO,
                (&raw mut s).cast::<u8>(),
                size_of::<Services::SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(s)
        }
    }

    fn fmt_raw(s: &Services::SERVICE_STATUS_PROCESS) -> String {
        format!(
            "state={} win32={} specific={} checkpoint={} waithint={} pid={} accepts={:#x}",
            s.dwCurrentState,
            s.dwWin32ExitCode,
            s.dwServiceSpecificExitCode,
            s.dwCheckPoint,
            s.dwWaitHint,
            s.dwProcessId,
            s.dwControlsAccepted
        )
    }

    /// The second and later runs: stay up until stopped, then stop cleanly.
    const STAY_UP: &str = "run,connect,wait-stop,stopped:w32:0";

    fn describe(c: Option<&Connect>, bound: Duration) -> String {
        match c {
            Some(c) => format!("restart observed at t={}ms ({})", c.at.as_millis(), c.body),
            None => format!("no restart within {}s", bound.as_secs()),
        }
    }

    // Config snapshots ================================================================================================

    fn services_key(name: &str) -> String {
        format!("SYSTEM\\CurrentControlSet\\Services\\{name}")
    }

    /// Every value under the service's registry key.
    fn registry_dump(name: &str) -> String {
        let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(services_key(name)) {
            Ok(k) => k,
            Err(e) => return format!("open error {e}"),
        };
        let mut parts = Vec::new();
        for v in key.enum_values() {
            match v {
                Ok((n, v)) => {
                    let hex: String = v.bytes.iter().map(|b| format!("{b:02x}")).collect();
                    parts.push(format!("{n}={:?}:{hex}", v.vtype));
                }
                Err(e) => parts.push(format!("enum error {e}")),
            }
        }
        parts.join("; ")
    }

    fn registry_string(name: &str, value: &str) -> String {
        RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(services_key(name))
            .and_then(|k| k.get_value::<String, _>(value))
            .unwrap_or_else(|e| panic!("read {value}: {e}"))
    }

    /// `QueryServiceConfig2W(level)` into a buffer; `None` on failure.
    fn query_config2(svc: &Service, level: u32) -> Result<Vec<u64>, std::io::Error> {
        let mut needed = 0u32;
        unsafe { Services::QueryServiceConfig2W(svc.raw_handle(), level, std::ptr::null_mut(), 0, &mut needed) };
        let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let ok = unsafe {
            Services::QueryServiceConfig2W(
                svc.raw_handle(),
                level,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(buf)
        }
    }

    fn failure_actions_api(p: &Probe) -> String {
        let svc = p.service(ServiceAccess::QUERY_CONFIG);
        let fa = match query_config2(&svc, Services::SERVICE_CONFIG_FAILURE_ACTIONS) {
            Ok(buf) => {
                // SAFETY: the buffer holds a SERVICE_FAILURE_ACTIONSW and the
                // action array it points into.
                let fa = unsafe { &*(buf.as_ptr() as *const Services::SERVICE_FAILURE_ACTIONSW) };
                let acts: Vec<String> = (0..fa.cActions as usize)
                    .map(|i| {
                        let a = unsafe { &*fa.lpsaActions.add(i) };
                        format!("{}/{}", a.Type, a.Delay)
                    })
                    .collect();
                format!("reset={} actions=[{}]", fa.dwResetPeriod, acts.join(","))
            }
            Err(e) => format!("error {e}"),
        };
        let flag = match query_config2(&svc, Services::SERVICE_CONFIG_FAILURE_ACTIONS_FLAG) {
            Ok(buf) => {
                let f = unsafe { &*(buf.as_ptr() as *const Services::SERVICE_FAILURE_ACTIONS_FLAG) };
                format!("flag={}", f.fFailureActionsOnNonCrashFailures)
            }
            Err(e) => format!("flag error {e}"),
        };
        format!("{fa} {flag}")
    }

    /// Every in-process queryable surface, named, for before/after diffing.
    fn snapshot(p: &Probe) -> Vec<(&'static str, String)> {
        vec![
            ("QueryServiceStatusEx", p.status()),
            ("QueryServiceConfig2(FAILURE_ACTIONS,FLAG)", failure_actions_api(p)),
            ("registry-values", registry_dump(&p.name)),
        ]
    }

    fn diff_snapshots(p: &Probe, before: &[(&'static str, String)], after: &[(&'static str, String)]) {
        for ((n, b), (_, a)) in before.iter().zip(after) {
            if b == a {
                p.p(format_args!("snapshot {n}: unchanged: {a}"));
            } else {
                p.p(format_args!("snapshot {n}: CHANGED\n  before: {b}\n  after:  {a}"));
            }
        }
    }

    // P9 ==============================================================================================================

    /// Run 0 reaches RUNNING and fails (1066, 7); run 1 (SCM's restart) fails
    /// with Win32(5) before RUNNING; run 2 stays up, so a connect-back from it
    /// proves another restart was queued after the failed one.
    fn p9() -> String {
        let delay = Duration::from_millis(1000);
        let mut p = Probe::new(
            "P9",
            &format!("run,connect,stopped:spec:7|connect,stopped:w32:5|{STAY_UP}"),
            "restart/1000",
        );
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        p.p(format_args!("run 0 failed: {}", p.status()));
        let bound = absence_bound(delay);
        let r1 = p.connect(1, bound).cloned();
        p.p(format_args!("run 1 (restart): {}", describe(r1.as_ref(), bound)));
        // Run 1 never reports RUNNING, so wait for its STOPPED to be recorded
        // by seeing run 2 (or the bound); the tracer logs every transition.
        let r2 = p.connect(2, bound).cloned();
        let r = format!(
            "restart after the failed restart: {}; notifications fired: {}; status {}",
            describe(r2.as_ref(), bound),
            p.notify.count.load(Ordering::SeqCst),
            p.status()
        );
        p.result(&r);
        p.dump_scm_events("P9");
        r
    }

    // P15 =============================================================================================================

    /// The shim's report for a daemon no Job Object contains: `STOPPED (0, 1003)` with no `RUNNING`,
    /// from SCM's own START_PENDING (a) or after the host's own START_PENDING (b). Is it stored as
    /// reported, and is a restart queued? Run 1 stays up, so a connect-back from it is a restart.
    fn p15(variant: &'static str) -> String {
        let delay = Duration::from_millis(1000);
        let run0 = match variant {
            "a" => "connect,rawstopped:0:1003",
            "b" => "pending,connect,rawstopped:0:1003",
            other => panic!("bad variant {other}"),
        };
        let mut p = Probe::new(
            &format!("P15{variant}"),
            &format!("{run0}|{STAY_UP}"),
            &format!("restart/{}", delay.as_millis()),
        );
        let started = p.start();
        p.connect(0, EXPECTED_BOUND);
        p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        p.p(format_args!("run 0 stopped: {}", p.status()));
        let bound = absence_bound(delay);
        let r1 = p.connect(1, bound).cloned();
        let r = format!(
            "StartService {started:?}; run 1: {}; notifications fired: {}; status {}",
            describe(r1.as_ref(), bound),
            p.notify.count.load(Ordering::SeqCst),
            p.status()
        );
        p.result(&r);
        p.dump_scm_events(&format!("P15{variant}"));
        r
    }

    // P16 =============================================================================================================

    /// A host whose control handler refuses STOP with `ERROR_SERVICE_CANNOT_ACCEPT_CTRL`, as
    /// goetia-shim does once it has decided on a failure: what does `ControlService` return, and
    /// what does SCM do with the service? The host traces every control its handler receives and
    /// what it returned. (a) a second STOP follows the refused one; (b) only the refused STOP, then
    /// the service is watched for the absence bound.
    fn p16(variant: &'static str) -> String {
        let delay = Duration::from_millis(1000);
        let tag = format!("P16{variant}");
        let mut p = Probe::new(
            &tag,
            &format!("run,refuse-one-stop,connect,wait-stop,stopped:w32:0|{STAY_UP}"),
            &format!("restart/{}", delay.as_millis()),
        );
        let trace = super::host::trace_path(&p.name);
        let dump = |p: &Probe, when: &str| {
            let text = std::fs::read_to_string(&trace).unwrap_or_default();
            for line in text.lines() {
                p.p(format_args!("host trace ({when}): {line}"));
            }
        };
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let svc = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let refused = control_stop(&svc);
        p.p(format_args!("first STOP: {refused}"));
        dump(&p, "right after the first STOP returned");
        let after_refusal = p.status();
        p.p(format_args!("status after the first STOP: {after_refusal}"));
        let bound = absence_bound(delay);
        let second = if variant == "a" {
            let accepted = control_stop(&svc);
            p.p(format_args!("second STOP: {accepted}"));
            dump(&p, "right after the second STOP returned");
            accepted
        } else {
            "not sent".to_string()
        };
        let r1 = p.connect(1, bound).cloned();
        dump(&p, "end");
        let r = format!(
            "first STOP {refused}; then status {after_refusal}; second STOP {second}; final status {}; after: {}",
            p.status(),
            describe(r1.as_ref(), bound)
        );
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    // P10 =============================================================================================================

    #[derive(Clone, Copy)]
    enum Break {
        ImagePath,
        /// `ObjectName` written straight to the registry (`sc config obj=`
        /// rejects a nonexistent account with 1057).
        AccountRegistry,
        /// A real local user, configured as the account, then deleted.
        AccountDeleted,
    }

    /// Stopped after a failure with a restart pending; during the delay SCM is
    /// made unable to start the service itself. What changes?
    fn p10(label: &'static str, how: Break) -> String {
        let tag = format!("P10[{label}]");
        let delay = match how {
            Break::AccountDeleted => Duration::from_millis(6000),
            _ => Duration::from_millis(3000),
        };
        let user = format!("gprobe{}", COUNTER.load(Ordering::SeqCst));
        if matches!(how, Break::AccountDeleted) {
            let out = Command::new("net")
                .args(["user", &user, "Pw-Probe-12!x", "/add"])
                .output()
                .expect("net user /add");
            println!(
                "PROBE {tag} net user {user} /add -> {:?} {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        let mut p = Probe::new(
            &tag,
            &format!("run,connect,stopped:spec:7|{STAY_UP}"),
            &format!("restart/{}", delay.as_millis()),
        );
        let orig_image = registry_string(&p.name, "ImagePath");
        let orig_obj = registry_string(&p.name, "ObjectName");
        p.p(format_args!(
            "original ImagePath={orig_image:?} ObjectName={orig_obj:?}"
        ));

        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        let f1 = p.t0.elapsed();
        p.p(format_args!("failure recorded: {}", p.status()));

        // Mutate inside the restart delay.
        let (c, o) = match how {
            Break::ImagePath => sc(&[
                "config",
                &p.name,
                "binPath=",
                "\"C:\\goetia-nonexistent-dir\\nope.exe\" --probe-host x",
            ]),
            Break::AccountRegistry => {
                let r = RegKey::predef(HKEY_LOCAL_MACHINE)
                    .open_subkey_with_flags(services_key(&p.name), winreg::enums::KEY_SET_VALUE)
                    .and_then(|k| k.set_value("ObjectName", &".\\goetia-nouser-xyz"));
                (Some(0), format!("registry ObjectName write: {r:?}"))
            }
            Break::AccountDeleted => {
                let (c, o) = sc(&[
                    "config",
                    &p.name,
                    "obj=",
                    &format!(".\\{user}"),
                    "password=",
                    "Pw-Probe-12!x",
                ]);
                let del = Command::new("net").args(["user", &user, "/delete"]).output();
                let del = del.map(|d| format!("{:?} {}", d.status.code(), String::from_utf8_lossy(&d.stdout).trim()));
                (c, format!("{o}; net user /delete -> {del:?}"))
            }
        };
        let m = p.t0.elapsed();
        p.p(format_args!(
            "mutation done {}ms after failure: sc config -> {c:?} {o}; notifications so far: {}",
            (m - f1).as_millis(),
            p.notify.count.load(Ordering::SeqCst)
        ));
        let before = snapshot(&p);
        p.p(format_args!(
            "snapshot taken {}ms after failure",
            (p.t0.elapsed() - f1).as_millis()
        ));
        let notified_before = p.notify.count.load(Ordering::SeqCst);

        // Watch the window: a connect-back would mean the service started.
        let window = Duration::from_secs(30);
        let started = p.connect(1, window).cloned();
        p.p(format_args!(
            "window over ({}s after mutation): {}",
            window.as_secs(),
            describe(started.as_ref(), window)
        ));
        let notified_after = p.notify.count.load(Ordering::SeqCst);
        let after = snapshot(&p);
        diff_snapshots(&p, &before, &after);
        let (_, qc) = sc(&["qc", &p.name]);
        let (_, qf) = sc(&["qfailure", &p.name]);
        let (_, qx) = sc(&["queryex", &p.name]);
        p.p(format_args!("sc qc: {qc}"));
        p.p(format_args!("sc qfailure: {qf}"));
        p.p(format_args!("sc queryex: {qx}"));
        p.dump_scm_events("before-restore");

        // Restore, then see whether SCM's repeating action starts it by itself.
        let (c, o) = sc(&["config", &p.name, "binPath=", &orig_image, "obj=", &orig_obj]);
        p.p(format_args!("restore: sc config -> {c:?} {o}"));
        p.p(format_args!(
            "restored ImagePath matches: {}; ObjectName matches: {}",
            registry_string(&p.name, "ImagePath") == orig_image,
            registry_string(&p.name, "ObjectName") == orig_obj
        ));
        let bound = absence_bound(delay);
        let after_restore = p.connect(1, bound).cloned();
        let manual = if after_restore.is_some() {
            String::new()
        } else {
            let s = p.start();
            let c = p.connect(1, EXPECTED_BOUND).cloned();
            format!(
                " (manual start after restore: {s:?}, {})",
                describe(c.as_ref(), EXPECTED_BOUND)
            )
        };
        let r = format!(
            "notifications fired between mutation and window end: {} (total {}); status at window end {}; after restore: {}{manual}",
            notified_after - notified_before,
            notified_after,
            after[0].1,
            describe(after_restore.as_ref(), bound)
        );
        p.result(&r);
        p.dump_scm_events("final");
        r
    }

    // P11 =============================================================================================================

    /// Does SCM keep reporting a pid after the process object is freed?
    /// Per run: start a host that reports RUNNING, terminate it from a handle
    /// that is closed at once, then (no sleeps) alternate QueryServiceStatusEx
    /// and OpenProcess(pid) until SCM reports STOPPED.
    fn p11() -> String {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
        };
        const SERVICES: usize = 25;
        const REPS: usize = 10;
        const KILL_CODE: u32 = 0xC0DE_0011;

        let t0 = Instant::now();
        let tag = "P11";
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let exe = std::env::current_exe().expect("current_exe");
        let mut iterations = 0u64;
        let mut freed_while_reported = 0u64;
        let mut freed_same_pid = 0u64;
        let mut runs = 0u64;
        let mut sequences: std::collections::BTreeMap<String, u64> = Default::default();

        for s in 0..SERVICES {
            let name = format!("goetia-probe11-{nanos:x}-{s}");
            let info = ServiceInfo {
                name: name.clone().into(),
                display_name: name.clone().into(),
                service_type: ServiceType::OWN_PROCESS,
                start_type: ServiceStartType::OnDemand,
                error_control: ServiceErrorControl::Normal,
                executable_path: exe.clone(),
                launch_arguments: vec![
                    HOST.into(),
                    name.clone().into(),
                    "0".into(),
                    "run,wait-stop,stopped:w32:0".into(),
                ],
                dependencies: vec![],
                account_name: None,
                account_password: None,
            };
            manager()
                .create_service(&info, ServiceAccess::QUERY_STATUS)
                .expect("create service");
            let svc = manager()
                .open_service(&name, ServiceAccess::QUERY_STATUS | ServiceAccess::START)
                .expect("open service");

            for rep in 0..REPS {
                svc.start::<&str>(&[]).expect("StartService");
                // Wait for the host to be RUNNING (the real condition).
                let pid = loop {
                    let st = raw_query(&svc).expect("query");
                    if st.dwCurrentState == Services::SERVICE_RUNNING && st.dwProcessId != 0 {
                        break st.dwProcessId;
                    }
                    assert!(
                        st.dwCurrentState != Services::SERVICE_STOPPED,
                        "host stopped before RUNNING: {}",
                        fmt_raw(&st)
                    );
                };
                unsafe {
                    let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    assert!(!h.is_null(), "OpenProcess(terminate) failed: {}", GetLastError());
                    assert!(TerminateProcess(h, KILL_CODE) != 0, "TerminateProcess failed");
                    CloseHandle(h);
                }

                // (state, pid relation, OpenProcess outcome), run-length encoded.
                let mut rle: Vec<(String, u64)> = Vec::new();
                loop {
                    let st = raw_query(&svc).expect("query");
                    let rel = if st.dwProcessId == 0 {
                        "pid0"
                    } else if st.dwProcessId == pid {
                        "samepid"
                    } else {
                        "otherpid"
                    };
                    let outcome = if st.dwProcessId == 0 {
                        "-".to_string()
                    } else {
                        unsafe {
                            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, st.dwProcessId);
                            if h.is_null() {
                                let e = GetLastError();
                                if e == 87 {
                                    freed_while_reported += 1;
                                    if st.dwProcessId == pid {
                                        freed_same_pid += 1;
                                    }
                                }
                                format!("open-err({e})")
                            } else {
                                let mut code = 0u32;
                                let ok = GetExitCodeProcess(h, &mut code);
                                CloseHandle(h);
                                if ok == 0 {
                                    "open-ok(exitcode-query-failed)".to_string()
                                } else if code == 259 {
                                    "open-ok(STILL_ACTIVE)".to_string()
                                } else {
                                    format!("open-ok(exit={code:#x})")
                                }
                            }
                        }
                    };
                    iterations += 1;
                    let key = format!("state={}/{rel}/{outcome}", st.dwCurrentState);
                    match rle.last_mut() {
                        Some((k, n)) if *k == key => *n += 1,
                        _ => rle.push((key, 1)),
                    }
                    if st.dwCurrentState == Services::SERVICE_STOPPED {
                        break;
                    }
                }
                runs += 1;
                let line: Vec<String> = rle.iter().map(|(k, n)| format!("{k} x{n}")).collect();
                println!(
                    "PROBE {tag} t={}ms run={runs} service={s} rep={rep} pid={pid} iterations: {}",
                    t0.elapsed().as_millis(),
                    line.join(" -> ")
                );
                let seq: Vec<&str> = rle.iter().map(|(k, _)| k.as_str()).collect();
                *sequences.entry(seq.join(" -> ")).or_default() += 1;
            }
            sc(&["delete", &name]);
            if let Some(dir) = std::env::var_os("ProgramData") {
                let _ = std::fs::remove_file(std::path::PathBuf::from(dir).join(format!("{name}.runs")));
            }
        }
        for (seq, n) in &sequences {
            println!("PROBE {tag} sequence seen in {n} runs: {seq}");
        }
        let r = format!(
            "{runs} runs over {SERVICES} services, {iterations} iterations; SCM reported a pid while OpenProcess failed with ERROR_INVALID_PARAMETER in {freed_while_reported} iterations ({freed_same_pid} of them for the terminated pid); {} distinct state sequences",
            sequences.len()
        );
        println!("PROBE {tag} RESULT {r}");
        r
    }

    // P12 =============================================================================================================

    /// `OpenService` outcome (with the raw status when it opens), for a
    /// service that may be gone or marked for deletion.
    fn open_state(name: &str) -> String {
        match manager().open_service(name, ServiceAccess::QUERY_STATUS) {
            Ok(svc) => format!(
                "OpenService ok; {}",
                raw_query(&svc)
                    .map(|s| fmt_raw(&s))
                    .unwrap_or_else(|e| format!("query error {e}"))
            ),
            Err(windows_service::Error::Winapi(e)) => format!("OpenService error {:?} ({e})", e.raw_os_error()),
            Err(e) => format!("OpenService error {e:?}"),
        }
    }

    fn record_gone(name: &str) -> bool {
        const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
        matches!(
            manager().open_service(name, ServiceAccess::QUERY_STATUS),
            Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST)
        )
    }

    /// Through a handle the caller already holds (OpenService may be refused
    /// once the record is marked for deletion).
    fn wait_stopped_via(svc: &Service, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            if raw_query(svc).is_ok_and(|s| s.dwCurrentState == Services::SERVICE_STOPPED) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Stopped after a failure with a 5 s restart pending; `DeleteService`
    /// inside the delay. (a) no other handle open; (b) a handle kept open past
    /// the delay, restarted host fails before RUNNING (Win32(5)) once then
    /// stops cleanly; (c) as (b) but the restarted host stays up.
    fn p12(variant: char) -> String {
        let delay = Duration::from_millis(5000);
        let bound = absence_bound(delay);
        let tag = format!("P12{variant}");
        let scripts = match variant {
            'a' | 'c' => format!("run,connect,stopped:spec:7|{STAY_UP}"),
            _ => "run,connect,stopped:spec:7|connect,stopped:w32:5|connect,stopped:w32:0".to_string(),
        };
        let mut p = Probe::with_observers(&tag, &scripts, "restart/5000", variant != 'a');
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        let f1 = p.t0.elapsed();
        p.p(format_args!("failure recorded: {}", p.status()));

        let keep = (variant != 'a').then(|| p.service(ServiceAccess::QUERY_STATUS));
        let del = {
            let svc = p.service(ServiceAccess::DELETE);
            svc.delete().map_err(|e| format!("{e:?}"))
        };
        p.p(format_args!(
            "DeleteService -> {del:?} {}ms after failure (delay {}ms); delete handle closed",
            (p.t0.elapsed() - f1).as_millis(),
            delay.as_millis()
        ));
        p.p(format_args!(
            "after DeleteService, before the delay: {}",
            open_state(&p.name)
        ));
        if let Some(k) = &keep {
            p.p(format_args!(
                "QueryServiceStatusEx via kept handle, before the delay: {}",
                raw_query(k)
                    .map(|s| fmt_raw(&s))
                    .unwrap_or_else(|e| format!("query error {e}"))
            ));
        }

        let c1 = p.connect(1, bound).cloned();
        p.p(format_args!(
            "connect-back after delete ({}ms after failure window start): {}",
            (p.t0.elapsed() - f1).as_millis(),
            describe(c1.as_ref(), bound)
        ));
        let mut extra = String::new();
        if let Some(k) = &keep {
            p.p(format_args!(
                "QueryServiceStatusEx via kept handle, after the delay: {}",
                raw_query(k)
                    .map(|s| fmt_raw(&s))
                    .unwrap_or_else(|e| format!("query error {e}"))
            ));
            if variant == 'b' && c1.is_some() {
                let c2 = p.connect(2, bound).cloned();
                p.p(format_args!("second restart: {}", describe(c2.as_ref(), bound)));
                let stopped = wait_stopped_via(k, Duration::from_secs(30));
                p.p(format_args!(
                    "stopped again: {stopped}; {}",
                    raw_query(k)
                        .map(|s| fmt_raw(&s))
                        .unwrap_or_else(|e| format!("query error {e}"))
                ));
            }
            p.quiesce();
            drop(keep);
            let s = open_state(&p.name);
            p.p(format_args!("after every handle closed: {s}"));
            if variant == 'c' && c1.is_some() {
                let (c, o) = sc(&["stop", &p.name]);
                p.p(format_args!("sc stop -> {c:?} {o}"));
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while !record_gone(&p.name) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            let s2 = open_state(&p.name);
            p.p(format_args!("record check (up to 30s after close/stop): {s2}"));
            extra = format!("; after close: {s}; later: {s2}");
        } else {
            p.p(format_args!("after the bound: {}", open_state(&p.name)));
        }
        let r = format!("delete {del:?}; {}{extra}", describe(c1.as_ref(), bound));
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    // P13 =============================================================================================================

    /// `ControlService(STOP)` through a raw handle: the Win32 error (or Ok)
    /// and the `SERVICE_STATUS` it filled in.
    fn control_stop(svc: &Service) -> String {
        let mut st: Services::SERVICE_STATUS = unsafe { std::mem::zeroed() };
        let ok = unsafe { Services::ControlService(svc.raw_handle(), Services::SERVICE_CONTROL_STOP, &mut st) };
        let res = if ok != 0 {
            "Ok".to_string()
        } else {
            let e = std::io::Error::last_os_error();
            format!("Err {:?} ({e})", e.raw_os_error())
        };
        format!(
            "{res}; SERVICE_STATUS state={} win32={} specific={} checkpoint={} waithint={} accepts={:#x}",
            st.dwCurrentState,
            st.dwWin32ExitCode,
            st.dwServiceSpecificExitCode,
            st.dwCheckPoint,
            st.dwWaitHint,
            st.dwControlsAccepted
        )
    }

    fn query_via(svc: &Service) -> String {
        raw_query(svc)
            .map(|s| fmt_raw(&s))
            .unwrap_or_else(|e| format!("query error {e}"))
    }

    fn wait_state_via(svc: &Service, state: u32, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            if raw_query(svc).is_ok_and(|s| s.dwCurrentState == state) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Register a one-shot `NotifyServiceStatusChangeW` for `mask` on `svc`;
    /// returns the return code and the (leaked) context.
    fn register_notify(p: &Probe, svc: &Service, mask: u32) -> (u32, &'static NotifyCtx) {
        let ctx: &'static NotifyCtx = Box::leak(Box::new(NotifyCtx {
            tag: p.tag.clone(),
            t0: p.t0,
            last: AtomicU32::new(0),
            fired: AtomicBool::new(false),
            err: AtomicU32::new(0),
            count: AtomicU32::new(0),
            at_ns: AtomicU64::new(0),
        }));
        let buf = Box::into_raw(Box::new(Services::SERVICE_NOTIFY_2W {
            dwVersion: Services::SERVICE_NOTIFY_STATUS_CHANGE,
            pfnNotifyCallback: Some(on_notify),
            pContext: (ctx as *const NotifyCtx).cast_mut().cast(),
            ..Default::default()
        }));
        let rc = unsafe { Services::NotifyServiceStatusChangeW(svc.raw_handle(), mask, buf) };
        (rc, ctx)
    }

    /// The APC delivery, run in this thread's alertable wait; at most `bound`.
    fn await_notify(ctx: &NotifyCtx, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while !ctx.fired.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            unsafe { SleepEx(100, 1) };
        }
        true
    }

    /// A RUNNING host that accepts STOP; `DeleteService`, then (on the same
    /// handle) a STOPPED notification registration and `ControlService(STOP)`.
    /// (b) sends the stop from a second handle opened before the delete.
    fn p13_delete_then_stop(second_handle: bool) -> String {
        let tag = if second_handle { "P13b" } else { "P13a" };
        let bound = Duration::from_secs(30);
        // No tracer or notifier: the probe holds no handle beyond its own.
        let mut p = Probe::with_observers(tag, STAY_UP, "", false);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE);
        let h2 = second_handle.then(|| p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS));
        let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        p.p(format_args!("running: {running}; {}", query_via(&h)));

        let del = h.delete().map_err(|e| format!("{e:?}"));
        p.p(format_args!(
            "DeleteService -> {del:?}; status via h: {}",
            query_via(&h)
        ));
        p.p(format_args!("OpenService after delete: {}", open_state(&p.name)));

        // SERVICE_NOTIFY_STOPPED = 0x1.
        let (rc, ctx) = register_notify(&p, &h, 0x1);
        p.p(format_args!("NotifyServiceStatusChangeW(STOPPED) on h -> {rc}"));

        let stopper = h2.as_ref().unwrap_or(&h);
        let stop = control_stop(stopper);
        p.p(format_args!(
            "ControlService(STOP) via {} -> {stop}",
            if second_handle { "second handle" } else { "h" }
        ));

        let notified = rc == 0 && await_notify(ctx, bound);
        p.p(format_args!(
            "notification delivered: {notified} (registration rc {rc}; no delivery within {}s if false); count {}",
            bound.as_secs(),
            ctx.count.load(Ordering::SeqCst)
        ));
        let stopped = wait_stopped_via(&h, bound);
        let fin = query_via(&h);
        p.p(format_args!(
            "stopped: {stopped} (no change within {}s if false); final status via h: {fin}",
            bound.as_secs()
        ));
        let open_before_close = open_state(&p.name);
        p.p(format_args!("OpenService with h still open: {open_before_close}"));

        drop(h);
        let mut mid = String::new();
        if h2.is_some() {
            mid = format!("; after h closed, h2 open: {}", open_state(&p.name));
            p.p(format_args!(
                "after h closed, second handle still open: {}",
                open_state(&p.name)
            ));
        }
        drop(h2);
        let first = open_state(&p.name);
        p.p(format_args!("immediately after every handle closed: {first}"));
        let deadline = Instant::now() + bound;
        while !record_gone(&p.name) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let later = open_state(&p.name);
        p.p(format_args!(
            "record check (up to {}s after close): {later}",
            bound.as_secs()
        ));
        let r = format!(
            "delete {del:?}; notify register rc {rc}, delivered {notified}; ControlService {stop}; stopped {stopped}, final {fin}; with handle open: {open_before_close}{mid}; after close: {first}; later: {later}"
        );
        p.result(&r);
        p.dump_scm_events(tag);
        r
    }

    /// STOP during START_PENDING (the host holds it 4 s before RUNNING).
    /// `after_host_pending`: send it only once the host's own START_PENDING
    /// (checkpoint 1) is recorded, not in SCM's initial one (checkpoint 0).
    fn p13c(after_host_pending: bool) -> String {
        let tag = if after_host_pending { "P13c2" } else { "P13c" };
        let mut p = Probe::new(tag, &format!("pending,hold:4000,{STAY_UP}"), "");
        let _ = p.start();
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        // The real condition: SCM has left STOPPED (the host reported START_PENDING).
        while raw_query(&h).is_ok_and(|s| s.dwCurrentState == Services::SERVICE_STOPPED) {
            thread::sleep(Duration::from_millis(5));
        }
        while after_host_pending
            && raw_query(&h).is_ok_and(|s| s.dwCurrentState == Services::SERVICE_START_PENDING && s.dwCheckPoint == 0)
        {
            thread::sleep(Duration::from_millis(5));
        }
        let before = query_via(&h);
        let stop = control_stop(&h);
        p.p(format_args!("status before stop: {before}"));
        p.p(format_args!("ControlService(STOP) during START_PENDING -> {stop}"));
        p.p(format_args!("status after stop: {}", query_via(&h)));
        let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        let c = p.connect(0, EXPECTED_BOUND).cloned();
        p.p(format_args!(
            "reached RUNNING: {running} (at t={}ms); connect-back: {}; status {}",
            p.t0.elapsed().as_millis(),
            describe(c.as_ref(), EXPECTED_BOUND),
            query_via(&h)
        ));
        let bound = Duration::from_secs(10);
        let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, bound);
        let fin = query_via(&h);
        p.p(format_args!(
            "stopped by the earlier STOP: {stopped} (no change within {}s if false); status {fin}",
            bound.as_secs()
        ));
        let r = format!("ControlService {stop}; reached RUNNING {running}; later stopped {stopped}; final {fin}");
        p.result(&r);
        p.dump_scm_events(tag);
        r
    }

    // P14 =============================================================================================================

    /// Start, wait for run 0 to connect back and fail (1066, 7); returns the
    /// time the failure was recorded.
    fn p14_fail_first(p: &mut Probe) -> Duration {
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        let f1 = p.t0.elapsed();
        p.p(format_args!(
            "failure recorded at t={}ms: {}",
            f1.as_millis(),
            p.status()
        ));
        f1
    }

    /// Clean stop of the running instance and wait for STOPPED.
    fn p14_stop(p: &Probe, f1: Duration, what: &str) {
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let stop = control_stop(&h);
        let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
        p.p(format_args!(
            "{what}: ControlService(STOP) -> {stop}; stopped {stopped} at {}ms after the failure; {}",
            (p.t0.elapsed() - f1).as_millis(),
            query_via(&h)
        ));
    }

    /// Whether the connect-back `n` arrived within `bound`, logged with its
    /// time relative to the failure.
    fn p14_connect(p: &mut Probe, f1: Duration, n: usize, bound: Duration, what: &str) -> String {
        let c = p.connect(n, bound).cloned();
        let d = describe(c.as_ref(), bound);
        let rel = c
            .as_ref()
            .map(|c| {
                format!(
                    " = {}ms after the failure",
                    c.at.as_millis() as i128 - f1.as_millis() as i128
                )
            })
            .unwrap_or_default();
        p.p(format_args!("{what}: {d}{rel}"));
        format!("{what}: {d}{rel}")
    }

    /// A restart queued by a failure: which System-log events mark the queue,
    /// the firing, and the cases where the status misleads. Delay 6 s.
    fn p14(variant: &'static str) -> String {
        let delay = Duration::from_millis(6000);
        let bound = absence_bound(delay);
        let tag = format!("P14{variant}");
        let actions = if variant.starts_with('d') { "" } else { "restart/6000" };
        let scripts = match variant {
            "f" => format!("run,connect,stopped:spec:7|run,connect,stopped:spec:7|{STAY_UP}"),
            _ => format!("run,connect,stopped:spec:7|{STAY_UP}"),
        };
        let mut p = Probe::new(&tag, &scripts, actions);
        if variant == "d0" {
            let (c, o) = sc(&["failureflag", &p.name, "0"]);
            p.p(format_args!("setup: sc failureflag 0 -> {c:?} {o}"));
        }
        let (_, q) = sc(&["qfailure", &p.name]);
        let (_, qf) = sc(&["qfailureflag", &p.name]);
        p.p(format_args!("before the failure: qfailure: {q}; qfailureflag: {qf}"));
        let r;
        let f1 = p14_fail_first(&mut p);

        match variant {
            // Baseline: the restart fires and the new instance stays up.
            "a" => {
                r = p14_connect(&mut p, f1, 1, bound, "restart");
                p.wait_state(ServiceState::Running, EXPECTED_BOUND);
                p14_stop(&p, f1, "clean stop of the restarted instance");
            }
            // H2: manual start during the delay, clean stop before it ends.
            "b" => {
                let _ = p.start();
                p14_connect(&mut p, f1, 1, EXPECTED_BOUND, "manual start");
                p.wait_state(ServiceState::Running, EXPECTED_BOUND);
                p14_stop(&p, f1, "clean stop inside the delay");
                r = p14_connect(&mut p, f1, 2, bound, "queued restart");
                if p.seen.len() > 2 {
                    p.wait_state(ServiceState::Running, EXPECTED_BOUND);
                    p14_stop(&p, f1, "clean stop of the queued restart's instance");
                }
            }
            // Manual start during the delay; still RUNNING when it ends.
            "c" => {
                let _ = p.start();
                p14_connect(&mut p, f1, 1, EXPECTED_BOUND, "manual start");
                p.wait_state(ServiceState::Running, EXPECTED_BOUND);
                r = p14_connect(&mut p, f1, 2, bound, "second instance");
                p.p(format_args!("after the delay: {}", p.status()));
                p14_stop(&p, f1, "clean stop after the delay");
            }
            // H1 stale: the failure had no actions; add them afterwards.
            "d" | "d0" => {
                let (c1, o1) = sc(&["failure", &p.name, "reset=", "86400", "actions=", "restart/6000"]);
                let (c2, o2) = sc(&["failureflag", &p.name, "1"]);
                let (_, q) = sc(&["qfailure", &p.name]);
                let (_, qf) = sc(&["qfailureflag", &p.name]);
                p.p(format_args!(
                    "actions added {}ms after the failure: sc failure -> {c1:?} {o1}; sc failureflag -> {c2:?} {o2}; qfailure: {q}; qfailureflag: {qf}",
                    (p.t0.elapsed() - f1).as_millis()
                ));
                r = p14_connect(&mut p, f1, 1, bound, "restart after adding actions");
                p.p(format_args!("status at the end: {}", p.status()));
            }
            // The actions are cleared during the delay.
            "e" => {
                let (c, o) = sc(&["failure", &p.name, "reset=", "0", "actions=", ""]);
                let (_, q) = sc(&["qfailure", &p.name]);
                p.p(format_args!(
                    "actions cleared {}ms after the failure: sc failure -> {c:?} {o}; qfailure: {q}",
                    (p.t0.elapsed() - f1).as_millis()
                ));
                r = p14_connect(&mut p, f1, 1, bound, "restart after clearing actions");
                if p.seen.len() > 1 {
                    p.wait_state(ServiceState::Running, EXPECTED_BOUND);
                    p14_stop(&p, f1, "clean stop of the restarted instance");
                }
            }
            // The restarted run fails too; then the third run stays up.
            "f" => {
                let c1 = p14_connect(&mut p, f1, 1, bound, "restart 1 (run 1)");
                p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
                p.p(format_args!(
                    "run 1 failed {}ms after failure 1: {}",
                    (p.t0.elapsed() - f1).as_millis(),
                    p.status()
                ));
                let c2 = p14_connect(&mut p, f1, 2, bound, "restart 2 (run 2)");
                r = format!("{c1}; {c2}");
                p.wait_state(ServiceState::Running, EXPECTED_BOUND);
                p14_stop(&p, f1, "clean stop of run 2");
            }
            other => panic!("bad variant {other}"),
        }
        // Stopped events: failure(s) plus the final clean stop(s) of each variant.
        let stops = match variant {
            "a" | "c" | "e" => 2,
            "b" | "f" => 3,
            _ => 1,
        };
        p.wait_logged_stopped(stops);
        p.quiesce();
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    // P17, P18 ========================================================================================================

    /// The named event the host's `signal-wait` step blocks on. Auto-reset: one `fire` releases one wait.
    struct Signal(windows_sys::Win32::Foundation::HANDLE);

    impl Signal {
        /// Created before the service starts, so the host can open it.
        fn create(service: &str) -> Signal {
            use windows_sys::Win32::System::Threading::CreateEventW;
            let name = super::host::signal_name(service);
            // SAFETY: a NUL-terminated name; default security (SYSTEM and Administrators).
            let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
            assert!(!h.is_null(), "CreateEventW: {}", std::io::Error::last_os_error());
            Signal(h)
        }

        fn fire(&self) {
            use windows_sys::Win32::System::Threading::SetEvent;
            // SAFETY: `self.0` is a live event handle.
            assert!(
                unsafe { SetEvent(self.0) } != 0,
                "SetEvent: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    impl Drop for Signal {
        fn drop(&mut self) {
            // SAFETY: owned handle, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }

    /// Every line the host has traced so far (handler calls, `SetServiceStatus` results).
    fn dump_host_trace(p: &Probe, when: &str) {
        let text = std::fs::read_to_string(super::host::trace_path(&p.name)).unwrap_or_default();
        for line in text.lines() {
            p.p(format_args!("host trace ({when}): {line}"));
        }
    }

    /// A failure that comes after `DeleteService` marked the service (the handle `h` that deleted
    /// stays open). Failure actions restart, `failureflag` 1. Does SCM restart a marked service
    /// after a later failure, and does the attempt fail as in P12b (7032, `%%1058`) without a
    /// START_PENDING? (a) delay 0 ms; (b) delay 2000 ms.
    fn p17(variant: &'static str) -> String {
        let delay = Duration::from_millis(match variant {
            "a" => 0,
            "b" => 2000,
            other => panic!("bad variant {other}"),
        });
        let bound = absence_bound(delay);
        let tag = format!("P17{variant}");
        let mut p = Probe::new(
            &tag,
            &format!("run,connect,signal-wait,stopped:spec:7|{STAY_UP}"),
            &format!("restart/{}", delay.as_millis()),
        );
        let go = Signal::create(&p.name);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::DELETE | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        p.p(format_args!("running: {running}; {}", query_via(&h)));

        let del = h.delete().map_err(|e| format!("{e:?}"));
        let marked = p.t0.elapsed();
        p.p(format_args!(
            "DeleteService -> {del:?}; status via h: {}; OpenService after delete: {}",
            query_via(&h),
            open_state(&p.name)
        ));

        go.fire();
        let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
        p.p(format_args!(
            "host told to report STOPPED (1066, 7) {}ms after the mark; stopped via h: {stopped}; {}",
            (p.t0.elapsed() - marked).as_millis(),
            query_via(&h)
        ));
        let failed_at = p.t0.elapsed();

        let c1 = p.connect(1, bound).cloned();
        p.p(format_args!(
            "{}ms after the failure: {}",
            (p.t0.elapsed() - failed_at).as_millis(),
            describe(c1.as_ref(), bound)
        ));
        let after = query_via(&h);
        p.p(format_args!(
            "status via h after the bound: {after}; notifications fired: {}",
            p.notify.count.load(Ordering::SeqCst)
        ));
        let mut clean = String::new();
        if c1.is_some() {
            let stop = control_stop(&h);
            let s = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            clean = format!("; restarted instance stopped by ControlService: {stop}, stopped {s}");
            p.p(format_args!("clean-up of the restarted instance{clean}"));
        }
        p.wait_logged_stopped(if c1.is_some() { 2 } else { 1 });
        dump_host_trace(&p, "end");
        p.quiesce();
        drop(h);
        let first = open_state(&p.name);
        p.p(format_args!(
            "OpenService right after h and every observer closed: {first}"
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !record_gone(&p.name) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let later = open_state(&p.name);
        p.p(format_args!("record check (up to 30s after close): {later}"));
        let r = format!(
            "delete {del:?}; failure reported after the mark: {}; status via h at the end {after}{clean}; after close: {first}; later: {later}",
            describe(c1.as_ref(), bound)
        );
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    /// STOP_PENDING reported by the host while it tears down. Failure actions restart, delay 0 ms,
    /// `failureflag` 1. (a) no STOP: RUNNING, STOP_PENDING (checkpoint 1, hint 10 s), STOPPED (1066, 7).
    /// (b) the probe sends STOP, the handler answers NO_ERROR, STOP_PENDING, STOPPED (0, 0).
    /// (c) as (b), but the host holds STOP_PENDING until the probe has sent another STOP, a
    /// StartService and a query, and then signals it.
    fn p18(variant: &'static str) -> String {
        let delay = Duration::ZERO;
        let bound = absence_bound(delay);
        let tag = format!("P18{variant}");
        let script = match variant {
            "a" => "run,connect,stoppending,stopped:spec:7",
            "b" => "run,connect,wait-stop,stoppending,stopped:w32:0",
            "c" => "run,connect,wait-stop,stoppending,signal-wait,stopped:w32:0",
            other => panic!("bad variant {other}"),
        };
        let mut p = Probe::new(&tag, &format!("{script}|{STAY_UP}"), "restart/0");
        let go = (variant == "c").then(|| Signal::create(&p.name));
        let started = p.start();
        let started_at = p.t0.elapsed();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let mut steps = format!("StartService {started:?}");

        if variant != "a" {
            let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            p.p(format_args!("running: {running}; {}", query_via(&h)));
            let stop = control_stop(&h);
            p.p(format_args!("ControlService(STOP) -> {stop}"));
            steps = format!("{steps}; STOP {stop}");
        }
        if let Some(go) = &go {
            let pending = wait_state_via(&h, Services::SERVICE_STOP_PENDING, EXPECTED_BOUND);
            p.p(format_args!("STOP_PENDING reached: {pending}; {}", query_via(&h)));
            let stop2 = control_stop(&h);
            p.p(format_args!("second ControlService(STOP) in STOP_PENDING -> {stop2}"));
            p.p(format_args!("status after the second STOP: {}", query_via(&h)));
            let start2 = p.start();
            p.p(format_args!(
                "status after StartService in STOP_PENDING: {}",
                query_via(&h)
            ));
            let q = query_via(&h);
            p.p(format_args!("QueryServiceStatusEx in STOP_PENDING: {q}"));
            dump_host_trace(&p, "before the signal");
            steps = format!("{steps}; in STOP_PENDING: second STOP {stop2}; StartService {start2:?}; query {q}");
            go.fire();
            p.p(format_args!("host signalled to leave STOP_PENDING"));
        }

        // (a) restarts at once, so STOPPED may be gone before a poll sees it: the tracer and the
        // notifier record it instead.
        if variant != "a" {
            let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!("stopped via h: {stopped}; {}", query_via(&h)));
        }
        let c1 = p.connect(1, bound).cloned();
        let after = query_via(&h);
        p.p(format_args!(
            "{}ms after the first connect-back: {}; status via h {after}; notifications fired: {}",
            (p.t0.elapsed() - started_at).as_millis(),
            describe(c1.as_ref(), bound),
            p.notify.count.load(Ordering::SeqCst)
        ));
        let mut stops = 1;
        if c1.is_some() {
            let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            let stop = control_stop(&h);
            let s = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!(
                "clean-up of the restarted instance: running {running}; ControlService {stop}; stopped {s}"
            ));
            stops = 2;
        }
        p.wait_logged_stopped(stops);
        dump_host_trace(&p, "end");
        p.quiesce();
        let r = format!("{steps}; after: {}; final status {after}", describe(c1.as_ref(), bound));
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    // P19-P22 =========================================================================================================

    /// An attended window for "no delivery" and "no change" observations: the result is read by a
    /// human as "none within this window", not as proof.
    const ATTENDED_WINDOW: Duration = Duration::from_secs(5);

    // SERVICE_NOTIFY_* bits.
    const N_STOPPED: u32 = 0x1;
    const N_START_PENDING: u32 = 0x2;
    const N_STOP_PENDING: u32 = 0x4;
    const N_RUNNING: u32 = 0x8;
    const N_PAUSED: u32 = 0x40;

    /// `ChangeServiceConfigW` with `SERVICE_NO_CHANGE` for everything but the start type.
    fn change_start_type(svc: &Service, start_type: u32) -> String {
        // SAFETY: a valid handle with SERVICE_CHANGE_CONFIG; every pointer argument is NULL (no change).
        let ok = unsafe {
            Services::ChangeServiceConfigW(
                svc.raw_handle(),
                Services::SERVICE_NO_CHANGE,
                start_type,
                Services::SERVICE_NO_CHANGE,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if ok != 0 {
            "Ok".to_string()
        } else {
            let e = std::io::Error::last_os_error();
            format!("Err {:?} ({e})", e.raw_os_error())
        }
    }

    /// `register_notify` + `await_notify` for one arm, logged: the return code, whether it was
    /// delivered, and the delivered state (the line of the delivery itself has the rest).
    fn arm_and_await(p: &Probe, h: &Service, what: &str, mask: u32) -> String {
        let (rc, ctx) = register_notify(p, h, mask);
        p.p(format_args!(
            "{what}: NotifyServiceStatusChangeW(mask {mask:#x}) -> {rc}"
        ));
        let delivered = rc == 0 && await_notify(ctx, ATTENDED_WINDOW);
        let r = if delivered {
            format!(
                "{what}: mask {mask:#x} rc {rc}; delivered state {} notification_status {}",
                ctx.last.load(Ordering::SeqCst),
                ctx.err.load(Ordering::SeqCst)
            )
        } else {
            format!(
                "{what}: mask {mask:#x} rc {rc}; no delivery within {}s (attended window)",
                ATTENDED_WINDOW.as_secs()
            )
        };
        p.p(&r);
        r
    }

    /// `NotifyServiceStatusChangeW` on a handle opened before `DeleteService`, which stays open.
    /// No failure actions; the probe holds no other handle. (a) the service is STOPPED when marked:
    /// arm STOPPED|RUNNING. (b) RUNNING when marked: arm RUNNING|STOPPED, let it fire, re-arm for
    /// STOPPED while still RUNNING, then stop it from the probe. (c) RUNNING when marked, stopped,
    /// and only then armed (STOPPED|RUNNING).
    fn p22(variant: &'static str) -> String {
        let tag = format!("P22{variant}");
        let mut p = Probe::with_observers(&tag, STAY_UP, "", false);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::DELETE | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        p.p(format_args!("running: {running}; {}", query_via(&h)));
        let mut parts = Vec::new();

        let stop_and_wait = |p: &Probe, what: &str| {
            let stop = control_stop(&h);
            let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!(
                "{what}: ControlService(STOP) {stop}; stopped {stopped}; {}",
                query_via(&h)
            ));
        };
        if variant == "a" {
            stop_and_wait(&p, "clean stop before the mark");
        }
        let del = h.delete().map_err(|e| format!("{e:?}"));
        p.p(format_args!(
            "DeleteService -> {del:?}; status via h: {}",
            query_via(&h)
        ));
        parts.push(format!("delete {del:?}"));

        match variant {
            "a" => parts.push(arm_and_await(
                &p,
                &h,
                "arm on a STOPPED marked service",
                N_STOPPED | N_RUNNING,
            )),
            "b" => {
                let (rc1, ctx1) = register_notify(&p, &h, N_RUNNING | N_STOPPED);
                p.p(format_args!("first arm (RUNNING|STOPPED) -> {rc1}"));
                let d1 = rc1 == 0 && await_notify(ctx1, ATTENDED_WINDOW);
                parts.push(format!(
                    "first arm mask 0x9 rc {rc1}; delivered {d1} (state {}, notification_status {})",
                    ctx1.last.load(Ordering::SeqCst),
                    ctx1.err.load(Ordering::SeqCst)
                ));
                p.p(format_args!("status before the re-arm: {}", query_via(&h)));
                // Still RUNNING: the re-arm masks out the delivered state, as the stream's notifier does.
                let (rc2, ctx2) = register_notify(&p, &h, N_STOPPED);
                p.p(format_args!("re-arm (STOPPED) while RUNNING -> {rc2}"));
                let stop = control_stop(&h);
                p.p(format_args!("ControlService(STOP) -> {stop}"));
                let d2 = rc2 == 0 && await_notify(ctx2, ATTENDED_WINDOW);
                let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
                p.p(format_args!("stopped {stopped}; {}", query_via(&h)));
                parts.push(format!(
                    "re-arm mask 0x1 rc {rc2}; delivered {d2} (state {}, notification_status {}); first arm deliveries {}; ControlService {stop}; final {}",
                    ctx2.last.load(Ordering::SeqCst),
                    ctx2.err.load(Ordering::SeqCst),
                    ctx1.count.load(Ordering::SeqCst),
                    query_via(&h)
                ));
            }
            "c" => {
                stop_and_wait(&p, "stop after the mark");
                parts.push(arm_and_await(
                    &p,
                    &h,
                    "arm after STOPPED on a service marked while RUNNING",
                    N_STOPPED | N_RUNNING,
                ));
            }
            other => panic!("bad variant {other}"),
        }

        let with_handle = open_state(&p.name);
        drop(h);
        let after = open_state(&p.name);
        p.p(format_args!(
            "OpenService: with h open {with_handle}; right after h closed {after}"
        ));
        let r = parts.join("; ");
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    /// Re-arms `NotifyServiceStatusChangeW(mask)` from this thread after every delivery, for
    /// `window` (the service state does not change). One context and one `SERVICE_NOTIFY_2W`
    /// are reused, so a storm leaks nothing.
    fn count_rearms(p: &Probe, svc: &Service, mask: u32, window: Duration) -> String {
        let ctx: &'static NotifyCtx = Box::leak(Box::new(NotifyCtx {
            tag: p.tag.clone(),
            t0: p.t0,
            last: AtomicU32::new(0),
            fired: AtomicBool::new(false),
            err: AtomicU32::new(0),
            count: AtomicU32::new(0),
            at_ns: AtomicU64::new(0),
        }));
        let buf: *mut Services::SERVICE_NOTIFY_2W = Box::into_raw(Box::default());
        let begun = Instant::now();
        let deadline = begun + window;
        let mut deliveries = 0u64;
        let mut first_us = None;
        let mut last_us = None;
        let ended = 'arming: loop {
            ctx.fired.store(false, Ordering::SeqCst);
            // SAFETY: not registered at this point (the last delivery was consumed); leaked, so it
            // outlives any registration still pending when the window ends.
            unsafe {
                buf.write(Services::SERVICE_NOTIFY_2W {
                    dwVersion: Services::SERVICE_NOTIFY_STATUS_CHANGE,
                    pfnNotifyCallback: Some(on_notify),
                    pContext: (ctx as *const NotifyCtx).cast_mut().cast(),
                    ..Default::default()
                });
            }
            let rc = unsafe { Services::NotifyServiceStatusChangeW(svc.raw_handle(), mask, buf) };
            if rc != 0 {
                break format!("a registration returned {rc}");
            }
            while !ctx.fired.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    break 'arming "window over, one registration pending".to_string();
                }
                unsafe { SleepEx(100, 1) };
            }
            deliveries += 1;
            let at = begun.elapsed().as_micros();
            first_us.get_or_insert(at);
            last_us = Some(at);
            if ctx.err.load(Ordering::SeqCst) != 0 {
                break format!(
                    "a delivery carried notification_status {}",
                    ctx.err.load(Ordering::SeqCst)
                );
            }
        };
        format!(
            "mask {mask:#x}: {deliveries} deliveries in a {}s attended window (first at {first_us:?} us, last at {last_us:?} us after the first arm); last delivered state {}; ended: {ended}",
            window.as_secs(),
            ctx.last.load(Ordering::SeqCst)
        )
    }

    /// Does re-arming on an unchanged state that is in the mask fire again at once? The host holds
    /// the state until the probe fires the event. (a) RUNNING, mask RUNNING|STOPPED|PAUSED.
    /// (b) STOP_PENDING after an accepted STOP, mask STOPPED|STOP_PENDING. (c) the host's own
    /// START_PENDING (checkpoint 1), mask RUNNING|START_PENDING.
    fn p20(variant: &'static str) -> String {
        let tag = format!("P20{variant}");
        let (script, mask) = match variant {
            "a" => (
                "run,connect,signal-wait,stopped:w32:0",
                N_RUNNING | N_STOPPED | N_PAUSED,
            ),
            "b" => (
                "run,connect,wait-stop,stoppending,signal-wait,stopped:w32:0",
                N_STOPPED | N_STOP_PENDING,
            ),
            "c" => (
                "pending,connect,signal-wait,run,wait-stop,stopped:w32:0",
                N_RUNNING | N_START_PENDING,
            ),
            other => panic!("bad variant {other}"),
        };
        let mut p = Probe::with_observers(&tag, script, "", false);
        let go = Signal::create(&p.name);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        match variant {
            "a" => {
                wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            }
            "b" => {
                wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
                let stop = control_stop(&h);
                p.p(format_args!("ControlService(STOP) -> {stop}"));
                wait_state_via(&h, Services::SERVICE_STOP_PENDING, EXPECTED_BOUND);
            }
            _ => {
                wait_state_via(&h, Services::SERVICE_START_PENDING, EXPECTED_BOUND);
            }
        }
        p.p(format_args!("state held: {}", query_via(&h)));
        let counted = count_rearms(&p, &h, mask, ATTENDED_WINDOW);
        p.p(&counted);
        p.p(format_args!("state after the window: {}", query_via(&h)));

        go.fire();
        match variant {
            "c" => {
                wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
                let stop = control_stop(&h);
                p.p(format_args!("clean-up: ControlService(STOP) {stop}"));
                wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            }
            _ => {
                wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            }
        }
        p.p(format_args!("final: {}", query_via(&h)));
        p.result(&counted);
        p.dump_scm_events(&tag);
        counted
    }

    /// A service set to `SERVICE_DISABLED` (not deleted), failure actions restart, `failureflag` 1.
    /// (a) disabled while RUNNING, then the host reports `STOPPED (1066, 7)`, delay 0. (b) delay
    /// 2000 ms; the probe disables the service right after the STOPPED notification, inside the
    /// delay. Both end with a `StartService` on the disabled, STOPPED service.
    fn p21(variant: &'static str) -> String {
        let delay = Duration::from_millis(match variant {
            "a" => 0,
            "b" => 2000,
            other => panic!("bad variant {other}"),
        });
        let bound = absence_bound(delay);
        let tag = format!("P21{variant}");
        let mut p = Probe::new(
            &tag,
            &format!("run,connect,signal-wait,stopped:spec:7|{STAY_UP}"),
            &format!("restart/{}", delay.as_millis()),
        );
        let go = Signal::create(&p.name);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG);
        let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        p.p(format_args!("running: {running}; {}", query_via(&h)));

        let disable_note;
        if variant == "a" {
            let d = change_start_type(&h, Services::SERVICE_DISABLED);
            p.p(format_args!(
                "ChangeServiceConfigW(SERVICE_DISABLED) while RUNNING -> {d}"
            ));
            p.p(format_args!("sc qc: {}", sc(&["qc", &p.name]).1));
            disable_note = format!("disabled while RUNNING: {d}");
            go.fire();
        } else {
            let (rc, ctx) = register_notify(&p, &h, N_STOPPED);
            p.p(format_args!("STOPPED registration -> {rc}"));
            go.fire();
            let got = rc == 0 && await_notify(ctx, EXPECTED_BOUND);
            let at = ctx.at_ns.load(Ordering::SeqCst);
            let d = change_start_type(&h, Services::SERVICE_DISABLED);
            let took = (p.t0.elapsed().as_nanos() as u64).saturating_sub(at);
            p.p(format_args!(
                "STOPPED notification delivered {got}; ChangeServiceConfigW(SERVICE_DISABLED) -> {d} {}us after the delivery",
                took / 1000
            ));
            p.p(format_args!("sc qc: {}", sc(&["qc", &p.name]).1));
            disable_note = format!("disabled {}us after the STOPPED notification: {d}", took / 1000);
        }

        let c1 = p.connect(1, bound).cloned();
        p.p(format_args!("{}", describe(c1.as_ref(), bound)));
        let status = query_via(&h);
        p.p(format_args!(
            "status via h after the bound: {status}; notifications fired: {}",
            p.notify.count.load(Ordering::SeqCst)
        ));
        let start = p.start();
        p.p(format_args!(
            "P21c: StartService on the disabled service afterwards -> {start:?}"
        ));
        let mut stops = 1;
        if raw_query(&h).is_ok_and(|s| s.dwCurrentState == Services::SERVICE_RUNNING) {
            let stop = control_stop(&h);
            let s = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!("clean-up of a running instance: {stop}; stopped {s}"));
            stops = 2;
        }
        p.wait_logged_stopped(stops);
        dump_host_trace(&p, "end");
        p.quiesce();
        let r = format!(
            "{disable_note}; after: {}; status via h {status}; StartService afterwards {start:?}",
            describe(c1.as_ref(), bound)
        );
        p.result(&r);
        p.dump_scm_events(&tag);
        r
    }

    /// `StartService` on a disabled, STOPPED service that never ran. No failure actions.
    fn p21c() -> String {
        let p = Probe::with_observers("P21c", STAY_UP, "", false);
        let h = p.service(ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_STATUS);
        let d = change_start_type(&h, Services::SERVICE_DISABLED);
        p.p(format_args!(
            "ChangeServiceConfigW(SERVICE_DISABLED) -> {d}; {}",
            query_via(&h)
        ));
        p.p(format_args!("sc qc: {}", sc(&["qc", &p.name]).1));
        let start = p.start();
        let r = format!("disabled {d}; StartService {start:?}; status {}", query_via(&h));
        p.result(&r);
        p.dump_scm_events("P21c");
        r
    }

    /// The disable-or-delete instant against a due restart (delay 0, a host that fails at once
    /// when signalled). Per sample the probe waits for the failure's STOPPED notification, then
    /// disables the service (a) or deletes it (b; the handle stays open), and records what follows.
    fn p19(variant: &'static str) -> String {
        const SAMPLES: u32 = 10;
        const WINDOW: Duration = Duration::from_secs(3);
        let mut took_us = Vec::new();
        let mut restarted = 0u32;
        let mut lines = Vec::new();
        for i in 1..=SAMPLES {
            let tag = format!("P19{variant}[{i}/{SAMPLES}]");
            let mut p = Probe::new(
                &tag,
                &format!("run,connect,signal-wait,stopped:spec:7|{STAY_UP}"),
                "restart/0",
            );
            let go = Signal::create(&p.name);
            let _ = p.start();
            p.connect(0, EXPECTED_BOUND);
            let h = p.service(
                ServiceAccess::DELETE
                    | ServiceAccess::STOP
                    | ServiceAccess::QUERY_STATUS
                    | ServiceAccess::CHANGE_CONFIG,
            );
            wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            let (rc, ctx) = register_notify(&p, &h, N_STOPPED);
            go.fire();
            let delivered = rc == 0 && await_notify(ctx, EXPECTED_BOUND);
            let at = ctx.at_ns.load(Ordering::SeqCst);
            let notified_at_action = p.notify.count.load(Ordering::SeqCst);
            let action = match variant {
                "a" => change_start_type(&h, Services::SERVICE_DISABLED),
                _ => format!("{:?}", h.delete().map_err(|e| format!("{e:?}"))),
            };
            let took = (p.t0.elapsed().as_nanos() as u64).saturating_sub(at) / 1000;
            took_us.push(took);
            p.p(format_args!(
                "STOPPED notification registered rc {rc}, delivered {delivered}; {} -> {action} {took}us after the delivery",
                if variant == "a" { "disable" } else { "DeleteService" }
            ));

            let c1 = p.connect(1, WINDOW).cloned();
            let status = query_via(&h);
            let notified_end = p.notify.count.load(Ordering::SeqCst);
            let events = p.event_counts();
            let log_restarts = events.get("7036running").copied().unwrap_or(0).saturating_sub(1);
            if c1.is_some() || log_restarts > 0 {
                restarted += 1;
            }
            let line = format!(
                "sample {i}/{SAMPLES}: action {action} {took}us after the STOPPED delivery; connect-back {} (within the {}s attended window); 7036 running events after run 0: {log_restarts}; probe notifications at the action {notified_at_action}, at the window end {notified_end}; status via h {status}; SCM events {events:?}",
                c1.is_some(),
                WINDOW.as_secs()
            );
            p.result(&line);
            lines.push(line);
            if raw_query(&h).is_ok_and(|s| s.dwCurrentState == Services::SERVICE_RUNNING) {
                let stop = control_stop(&h);
                let s = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
                p.p(format_args!("clean-up of a running instance: {stop}; stopped {s}"));
            }
            p.quiesce();
            drop(h);
        }
        took_us.sort_unstable();
        format!(
            "{SAMPLES} samples; {restarted} with a restart seen (connect-back or a second 7036 running); action latency after the STOPPED delivery, us: min {} median {} max {}; per sample: {}",
            took_us[0],
            took_us[took_us.len() / 2],
            took_us[took_us.len() - 1],
            lines.join(" || ")
        )
    }

    // P23 =============================================================================================================

    /// An attended window after the action: what happens "within it" is read by a human, not proof.
    const P23_WINDOW: Duration = Duration::from_millis(500);

    /// Epoch microseconds, to line a sample up with an event's `TimeCreated`.
    fn epoch_us() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as i64
    }

    /// `2026-10-24T21:23:21.3623361Z` as microseconds since the epoch.
    fn iso_to_us(t: &str) -> Option<i64> {
        let ms = iso_to_ms(t)?;
        let frac = t.trim_end_matches('Z').split_once('.').map_or("", |x| x.1);
        let digits = format!("{:0<6}", &frac[..frac.len().min(6)]);
        let sub: i64 = digits[3..6].parse().ok()?;
        Some(ms * 1000 + sub)
    }

    #[derive(Clone, Copy)]
    enum P23Kind {
        /// No disable: the time from STOPPED to START_PENDING and to the connect-back.
        Baseline,
        /// Disable this many ms after the anchor (busy-wait on a high-resolution clock).
        DisableAfterMs(u64),
        /// Disable as soon as START_PENDING is visible in the status.
        DisableAtStartPending,
    }

    struct P23Result {
        anchor: String,
        entered: bool,
    }

    /// One failure, one optional disable, one observation window, then back to a RUNNING host
    /// waiting at `signal-wait` with the start type restored.
    fn p23_sample(p: &mut Probe, go: &Signal, kind: P23Kind, label: &str, i: u32) -> P23Result {
        const RUNNING: u32 = Services::SERVICE_RUNNING;
        const STOPPED: u32 = Services::SERVICE_STOPPED;
        const START_PENDING: u32 = Services::SERVICE_START_PENDING;
        while p.connect(p.seen.len(), Duration::ZERO).is_some() {}
        let k = p.seen.len();
        let h = p.service(
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG,
        );
        let (rc, ctx) = register_notify(p, &h, N_STOPPED);
        let fired_at = Instant::now();
        go.fire();

        // The anchor: the STOPPED notification, or the status if that is seen first (or the
        // notification never comes). The loop re-checks the real condition; the bound is only the
        // failure surface for a host that never fails.
        let how = loop {
            unsafe { SleepEx(0, 1) };
            if ctx.fired.load(Ordering::SeqCst) {
                break "notify".to_string();
            }
            match raw_query(&h) {
                Ok(s) if s.dwCurrentState == STOPPED => break "poll-stopped".to_string(),
                Ok(s) if s.dwCurrentState != RUNNING => break format!("late-state-{}", s.dwCurrentState),
                _ => {}
            }
            if fired_at.elapsed() >= EXPECTED_BOUND {
                break "bound".to_string();
            }
        };
        let anchor = Instant::now();
        let anchor_after_fire_us = anchor.duration_since(fired_at).as_micros();

        // (start, end, status-right-after) in microseconds after the anchor.
        let disable = |h: &Service| {
            let a = anchor.elapsed().as_micros();
            let r = change_start_type(h, Services::SERVICE_DISABLED);
            let b = anchor.elapsed().as_micros();
            let st = raw_query(h);
            let c = anchor.elapsed().as_micros();
            (a, b, c, r, st)
        };
        let mut off_ms = "na".to_string();
        let mut trigger = String::new();
        let mut acted = None;
        match kind {
            P23Kind::Baseline => {}
            P23Kind::DisableAfterMs(ms) => {
                off_ms = ms.to_string();
                let off = Duration::from_millis(ms);
                while anchor.elapsed() < off {
                    std::hint::spin_loop();
                }
                acted = Some(disable(&h));
            }
            P23Kind::DisableAtStartPending => {
                let seen = loop {
                    match raw_query(&h) {
                        Ok(s) if s.dwCurrentState == START_PENDING => break Some(fmt_raw(&s)),
                        Ok(s) if s.dwCurrentState == RUNNING => break None,
                        _ => {}
                    }
                    if anchor.elapsed() >= EXPECTED_BOUND {
                        break None;
                    }
                };
                trigger = format!("START_PENDING seen: {seen:?}");
                if seen.is_some() {
                    acted = Some(disable(&h));
                }
            }
        }

        // The window: every change in the status, and the connect-back.
        let after_state = acted.as_ref().and_then(|a| a.4.as_ref().ok()).map(|s| s.dwCurrentState);
        let mut last = acted
            .as_ref()
            .and_then(|a| a.4.as_ref().ok())
            .map(fmt_raw)
            .unwrap_or_default();
        let mut states = Vec::new();
        let mut changes = Vec::new();
        let mut connect_us: Option<i128> = None;
        let anchor_since_t0 = anchor.duration_since(p.t0).as_micros() as i128;
        let window_end = Instant::now() + P23_WINDOW;
        while Instant::now() < window_end {
            if let Ok(s) = raw_query(&h) {
                let cur = fmt_raw(&s);
                if cur != last {
                    changes.push(format!("+{}us {cur}", anchor.elapsed().as_micros()));
                    states.push(s.dwCurrentState);
                    last = cur;
                }
            }
            if connect_us.is_none() {
                connect_us = p
                    .connect(k, Duration::ZERO)
                    .map(|c| c.at.as_micros() as i128 - anchor_since_t0);
            }
        }
        let entered = after_state.is_some_and(|s| s == START_PENDING || s == RUNNING)
            || states.iter().any(|s| *s == START_PENDING || *s == RUNNING)
            || connect_us.is_some();

        // Let an attempt still in flight finish before the start type is restored.
        while raw_query(&h).is_ok_and(|s| s.dwCurrentState == START_PENDING) {
            if anchor.elapsed() >= EXPECTED_BOUND {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let final_status = query_via(&h);
        let restore = if acted.is_some() {
            change_start_type(&h, Services::SERVICE_DEMAND_START)
        } else {
            "n/a".to_string()
        };
        let mut start = "n/a".to_string();
        if raw_query(&h).is_ok_and(|s| s.dwCurrentState == STOPPED) {
            start = format!("{:?}", h.start::<&str>(&[]).map_err(|e| format!("{e:?}")));
        }
        let reconnect = p.connect(k, EXPECTED_BOUND).is_some();
        let running = wait_state_via(&h, RUNNING, EXPECTED_BOUND);

        let (act_text, act_row) = match &acted {
            None => ("no disable".to_string(), "disable_us=na after_state=na".to_string()),
            Some((a, b, c, r, st)) => {
                let st_text = match st {
                    Ok(s) => fmt_raw(s),
                    Err(e) => format!("query error {e}"),
                };
                (
                    format!(
                        "ChangeServiceConfigW(SERVICE_DISABLED) -> {r} from +{a}us to +{b}us after the anchor; QueryServiceStatusEx right after it (+{c}us): {st_text}"
                    ),
                    format!(
                        "disable_us={a}..{b} disable_result={r} after_state={}",
                        st.as_ref().map(|s| s.dwCurrentState).unwrap_or(0)
                    ),
                )
            }
        };
        p.p(format_args!(
            "P23 {label} sample {i}: STOPPED registration rc {rc}; anchor {how} {anchor_after_fire_us}us after the fire; {trigger} {act_text}; status changes in the {}ms window: [{}]; connect-back (us after the anchor): {connect_us:?}; final status {final_status}; start type restore {restore}; StartService {start}; reconnected {reconnect} running {running}",
            P23_WINDOW.as_millis(),
            changes.join("; ")
        ));
        p.p(format_args!(
            "P23ROW label={label} i={i} off_ms={off_ms} anchor={how} {act_row} saw_states={states:?} connect_us={} entered={entered} final_state={}",
            connect_us.map_or("none".to_string(), |c| c.to_string()),
            raw_query(&h).map(|s| s.dwCurrentState).unwrap_or(0)
        ));
        drop(h);
        P23Result { anchor: how, entered }
    }

    struct ScmEvent {
        label: String,
        created_us: i64,
    }

    /// This service's System-log events from the probe's lifetime. `label` is the EventID, with the
    /// state for 7036 and the other `<Data>` values (minus the service's own names) for the rest.
    fn p23_scm_events(p: &Probe) -> Vec<ScmEvent> {
        let age_ms = p.t0.elapsed().as_millis() + 5000;
        let query = format!(
            "*[System[Provider[@Name='Service Control Manager'] and TimeCreated[timediff(@SystemTime) <= {age_ms}]]]"
        );
        let Ok(o) = Command::new("wevtutil")
            .args(["qe", "System", &format!("/q:{query}"), "/f:xml", "/rd:false"])
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        let mut out = Vec::new();
        for e in text.split("</Event>").filter(|s| s.contains("<Event")) {
            if !(e.contains(&p.display) || e.contains(&p.name)) {
                continue;
            }
            let id = e
                .find("<EventID")
                .and_then(|i| between(&e[i..], ">", "</EventID>"))
                .unwrap_or("?");
            let data: Vec<&str> = e
                .split("<Data")
                .skip(1)
                .filter_map(|seg| between(seg, ">", "</Data>"))
                .collect();
            let label = if id == "7036" {
                format!("7036{}", data.get(1).copied().unwrap_or("?"))
            } else {
                let extra: Vec<&str> = data
                    .iter()
                    .copied()
                    .filter(|d| !d.contains(&p.display) && !d.contains(&p.name))
                    .collect();
                format!("{id}[{}]", extra.join("|"))
            };
            let created = between(e, "SystemTime='", "'").unwrap_or("?");
            if let Some(created_us) = iso_to_us(created) {
                out.push(ScmEvent { label, created_us });
            }
        }
        out.sort_by_key(|e| e.created_us);
        out
    }

    /// Prints the events grouped per sample: group n starts at the n-th 7036 "stopped" (the failure
    /// of sample n; its 7024 and 7031 carry the same `TimeCreated`), offsets in ms from it.
    fn p23_events(p: &Probe, label: &str, samples: u32) {
        // 120 s is the failure bound for an event that never comes (the log lags by up to ~1.1 s).
        let deadline = Instant::now() + Duration::from_secs(120);
        let events = loop {
            let evs = p23_scm_events(p);
            let stops = evs.iter().filter(|e| e.label == "7036stopped").count() as u32;
            if stops >= samples {
                break evs;
            }
            if Instant::now() >= deadline {
                p.p(format_args!(
                    "P23EV {label}: log showed {stops} stopped events, wanted {samples} (failure bound 120s)"
                ));
                break evs;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let starts: Vec<i64> = events
            .iter()
            .filter(|e| e.label == "7036stopped")
            .map(|e| e.created_us)
            .collect();
        for g in 0..=starts.len() {
            let base = if g == 0 {
                events.first().map_or(0, |e| e.created_us)
            } else {
                starts[g - 1]
            };
            let mut parts = Vec::new();
            for e in &events {
                if starts.partition_point(|s| *s <= e.created_us) == g {
                    parts.push(format!("{}@{:+.3}ms", e.label, (e.created_us - base) as f64 / 1000.0));
                }
            }
            p.p(format_args!("P23EV {label} group {g}: {}", parts.join(" ")));
        }
    }

    /// P23: a disable that lands while SCM's restart attempt (delay 0) is in flight. One service
    /// per job; `samples` failures on it, each followed by the same restore.
    fn p23(label: String, kind: P23Kind, samples: u32) -> String {
        let tag = format!("P23{label}");
        let mut p = Probe::with_observers(&tag, "run,connect,signal-wait,stopped:spec:7", "restart/0", false);
        let go = Signal::create(&p.name);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        {
            let h = p.service(ServiceAccess::QUERY_STATUS);
            wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        }
        let mut entered = 0u32;
        let mut anchors = std::collections::BTreeMap::<String, u32>::new();
        for i in 1..=samples {
            let r = p23_sample(&mut p, &go, kind, &label, i);
            entered += u32::from(r.entered);
            *anchors.entry(r.anchor).or_default() += 1;
        }
        p23_events(&p, &label, samples);
        let r = format!("{samples} samples; attempt entered START_PENDING/RUNNING in {entered}; anchors {anchors:?}");
        p.result(&r);
        r
    }

    // P22d, P24 =======================================================================================================

    /// Samples per case in round 11.
    const R11_SAMPLES: u32 = 5;

    /// Runs `sample` `R11_SAMPLES` times, each with its own service; a panic costs that sample only.
    fn r11_samples(label: &str, sample: fn(&str) -> String) -> String {
        let mut out = Vec::new();
        for i in 1..=R11_SAMPLES {
            let tag = format!("{label}s{i}");
            let r = match catch_unwind(AssertUnwindSafe(|| sample(&tag))) {
                Ok(r) => r,
                Err(e) => format!("PROBE-ERROR (panic) {:?}", panic_text(&e)),
            };
            out.push(format!("#{i}: {r}"));
        }
        out.join(" || ")
    }

    /// Opens a handle on the (marked) service for a post-mark arm; the open's outcome is logged.
    fn open_for_arm(p: &Probe, what: &str) -> Option<Service> {
        match manager().open_service(&p.name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS) {
            Ok(s) => {
                p.p(format_args!("{what}: OpenService after the mark ok"));
                Some(s)
            }
            Err(windows_service::Error::Winapi(e)) => {
                p.p(format_args!(
                    "{what}: OpenService after the mark error {:?} ({e})",
                    e.raw_os_error()
                ));
                None
            }
            Err(e) => {
                p.p(format_args!("{what}: OpenService after the mark error {e:?}"));
                None
            }
        }
    }

    /// The host's own START_PENDING (checkpoint 1) is recorded, not SCM's initial one.
    fn wait_host_start_pending(h: &Service) {
        while raw_query(h).is_ok_and(|s| {
            s.dwCurrentState == Services::SERVICE_STOPPED
                || (s.dwCurrentState == Services::SERVICE_START_PENDING && s.dwCheckPoint == 0)
        }) {
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// P22d: the service is delete-marked while the host holds a pending state; arm
    /// `RUNNING|PAUSED|STOPPED` (excluding that state) on a handle opened before the mark and on one
    /// opened after it; then release the host and see whether the next state is delivered.
    /// `stop_pending`: the host holds STOP_PENDING; else START_PENDING (checkpoint 1).
    fn p22d(stop_pending: bool, tag: &str) -> String {
        let script = if stop_pending {
            "run,connect,wait-stop,stoppending,signal-wait,stopped:w32:0"
        } else {
            "pending,signal-wait,run,connect,wait-stop,stopped:w32:0"
        };
        let mut p = Probe::with_observers(tag, script, "", false);
        let go = Signal::create(&p.name);
        let _ = p.start();
        // h: opened before the mark and kept; hd: the handle that deletes, also kept.
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let hd = p.service(ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS);
        if stop_pending {
            p.connect(0, EXPECTED_BOUND);
            wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            p.p(format_args!("ControlService(STOP) -> {}", control_stop(&h)));
            let reached = wait_state_via(&h, Services::SERVICE_STOP_PENDING, EXPECTED_BOUND);
            p.p(format_args!("STOP_PENDING reached: {reached}"));
        } else {
            wait_host_start_pending(&h);
        }
        p.p(format_args!("status before the mark: {}", query_via(&h)));
        let del = hd.delete().map_err(|e| format!("{e:?}"));
        p.p(format_args!(
            "DeleteService via hd -> {del:?}; status via h: {}",
            query_via(&h)
        ));
        let mask = N_RUNNING | N_PAUSED | N_STOPPED;
        let (rc_old, ctx_old) = register_notify(&p, &h, mask);
        p.p(format_args!("arm on h (opened before the mark) -> {rc_old}"));
        let h_new = open_for_arm(&p, "post-mark handle");
        let new_arm = h_new.as_ref().map(|s| {
            let (rc, ctx) = register_notify(&p, s, mask);
            p.p(format_args!("arm on the post-mark handle -> {rc}"));
            (rc, ctx)
        });
        p.p(format_args!("status before the release: {}", query_via(&h)));
        go.fire();
        p.p(format_args!("host released"));
        let d_old = rc_old == 0 && await_notify(ctx_old, ATTENDED_WINDOW);
        let d_new = new_arm.map(|(rc, ctx)| rc == 0 && await_notify(ctx, ATTENDED_WINDOW));
        let new_rc = new_arm.map(|(rc, _)| rc);
        p.p(format_args!(
            "old-handle arm rc {rc_old}, delivered {d_old} (state {}, notification_status {}); post-mark arm rc {new_rc:?} delivered {d_new:?}; status via h: {}",
            ctx_old.last.load(Ordering::SeqCst),
            ctx_old.err.load(Ordering::SeqCst),
            query_via(&h)
        ));
        if !stop_pending {
            // Back to STOPPED for the clean-up.
            wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            p.p(format_args!("ControlService(STOP) -> {}", control_stop(&h)));
        }
        let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
        p.p(format_args!("stopped {stopped}; {}", query_via(&h)));
        let r = format!(
            "delete {del:?}; old arm rc {rc_old} delivered {d_old} (state {}); post-mark open {}, arm rc {new_rc:?} delivered {d_new:?}",
            ctx_old.last.load(Ordering::SeqCst),
            h_new.is_some(),
        );
        p.result(&r);
        p.dump_scm_events(tag);
        r
    }

    /// P24: a STOP rejected during START_PENDING, then a re-arm of `RUNNING|PAUSED|STOPPED` (as
    /// `uninstall`'s settle loop does), the host released to RUNNING, a STOP, and a window for a
    /// recovery restart (`restart/0`, `failureflag` 1; the second script would connect back).
    fn p24(tag: &str) -> String {
        let script = "pending,signal-wait,run,connect,wait-stop,stopped:w32:0";
        let mut p = Probe::with_observers(tag, &format!("{script}|{STAY_UP}"), "restart/0", false);
        let go = Signal::create(&p.name);
        let _ = p.start();
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        wait_host_start_pending(&h);
        p.p(format_args!("status before the first STOP: {}", query_via(&h)));
        let stop1 = control_stop(&h);
        p.p(format_args!("ControlService(STOP) during START_PENDING -> {stop1}"));
        let (rc, ctx) = register_notify(&p, &h, N_RUNNING | N_PAUSED | N_STOPPED);
        p.p(format_args!("re-arm (RUNNING|PAUSED|STOPPED) -> {rc}"));
        go.fire();
        p.p(format_args!("host released"));
        let delivered = rc == 0 && await_notify(ctx, EXPECTED_BOUND);
        p.p(format_args!(
            "re-arm delivered {delivered} (state {}, notification_status {}); status {}",
            ctx.last.load(Ordering::SeqCst),
            ctx.err.load(Ordering::SeqCst),
            query_via(&h)
        ));
        let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
        let c0 = p.connect(0, EXPECTED_BOUND).cloned();
        p.p(format_args!(
            "RUNNING reached {running}; run 0 {}; status {}",
            describe(c0.as_ref(), EXPECTED_BOUND),
            query_via(&h)
        ));
        let stop2 = control_stop(&h);
        p.p(format_args!("ControlService(STOP) after RUNNING -> {stop2}"));
        let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
        p.p(format_args!("stopped {stopped}; {}", query_via(&h)));
        // Attended window for a recovery restart (the second script connects back).
        let c1 = p.connect(1, ATTENDED_WINDOW).cloned();
        p.p(format_args!(
            "{}s after STOPPED: {}; status {}",
            ATTENDED_WINDOW.as_secs(),
            describe(c1.as_ref(), ATTENDED_WINDOW),
            query_via(&h)
        ));
        dump_host_trace(&p, "end");
        let r = format!(
            "STOP#1 {stop1}; re-arm rc {rc} delivered {delivered} (state {}); RUNNING {running}; STOP#2 {stop2}; stopped {stopped}; restart: {}",
            ctx.last.load(Ordering::SeqCst),
            describe(c1.as_ref(), ATTENDED_WINDOW)
        );
        p.result(&r);
        p.dump_scm_events(tag);
        r
    }

    // P25 =============================================================================================================

    /// P25: the service process exits before it reports any status, with `restart/0` and
    /// `failureflag` 1. `run0` is the first run's script (the second stays up, so its connect-back
    /// is a recovery restart). A `RUNNING|PAUSED|STOPPED` notification is armed before the start;
    /// on a STOPPED service it delivers at once, so it is re-armed after that delivery.
    fn p25(tag: &str, run0: &str) -> String {
        let mut p = Probe::new(tag, &format!("{run0}|{STAY_UP}"), "restart/0");
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let mask = N_RUNNING | N_PAUSED | N_STOPPED;
        let (rc1, ctx1) = register_notify(&p, &h, mask);
        let at_once = rc1 == 0 && await_notify(ctx1, ATTENDED_WINDOW);
        p.p(format_args!(
            "arm before the start: rc {rc1}, delivered at once {at_once} (state {}, notification_status {})",
            ctx1.last.load(Ordering::SeqCst),
            ctx1.err.load(Ordering::SeqCst)
        ));
        let (rc, ctx) = if at_once {
            register_notify(&p, &h, mask)
        } else {
            (rc1, ctx1)
        };
        p.p(format_args!("armed for the start: rc {rc}"));
        let start = p.start();
        let delivered = rc == 0 && await_notify(ctx, ATTENDED_WINDOW);
        p.p(format_args!(
            "notification delivered {delivered} (state {}, notification_status {}); status {}",
            ctx.last.load(Ordering::SeqCst),
            ctx.err.load(Ordering::SeqCst),
            query_via(&h)
        ));
        let c0 = p.connect(0, ATTENDED_WINDOW).cloned();
        let c1 = p.connect(1, ATTENDED_WINDOW).cloned();
        // A restarted host stays up: a third connect-back would be a second failure.
        let c2 = c1.as_ref().and_then(|_| p.connect(2, ATTENDED_WINDOW).cloned());
        let status = query_via(&h);
        p.p(format_args!(
            "run 0 connect-back {}; restart {}; third run {}; final status {status}",
            c0.as_ref().map_or("none".to_string(), |c| c.body.clone()),
            describe(c1.as_ref(), ATTENDED_WINDOW),
            describe(c2.as_ref(), ATTENDED_WINDOW),
        ));
        dump_host_trace(&p, "end");
        let r = format!(
            "StartService {start:?}; pre-start arm delivered {at_once}; notification delivered {delivered} (state {}); run 0 connected {}; restart: {}; final status {status}",
            ctx.last.load(Ordering::SeqCst),
            c0.is_some(),
            describe(c1.as_ref(), ATTENDED_WINDOW)
        );
        p.result(&r);
        p.dump_scm_events(tag);
        r
    }

    // P26 =============================================================================================================

    const P26_HINT_MS: u32 = 1000;
    const P26_HOLD: Duration = Duration::from_secs(10);
    const P26_SAMPLE: Duration = Duration::from_millis(100);

    fn host_process_state(proc: windows_sys::Win32::Foundation::HANDLE) -> String {
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        if proc.is_null() {
            return "no handle".to_string();
        }
        let mut exit = 0u32;
        // SAFETY: `proc` is a live process handle with PROCESS_QUERY_LIMITED_INFORMATION.
        if unsafe { GetExitCodeProcess(proc, &mut exit) } == 0 {
            format!("GetExitCodeProcess error {}", std::io::Error::last_os_error())
        } else if exit == 259 {
            "alive".to_string()
        } else {
            format!("exited {exit:#x}")
        }
    }

    /// P26: the host holds STOP_PENDING (checkpoint 1, hint `P26_HINT_MS`, accepts nothing, no
    /// checkpoint change) for `P26_HOLD`, far past the hint, released by the named event. Every
    /// 100 ms the status and the host process are recorded; once the hint has expired, one
    /// `ControlService(STOP)` and one `StartService`. `restart/0`, `failureflag` 1.
    /// (a) the probe sends STOP, the handler answers NO_ERROR, then STOPPED (0, 0).
    /// (b) no STOP (the daemon failed): then STOPPED (1066, 7) and the recovery restart.
    fn p26(tag: &str, variant: &str) -> String {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let bound = absence_bound(Duration::ZERO);
        let pending = format!("stoppending:{P26_HINT_MS}");
        let script = match variant {
            "a" => format!("run,connect,wait-stop,{pending},signal-wait,stopped:w32:0"),
            "b" => format!("run,connect,{pending},signal-wait,stopped:spec:7"),
            other => panic!("bad variant {other}"),
        };
        let mut p = Probe::new(tag, &format!("{script}|{STAY_UP}"), "restart/0");
        let go = Signal::create(&p.name);
        let started = p.start();
        let started_at = p.t0.elapsed();
        p.connect(0, EXPECTED_BOUND);
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        let mut steps = format!("StartService {started:?}");
        if variant == "a" {
            let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            p.p(format_args!("running: {running}; {}", query_via(&h)));
            let stop = control_stop(&h);
            p.p(format_args!("ControlService(STOP) -> {stop}"));
            steps = format!("{steps}; STOP {stop}");
        }
        let reached = wait_state_via(&h, Services::SERVICE_STOP_PENDING, EXPECTED_BOUND);
        let hold_start = Instant::now();
        let pid = raw_query(&h).map_or(0, |s| s.dwProcessId);
        // SAFETY: a plain open by pid; closed below.
        let proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        p.p(format_args!(
            "STOP_PENDING reached: {reached}; host pid {pid}, process handle opened: {}; {}",
            !proc.is_null(),
            query_via(&h)
        ));

        let mut probed = false;
        loop {
            let t = hold_start.elapsed();
            p.p(format_args!(
                "hold t+{}ms: {}; host process {}",
                t.as_millis(),
                query_via(&h),
                host_process_state(proc)
            ));
            if !probed && t >= Duration::from_millis(u64::from(2 * P26_HINT_MS)) {
                probed = true;
                let stop2 = control_stop(&h);
                p.p(format_args!(
                    "t+{}ms (hint expired) ControlService(STOP) -> {stop2}",
                    hold_start.elapsed().as_millis()
                ));
                let start2 = p.start();
                let after = query_via(&h);
                p.p(format_args!(
                    "t+{}ms (hint expired) StartService -> {start2:?}; status after: {after}",
                    hold_start.elapsed().as_millis()
                ));
                steps = format!("{steps}; after the hint: STOP {stop2}; StartService {start2:?}");
            }
            if t >= P26_HOLD {
                break;
            }
            thread::sleep(P26_SAMPLE);
        }
        let held = query_via(&h);
        go.fire();
        p.p(format_args!(
            "host signalled to leave STOP_PENDING; status before: {held}"
        ));

        if variant == "a" {
            let stopped = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!("stopped via h: {stopped}; {}", query_via(&h)));
        }
        p.p(format_args!(
            "host process after the release: {}",
            host_process_state(proc)
        ));
        if !proc.is_null() {
            // SAFETY: owned handle, closed once.
            unsafe { CloseHandle(proc) };
        }
        let c1 = p.connect(1, bound).cloned();
        let after = query_via(&h);
        p.p(format_args!(
            "{}ms after the start: {}; status via h {after}; notifications fired: {}",
            (p.t0.elapsed() - started_at).as_millis(),
            describe(c1.as_ref(), bound),
            p.notify.count.load(Ordering::SeqCst)
        ));
        let mut stops = 1;
        if c1.is_some() {
            let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            let stop = control_stop(&h);
            let s = wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!(
                "clean-up of the restarted instance: running {running}; ControlService {stop}; stopped {s}"
            ));
            stops = 2;
        }
        p.wait_logged_stopped(stops);
        dump_host_trace(&p, "end");
        p.quiesce();
        let r = format!(
            "{steps}; after: {}; status after the bound {after}",
            describe(c1.as_ref(), bound)
        );
        p.result(&r);
        p.dump_scm_events(tag);
        r
    }

    // P27 =============================================================================================================

    const P27_MASK: u32 = N_RUNNING | N_PAUSED | N_STOPPED;

    /// Time base of the `t=` stamps: moved to each arm, so a delivery is stamped relative to the
    /// arm before it (negative: before it).
    struct ArmClock {
        origin: Instant,
        base_ns: AtomicU64,
        arm: AtomicU32,
    }

    impl ArmClock {
        fn rebase(&self, arm: u32) {
            self.base_ns
                .store(self.origin.elapsed().as_nanos() as u64, Ordering::SeqCst);
            self.arm.store(arm, Ordering::SeqCst);
        }
    }

    /// Leaked: the SCM may still hold a pointer to it. Deliveries run in the arming thread's
    /// alertable waits.
    struct P27Ctx {
        tag: String,
        label: &'static str,
        clock: &'static ArmClock,
        /// Every delivery's record, in order.
        records: Mutex<Vec<String>>,
    }

    impl P27Ctx {
        fn records(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
            self.records.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    unsafe extern "system" fn on_notify_p27(p: *const c_void) {
        // SAFETY: the SCM hands back the SERVICE_NOTIFY_2W registered in `p27_arm`, whose pContext
        // is a leaked `P27Ctx`.
        let n = unsafe { &*(p as *const Services::SERVICE_NOTIFY_2W) };
        let ctx = unsafe { &*(n.pContext as *const P27Ctx) };
        let us =
            (ctx.clock.origin.elapsed().as_nanos() as i64 - ctx.clock.base_ns.load(Ordering::SeqCst) as i64) / 1000;
        let record = format!(
            "t={us}us since arm#{} triggered={:#x} notification_status={} {}",
            ctx.clock.arm.load(Ordering::SeqCst),
            n.dwNotificationTriggered,
            n.dwNotificationStatus,
            fmt_raw(&n.ServiceStatus)
        );
        let mut records = ctx.records();
        records.push(record.clone());
        println!(
            "PROBE {} wall={} notify[{}] #{} ctx={ctx:p} buf={p:p} {record}",
            ctx.tag,
            iso_now(),
            ctx.label,
            records.len()
        );
    }

    fn p27_ctx(
        tag: &str,
        label: &'static str,
        clock: &'static ArmClock,
    ) -> (&'static P27Ctx, *mut Services::SERVICE_NOTIFY_2W) {
        let ctx: &'static P27Ctx = Box::leak(Box::new(P27Ctx {
            tag: tag.to_string(),
            label,
            clock,
            records: Mutex::new(Vec::new()),
        }));
        (ctx, Box::into_raw(Box::default()))
    }

    /// One `NotifyServiceStatusChangeW` with `P27_MASK` on `svc`. The buffer is rewritten to its
    /// initial contents first (as P20 did), so a same-buffer re-arm is a clean one. The caller has
    /// consumed the buffer's previous delivery.
    fn p27_arm(p: &Probe, svc: &Service, ctx: &'static P27Ctx, buf: *mut Services::SERVICE_NOTIFY_2W, arm: u32) -> u32 {
        ctx.clock.rebase(arm);
        // SAFETY: not registered at this point; leaked, so it outlives a registration still pending.
        unsafe {
            buf.write(Services::SERVICE_NOTIFY_2W {
                dwVersion: Services::SERVICE_NOTIFY_STATUS_CHANGE,
                pfnNotifyCallback: Some(on_notify_p27),
                pContext: (ctx as *const P27Ctx).cast_mut().cast(),
                ..Default::default()
            });
        }
        let t = Instant::now();
        let rc = unsafe { Services::NotifyServiceStatusChangeW(svc.raw_handle(), P27_MASK, buf) };
        p.p(format_args!(
            "arm #{arm} (mask {P27_MASK:#x}, {}) ctx={ctx:p} buf={buf:p} -> {rc} after {}us",
            ctx.label,
            t.elapsed().as_micros()
        ));
        rc
    }

    /// Alertable waits: a delivery is an APC, run in this thread's alertable wait. `bound` is the
    /// failure bound of an expected delivery.
    fn p27_await(ctx: &P27Ctx, n: usize, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while ctx.records().len() < n {
            if Instant::now() >= deadline {
                return false;
            }
            unsafe { SleepEx(100, 1) };
        }
        true
    }

    /// An attended window: the arming thread stays alertable for the whole of it.
    fn p27_window(window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            unsafe { SleepEx(100, 1) };
        }
    }

    /// Until the independent tracer has been delivered RUNNING; at most `bound`.
    fn p27_await_running(tracer: &NotifyCtx, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while tracer.last.load(Ordering::SeqCst) != Services::SERVICE_RUNNING {
            if Instant::now() >= deadline {
                return false;
            }
            unsafe { SleepEx(100, 1) };
        }
        true
    }

    /// P27: does a second arm on a state that is unchanged and in the mask deliver again? Arm #1
    /// (mask `RUNNING|PAUSED|STOPPED`) is delivered at once; arm #2 per `case`:
    /// (a) STOPPED, same context and buffer, 5 s alertable window. (b) as (a), fresh context and
    /// buffer. (c) the host holds RUNNING, fresh context and buffer, 5 s window. (d) as (a), then
    /// `StartServiceW` from the arming thread, then an alertable wait until an independent tracer
    /// (its own handle and thread) sees RUNNING. (e) as (d), but another thread starts the service
    /// while the arming thread is blocked non-alertably. (f) as (a), arm #2 on a fresh handle.
    fn p27(tag: &str, case: char) -> String {
        let script = if case == 'c' {
            "run,connect,signal-wait,stopped:w32:0"
        } else {
            STAY_UP
        };
        let mut p = Probe::with_observers(tag, script, "", false);
        let go = (case == 'c').then(|| Signal::create(&p.name));
        let h = p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS);
        if case == 'c' {
            let _ = p.start();
            p.connect(0, EXPECTED_BOUND);
            let running = wait_state_via(&h, Services::SERVICE_RUNNING, EXPECTED_BOUND);
            p.p(format_args!("host holds RUNNING: {running}; {}", query_via(&h)));
        } else {
            p.p(format_args!("never started: {}", query_via(&h)));
        }
        let clock: &'static ArmClock = Box::leak(Box::new(ArmClock {
            origin: Instant::now(),
            base_ns: AtomicU64::new(0),
            arm: AtomicU32::new(0),
        }));

        let (ctx1, buf1) = p27_ctx(&p.tag, "ctx1", clock);
        let rc1 = p27_arm(&p, &h, ctx1, buf1, 1);
        let d1 = rc1 == 0 && p27_await(ctx1, 1, EXPECTED_BOUND);
        p.p(format_args!("arm #1 rc {rc1} delivered {d1}; status {}", query_via(&h)));

        // The independent tracer of (d) and (e): its own handle, thread and context; it has seen
        // the current state before arm #2.
        let tracer = matches!(case, 'd' | 'e').then(|| {
            let ctx: &'static NotifyCtx = Box::leak(Box::new(NotifyCtx {
                tag: format!("{}-tracer", p.tag),
                t0: p.t0,
                last: AtomicU32::new(0),
                fired: AtomicBool::new(false),
                err: AtomicU32::new(0),
                count: AtomicU32::new(0),
                at_ns: AtomicU64::new(0),
            }));
            let run = Arc::new(AtomicBool::new(true));
            let thread = spawn_notifier(ctx, p.name.clone(), run.clone());
            let deadline = Instant::now() + EXPECTED_BOUND;
            while ctx.count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            p.p(format_args!(
                "tracer deliveries before arm #2: {}",
                ctx.count.load(Ordering::SeqCst)
            ));
            (ctx, run, thread)
        });

        let (ctx2, buf2) = if matches!(case, 'b' | 'c') {
            p27_ctx(&p.tag, "ctx2", clock)
        } else {
            (ctx1, buf1)
        };
        let from = ctx2.records().len();
        let h2 = (case == 'f').then(|| p.service(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS));
        let rc2 = p27_arm(&p, h2.as_ref().unwrap_or(&h), ctx2, buf2, 2);

        let mut steps = Vec::new();
        match case {
            'a' | 'b' | 'c' | 'f' => p27_window(ATTENDED_WINDOW),
            'd' | 'e' => {
                let (tctx, _, _) = tracer.as_ref().expect("tracer");
                if case == 'd' {
                    steps.push(format!("StartService {:?}", p.start()));
                } else {
                    let (tx, rx) = mpsc::channel();
                    let (name, tag, t0) = (p.name.clone(), p.tag.clone(), p.t0);
                    let starter = thread::spawn(move || {
                        let r = manager()
                            .open_service(&name, ServiceAccess::START)
                            .and_then(|s| s.start::<&str>(&[]))
                            .map_err(|e| format!("{e:?}"));
                        println!("PROBE {tag} {} starter thread: StartService returned {r:?}", stamp(t0));
                        let _ = tx.send(r);
                    });
                    // Blocking, not alertable: no delivery runs until the start has returned.
                    let r = rx.recv();
                    let _ = starter.join();
                    p.p(format_args!("starter thread reported {r:?}"));
                    steps.push(format!("StartService from another thread {r:?}"));
                }
                let running = p27_await_running(tctx, EXPECTED_BOUND);
                p.p(format_args!("tracer saw RUNNING: {running}; status {}", query_via(&h)));
                steps.push(format!("RUNNING seen by the tracer {running}"));
                // Arm #2's own delivery may follow the tracer's.
                p27_window(ATTENDED_WINDOW);
            }
            other => panic!("bad case {other}"),
        }
        let records: Vec<String> = ctx2.records()[from..].to_vec();
        let status = query_via(h2.as_ref().unwrap_or(&h));
        p.p(format_args!(
            "arm #2 rc {rc2}: {} deliveries; status {status}",
            records.len()
        ));
        if let Some((_, run, thread)) = tracer {
            run.store(false, Ordering::SeqCst);
            let _ = thread.join();
        }
        if let Some(go) = &go {
            go.fire();
            wait_state_via(&h, Services::SERVICE_STOPPED, EXPECTED_BOUND);
            p.p(format_args!("host released; {}", query_via(&h)));
        }
        let r = format!(
            "case {case}: arm #1 rc {rc1} delivered {d1}; arm #2 rc {rc2}, {} deliveries {records:?}; {}; status {status}",
            records.len(),
            steps.join("; ")
        );
        p.result(&r);
        r
    }

    // Driver ==========================================================================================================

    pub fn run_all() {
        // Sequential: the System-log events of a probe must be its own.
        type Job = Box<dyn FnOnce() -> String + Send>;
        let jobs: Vec<(String, Job)> = vec![
            (
                "P27a-stopped-same-ctx".to_string(),
                Box::new(|| r11_samples("P27a", |t| p27(t, 'a'))),
            ),
            (
                "P27b-stopped-fresh-ctx".to_string(),
                Box::new(|| r11_samples("P27b", |t| p27(t, 'b'))),
            ),
            (
                "P27c-running-fresh-ctx".to_string(),
                Box::new(|| r11_samples("P27c", |t| p27(t, 'c'))),
            ),
            (
                "P27d-stopped-start-same-thread".to_string(),
                Box::new(|| r11_samples("P27d", |t| p27(t, 'd'))),
            ),
            (
                "P27e-stopped-start-other-thread".to_string(),
                Box::new(|| r11_samples("P27e", |t| p27(t, 'e'))),
            ),
            (
                "P27f-stopped-fresh-handle".to_string(),
                Box::new(|| r11_samples("P27f", |t| p27(t, 'f'))),
            ),
        ];
        println!("PROBE SUMMARY begin");
        for (name, job) in jobs {
            let res = thread::spawn(move || catch_unwind(AssertUnwindSafe(job))).join();
            match res {
                Ok(Ok(r)) => println!("PROBE SUMMARY {name}: {r}"),
                Ok(Err(e)) => println!("PROBE SUMMARY {name}: PROBE-ERROR (panic) {:?}", panic_text(&e)),
                Err(_) => println!("PROBE SUMMARY {name}: PROBE-ERROR (thread)"),
            }
        }
        println!("PROBE SUMMARY end");
    }

    fn panic_text(e: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = e.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else {
            "?".to_string()
        }
    }
}
