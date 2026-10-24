//! THROWAWAY (issue #59, phase A): measures how the Windows SCM treats a
//! service's reported exit, so the shim's design rests on measurements.
//! Never merges. Every observation is a `PROBE ...` line in the job log.
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
    use std::io::Read as _;
    use std::net::{Ipv4Addr, TcpListener};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows_service::service::{
        Service, ServiceAccess, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::System::Services;

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

    #[derive(Clone)]
    struct Connect {
        at: Duration,
        body: String,
    }

    /// One service under test, with a connect-back listener, a status tracer,
    /// and cleanup on drop.
    struct Probe {
        tag: String,
        name: String,
        t0: Instant,
        connects: mpsc::Receiver<Connect>,
        seen: Vec<Connect>,
        tracing: Arc<AtomicBool>,
        tracer: Option<thread::JoinHandle<()>>,
    }

    fn manager() -> ServiceManager {
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS).expect("open SCM")
    }

    impl Probe {
        fn p(&self, msg: impl std::fmt::Display) {
            println!("PROBE {} t={}ms {}", self.tag, self.t0.elapsed().as_millis(), msg);
        }

        /// `scripts` as in the module doc; `actions` is the `sc failure`
        /// actions list (goetia writes a single `restart/<ms>`).
        fn new(tag: &str, scripts: &str, actions: &str) -> Probe {
            let t0 = Instant::now();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let name = format!("goetia-probe-{nanos:x}-{n}");

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
                    println!("PROBE {ltag} t={}ms connect-back {body}", at.as_millis());
                    if tx.send(Connect { at, body }).is_err() {
                        return;
                    }
                }
            });

            let exe = std::env::current_exe().expect("current_exe");
            let info = ServiceInfo {
                name: name.clone().into(),
                display_name: name.clone().into(),
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
            let mut probe = Probe {
                tag: tag.to_string(),
                name: name.clone(),
                t0,
                connects: rx,
                seen: Vec::new(),
                tracing: tracing.clone(),
                tracer: None,
            };
            probe.p(format_args!(
                "setup name={name} port={port} scripts={scripts} actions={actions}"
            ));
            probe.p(format_args!("setup sc failure -> {c1:?} {o1}"));
            probe.p(format_args!("setup sc failureflag -> {c2:?} {o2}"));
            probe.p(format_args!("setup qfailure: {q1}"));
            probe.p(format_args!("setup qfailureflag: {q2}"));

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
                                println!("PROBE {ttag} t={}ms scm {cur}", t0.elapsed().as_millis());
                                last = Some(cur);
                            }
                        }
                        Err(e) => {
                            println!("PROBE {ttag} t={}ms scm query error {e}", t0.elapsed().as_millis());
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

        fn stop(&self) -> Result<(), String> {
            let t = Instant::now();
            let r = self
                .service(ServiceAccess::STOP)
                .stop()
                .map(|s| format!("state={:?} exit={}", s.current_state, fmt_exit(s.exit_code)))
                .map_err(|e| format!("{e:?}"));
            self.p(format_args!(
                "ControlService(STOP) returned {r:?} after {}ms",
                t.elapsed().as_millis()
            ));
            r.map(drop)
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
            "state={} win32={} specific={} pid={} accepts={:#x}",
            s.dwCurrentState, s.dwWin32ExitCode, s.dwServiceSpecificExitCode, s.dwProcessId, s.dwControlsAccepted
        )
    }

    fn fmt_exit(e: ServiceExitCode) -> String {
        match e {
            ServiceExitCode::Win32(n) => format!("Win32({n})"),
            ServiceExitCode::ServiceSpecific(n) => format!("ServiceSpecific({n:#x})"),
        }
    }

    /// The second and later runs: stay up until stopped, then stop cleanly.
    const STAY_UP: &str = "run,connect,wait-stop,stopped:w32:0";

    fn describe(c: Option<&Connect>, bound: Duration) -> String {
        match c {
            Some(c) => format!("restart observed at t={}ms ({})", c.at.as_millis(), c.body),
            None => format!("no restart within {}s", bound.as_secs()),
        }
    }

    // P7 ==============================================================================================================

    /// Commanded stop, host reports STOPPED with win32 = 0 and specific = 6.
    fn p7() -> String {
        let delay = Duration::from_millis(1000);
        let mut p = Probe::new(
            "P7",
            &format!("run,connect,wait-stop,rawstopped:0:6|{STAY_UP}"),
            "restart/1000",
        );
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        p.p(format_args!("before stop: {}", p.status()));
        let stop = p.stop();
        p.p(format_args!("right after ControlService(STOP): {}", p.status()));
        let stopped = p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        let at_stopped = p.status();
        p.p(format_args!("reached STOPPED: {stopped}; {at_stopped}"));
        let bound = absence_bound(delay);
        let second = p.connect(1, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "commanded stop, host reports win32=0 specific=6 (stop() -> {stop:?}); QueryServiceStatusEx at STOPPED: {at_stopped}; {r}; final {}",
            p.status()
        ));
        r
    }

    /// Same pair, but the process stops on its own after RUNNING (no stop command).
    fn p7b() -> String {
        let delay = Duration::from_millis(1000);
        let mut p = Probe::new(
            "P7b",
            &format!("run,connect,rawstopped:0:6,exit:0|{STAY_UP}"),
            "restart/1000",
        );
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let stopped = p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        let at_stopped = p.status();
        p.p(format_args!("reached STOPPED: {stopped}; {at_stopped}"));
        let bound = absence_bound(delay);
        let second = p.connect(1, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "self-exit after RUNNING, host reports win32=0 specific=6; QueryServiceStatusEx at STOPPED: {at_stopped}; {r}; final {}",
            p.status()
        ));
        r
    }

    // P8 ==============================================================================================================

    /// Two actions: if the SCM failure count survives a mutation, the second
    /// failure waits 2 s (action #2); if the count was reset it waits 15 s.
    const P8_ACTIONS: &str = "restart/15000/restart/2000";
    const P8_FAIL: &str = "run,connect,stopped:w32:1";

    /// `null_when_empty`: pass a NULL action array for an empty list (else a
    /// non-NULL pointer with `cActions` = 0).
    fn set_actions_api(name: &str, actions: &[(i32, u32)], reset: u32, null_when_empty: bool) -> String {
        let mut raw: Vec<Services::SC_ACTION> = actions
            .iter()
            .map(|(t, d)| Services::SC_ACTION { Type: *t, Delay: *d })
            .collect();
        let fa = Services::SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: reset,
            lpRebootMsg: std::ptr::null_mut(),
            lpCommand: std::ptr::null_mut(),
            cActions: raw.len() as u32,
            lpsaActions: if raw.is_empty() && null_when_empty {
                std::ptr::null_mut()
            } else {
                raw.as_mut_ptr()
            },
        };
        let svc = manager()
            .open_service(
                name,
                ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG | ServiceAccess::START,
            )
            .expect("open service for config");
        let ok = unsafe {
            Services::ChangeServiceConfig2W(
                svc.raw_handle(),
                Services::SERVICE_CONFIG_FAILURE_ACTIONS,
                (&raw const fa).cast(),
            )
        };
        if ok == 0 {
            format!("ChangeServiceConfig2W failed: {}", std::io::Error::last_os_error())
        } else {
            "ChangeServiceConfig2W ok".to_string()
        }
    }

    fn restart_actions() -> [(i32, u32); 2] {
        [
            (Services::SC_ACTION_RESTART, 15000),
            (Services::SC_ACTION_RESTART, 2000),
        ]
    }

    fn sc_failure(name: &str, reset: &str, actions: &str) -> String {
        let (c, o) = sc(&["failure", name, "reset=", reset, "actions=", actions]);
        format!("sc failure reset={reset} actions={actions:?} -> {c:?} {o}")
    }

    fn p8(label: &'static str, mutate: fn(&Probe) -> String) -> String {
        let tag = format!("P8[{label}]");
        let mut p = Probe::new(&tag, &format!("{P8_FAIL}|{P8_FAIL}|{STAY_UP}"), P8_ACTIONS);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let stopped = p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        let f1 = p.t0.elapsed();
        p.p(format_args!(
            "failure 1 recorded (STOPPED reached: {stopped}); {}",
            p.status()
        ));

        let log = mutate(&p);
        let (_, q) = sc(&["qfailure", &p.name]);
        let (_, qf) = sc(&["qfailureflag", &p.name]);
        let m = p.t0.elapsed();
        p.p(format_args!(
            "mutation done {}ms after failure 1: {log}; qfailure: {q}; qfailureflag: {qf}; {}",
            (m - f1).as_millis(),
            p.status()
        ));

        let bound = absence_bound(Duration::from_secs(15));
        let second = p.connect(1, bound).cloned();
        let fired = match &second {
            Some(c) => format!("pending restart FIRED {}ms after failure 1", (c.at - f1).as_millis()),
            None => format!(
                "pending restart did NOT fire within {}s of the mutation",
                bound.as_secs()
            ),
        };
        p.p(format_args!("{fired}"));
        let mut manual = String::new();
        let second = if second.is_some() {
            second
        } else {
            manual = format!(" (manual start: {:?})", p.start());
            p.connect(1, EXPECTED_BOUND).cloned()
        };
        // Run 1 fails at once: which action does SCM use for failure 2?
        let delay2 = match second {
            None => "run 1 never connected".to_string(),
            Some(_) => {
                p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
                let f2 = p.t0.elapsed();
                p.p(format_args!("failure 2 recorded; {}", p.status()));
                match p.connect(2, bound) {
                    Some(c) => format!(
                        "failure 2 -> restart {}ms later (2000 = count survived, 15000 = count reset)",
                        (c.at - f2).as_millis()
                    ),
                    None => format!("no restart after failure 2 within {}s", bound.as_secs()),
                }
            }
        };
        let r = format!("{fired}{manual}; {delay2}");
        p.result(format_args!("{r}; final {}", p.status()));
        r
    }

    fn mutate_none(_: &Probe) -> String {
        "no mutation (control)".to_string()
    }

    fn mutate_a_sc(p: &Probe) -> String {
        sc_failure(&p.name, "86400", P8_ACTIONS)
    }

    fn mutate_a_api(p: &Probe) -> String {
        set_actions_api(&p.name, &restart_actions(), 86400, true)
    }

    fn mutate_b_sc(p: &Probe) -> String {
        let a = sc_failure(&p.name, "0", "");
        let (_, mid) = sc(&["qfailure", &p.name]);
        let b = sc_failure(&p.name, "86400", P8_ACTIONS);
        format!("{a}; qfailure between: {mid}; restore: {b}")
    }

    fn mutate_b_api(p: &Probe, null_when_empty: bool) -> String {
        let a = set_actions_api(&p.name, &[], 0, null_when_empty);
        let (_, mid) = sc(&["qfailure", &p.name]);
        let b = set_actions_api(&p.name, &restart_actions(), 86400, true);
        format!("empty (null array: {null_when_empty}): {a}; qfailure between: {mid}; restore: {b}")
    }
    fn mutate_b_api_null(p: &Probe) -> String {
        mutate_b_api(p, true)
    }
    fn mutate_b_api_empty(p: &Probe) -> String {
        mutate_b_api(p, false)
    }

    fn mutate_c_sc(p: &Probe) -> String {
        let (c0, o0) = sc(&["failureflag", &p.name, "0"]);
        let (_, mid) = sc(&["qfailureflag", &p.name]);
        let (c1, o1) = sc(&["failureflag", &p.name, "1"]);
        format!("failureflag 0 -> {c0:?} {o0}; between: {mid}; failureflag 1 -> {c1:?} {o1}")
    }

    // Driver ==========================================================================================================

    pub fn run_all() {
        type Job = Box<dyn FnOnce() -> String + Send>;
        let jobs: Vec<(&'static str, Job)> = vec![
            ("P7", Box::new(p7)),
            ("P7b", Box::new(p7b)),
            ("P8[control]", Box::new(|| p8("control", mutate_none))),
            ("P8[a-sc]", Box::new(|| p8("a-sc", mutate_a_sc))),
            ("P8[a-api]", Box::new(|| p8("a-api", mutate_a_api))),
            ("P8[b-sc]", Box::new(|| p8("b-sc", mutate_b_sc))),
            ("P8[b-api-null]", Box::new(|| p8("b-api-null", mutate_b_api_null))),
            ("P8[b-api-empty]", Box::new(|| p8("b-api-empty", mutate_b_api_empty))),
            ("P8[c-sc]", Box::new(|| p8("c-sc", mutate_c_sc))),
        ];
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(name, job)| (name, thread::spawn(move || catch_unwind(AssertUnwindSafe(job)))))
            .collect();
        println!("PROBE SUMMARY begin");
        for (name, h) in handles {
            match h.join() {
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
