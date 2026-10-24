//! THROWAWAY (issue #59, phase A): measures how the Windows SCM treats a
//! service's reported exit, so the shim's design rests on measurements.
//! Round 3: P9/P10 (failed restart attempts). Never merges. Every observation is a `PROBE ...` line in the job log.
//!
//! This binary is also the service host: `<exe> --probe-host <name> <port> <scripts>`
//! where `<scripts>` is `|`-separated, one script per run of the service
//! (run index kept in `%ProgramData%\<name>.runs`; the last script repeats).
//! A script is `,`-separated steps: `pending`, `run`, `connect`, `wait-stop`,
//! `stopped:w32:<n>`, `stopped:spec:<n>`, `rawstopped:<w32>:<spec>`, `exit:<n>`.
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
    use std::ffi::OsString;
    use std::io::Write as _;
    use std::net::{Ipv4Addr, TcpStream};
    use std::os::windows::io::AsRawHandle as _;
    use std::sync::mpsc;
    use std::time::Duration;

    use windows_sys::Win32::System::Services;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    pub const HOST: &str = "--probe-host";

    pub fn run_if_requested() -> bool {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) != Some(HOST) {
            return false;
        }
        service_dispatcher::start(&args[2], ffi_service_main).expect("connect to the SCM dispatcher");
        true
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

    fn code(s: &str) -> u32 {
        match s.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).expect("hex code"),
            None => s.parse().expect("decimal code"),
        }
    }

    fn service_main(_scm_arguments: Vec<OsString>) {
        let args: Vec<String> = std::env::args().collect();
        let name = args[2].clone();
        let port: u16 = args[3].parse().expect("port");
        let scripts: Vec<&str> = args[4].split('|').collect();
        let idx = next_run_index(&name);
        let script = scripts[idx.min(scripts.len() - 1)];

        let (stop_tx, stop_rx) = mpsc::channel();
        let handler = move |control| match control {
            ServiceControl::Stop => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let handle = service_control_handler::register(&name, handler).expect("register control handler");

        let status = |state, accepted, exit_code, checkpoint, wait_hint| ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code,
            checkpoint,
            wait_hint,
            process_id: None,
        };

        for step in script.split(',') {
            let parts: Vec<&str> = step.split(':').collect();
            match parts.as_slice() {
                ["pending"] => handle
                    .set_service_status(status(
                        ServiceState::StartPending,
                        ServiceControlAccept::empty(),
                        ServiceExitCode::Win32(0),
                        1,
                        Duration::from_secs(10),
                    ))
                    .expect("report START_PENDING"),
                ["run"] => handle
                    .set_service_status(status(
                        ServiceState::Running,
                        ServiceControlAccept::STOP,
                        ServiceExitCode::Win32(0),
                        0,
                        Duration::default(),
                    ))
                    .expect("report RUNNING"),
                ["connect"] => {
                    if let Ok(mut s) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                        let _ = write!(s, "run={idx} pid={}", std::process::id());
                        let _ = s.shutdown(std::net::Shutdown::Write);
                    }
                }
                ["wait-stop"] => stop_rx.recv().expect("stop control"),
                ["stopped", kind, n] => {
                    let n = code(n);
                    let exit_code = match *kind {
                        "w32" => ServiceExitCode::Win32(n),
                        "spec" => ServiceExitCode::ServiceSpecific(n),
                        other => panic!("bad kind {other}"),
                    };
                    handle
                        .set_service_status(status(
                            ServiceState::Stopped,
                            ServiceControlAccept::empty(),
                            exit_code,
                            0,
                            Duration::default(),
                        ))
                        .expect("report STOPPED");
                }
                // A raw SERVICE_STATUS, for pairs `ServiceExitCode` cannot say
                // (win32 code and service-specific code independent).
                ["rawstopped", w32, spec] => {
                    let raw = Services::SERVICE_STATUS {
                        dwServiceType: Services::SERVICE_WIN32_OWN_PROCESS,
                        dwCurrentState: Services::SERVICE_STOPPED,
                        dwControlsAccepted: 0,
                        dwWin32ExitCode: code(w32),
                        dwServiceSpecificExitCode: code(spec),
                        dwCheckPoint: 0,
                        dwWaitHint: 0,
                    };
                    let ok = unsafe { Services::SetServiceStatus(handle.as_raw_handle(), &raw) };
                    assert!(ok != 0, "SetServiceStatus failed: {}", std::io::Error::last_os_error());
                }
                ["exit", n] => std::process::exit(code(n) as i32),
                other => panic!("bad step {other:?}"),
            }
        }
    }
}

#[cfg(windows)]
mod probes {
    use std::ffi::c_void;
    use std::io::Read as _;
    use std::net::{Ipv4Addr, TcpListener};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, mpsc};
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
    }

    unsafe extern "system" fn on_notify(p: *const c_void) {
        // SAFETY: the SCM hands back the SERVICE_NOTIFY_2W registered below,
        // whose pContext is the leaked `NotifyCtx`.
        let n = unsafe { &*(p as *const Services::SERVICE_NOTIFY_2W) };
        let ctx = unsafe { &*(n.pContext as *const NotifyCtx) };
        let s = &n.ServiceStatus;
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

    // Probe ===========================================================================================================

    /// One service under test, with a connect-back listener, a status tracer,
    /// a notification watcher, and cleanup on drop.
    struct Probe {
        tag: String,
        name: String,
        t0: Instant,
        connects: mpsc::Receiver<Connect>,
        seen: Vec<Connect>,
        tracing: Arc<AtomicBool>,
        tracer: Option<thread::JoinHandle<()>>,
        notify: &'static NotifyCtx,
        notifier: Option<thread::JoinHandle<()>>,
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
            }));
            let mut probe = Probe {
                tag: tag.to_string(),
                name: name.clone(),
                t0,
                connects: rx,
                seen: Vec::new(),
                tracing: tracing.clone(),
                tracer: None,
                notify,
                notifier: None,
            };
            probe.p(format_args!(
                "setup name={name} display={display} port={port} scripts={scripts} actions={actions}"
            ));
            probe.p(format_args!("setup sc failure -> {c1:?} {o1}"));
            probe.p(format_args!("setup sc failureflag -> {c2:?} {o2}"));
            probe.p(format_args!("setup qfailure: {q1}"));
            probe.p(format_args!("setup qfailureflag: {q2}"));

            probe.notifier = Some(spawn_notifier(notify, name.clone(), tracing.clone()));

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
                .args(["user", &user, "Pw-Probe-1234!x", "/add"])
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
                    "Pw-Probe-1234!x",
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

    // Driver ==========================================================================================================

    pub fn run_all() {
        // Sequential: the System-log events of a probe must be its own.
        type Job = Box<dyn FnOnce() -> String + Send>;
        let jobs: Vec<(&'static str, Job)> = vec![
            ("P9", Box::new(p9)),
            ("P10[a-imagepath]", Box::new(|| p10("a-imagepath", Break::ImagePath))),
            (
                "P10[b1-account-registry]",
                Box::new(|| p10("b1-account-registry", Break::AccountRegistry)),
            ),
            (
                "P10[b2-account-deleted]",
                Box::new(|| p10("b2-account-deleted", Break::AccountDeleted)),
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
