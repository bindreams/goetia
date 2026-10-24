//! THROWAWAY (issue #59, phase A): measures how the Windows SCM treats a
//! service's reported exit, so the shim's design rests on measurements.
//! Never merges. Every observation is a `PROBE ...` line in the job log.
//!
//! This binary is also the service host: `<exe> --probe-host <name> <port> <scripts>`
//! where `<scripts>` is `|`-separated, one script per run of the service
//! (run index kept in `%ProgramData%\<name>.runs`; the last script repeats).
//! A script is `,`-separated steps: `pending`, `run`, `connect`, `wait-stop`,
//! `stopped:w32:<n>`, `stopped:spec:<n>`, `exit:<n>`.
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
    use std::sync::mpsc;
    use std::time::Duration;

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

        /// `scripts` as in the module doc; `delay_ms` is the single
        /// `restart/<ms>` action goetia writes.
        fn new(tag: &str, scripts: &str, delay_ms: u64) -> Probe {
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
            let action = format!("restart/{delay_ms}");
            let (c1, o1) = sc(&["failure", &name, "reset=", "86400", "actions=", &action]);
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
                "setup name={name} port={port} scripts={scripts} delay={delay_ms}ms"
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
                    match svc.query_status() {
                        Ok(s) => {
                            let cur = (s.current_state, s.exit_code, s.process_id, s.controls_accepted);
                            if last != Some(cur) {
                                println!(
                                    "PROBE {ttag} t={}ms scm state={:?} exit={} pid={:?} accepts={:?}",
                                    t0.elapsed().as_millis(),
                                    s.current_state,
                                    fmt_exit(s.exit_code),
                                    s.process_id,
                                    s.controls_accepted
                                );
                                last = Some(cur);
                            }
                        }
                        Err(e) => {
                            println!("PROBE {ttag} t={}ms scm query error {e:?}", t0.elapsed().as_millis());
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

        fn status(&self) -> String {
            match self.service(ServiceAccess::QUERY_STATUS).query_status() {
                Ok(s) => format!(
                    "state={:?} exit={} pid={:?}",
                    s.current_state,
                    fmt_exit(s.exit_code),
                    s.process_id
                ),
                Err(e) => format!("query error {e:?}"),
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

    // P1 ==============================================================================================================

    fn p1(label: &'static str, stopped: &'static str) -> String {
        let tag = format!("P1[{label}]");
        let mut p = Probe::new(&tag, &format!("run,connect,{stopped}|{STAY_UP}"), 1000);
        let _ = p.start();
        let first = p.connect(0, EXPECTED_BOUND).map(|c| c.body.clone());
        p.p(format_args!("first run connect-back: {first:?}"));
        let bound = EXPECTED_BOUND;
        let second = p.connect(1, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "after RUNNING, report STOPPED {stopped}: {r}; final {}",
            p.status()
        ));
        r
    }

    // P2 ==============================================================================================================

    fn p2(label: &'static str, stop_code: &'static str) -> String {
        let tag = format!("P2[{label}]");
        let delay = Duration::from_millis(1000);
        let mut p = Probe::new(
            &tag,
            &format!("run,connect,wait-stop,stopped:{stop_code}|{STAY_UP}"),
            1000,
        );
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let stop = p.stop();
        let bound = absence_bound(delay);
        let second = p.connect(1, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "commanded stop (stop() -> {stop:?}), host reports STOPPED {stop_code}: {r}; final {}",
            p.status()
        ));
        r
    }

    // P3 ==============================================================================================================

    fn p3(label: &'static str, first_run: &'static str) -> String {
        let tag = format!("P3[{label}]");
        let mut p = Probe::new(&tag, &format!("{first_run}|{STAY_UP}"), 1000);
        let start = p.start();
        let bound = EXPECTED_BOUND;
        let second = p.connect(0, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "failure before ever RUNNING ({first_run}): StartService -> {start:?}; {r}; final {}",
            p.status()
        ));
        r
    }

    // P4 ==============================================================================================================

    fn p4_stop() -> String {
        let tag = "P4[stop]";
        let delay = Duration::from_secs(15);
        let mut p = Probe::new(tag, &format!("run,connect,stopped:w32:1|{STAY_UP}"), 15000);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let stopped = p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        p.p(format_args!("reached STOPPED: {stopped}"));
        let stop = p.stop();
        let bound = absence_bound(delay);
        let second = p.connect(1, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "failed, 15s action pending, stop() -> {stop:?}: {r}; final {}",
            p.status()
        ));
        r
    }

    fn p4_start_stop() -> String {
        let tag = "P4[start-then-stop]";
        let delay = Duration::from_secs(15);
        let mut p = Probe::new(tag, &format!("run,connect,stopped:w32:1|{STAY_UP}"), 15000);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let stopped = p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        p.p(format_args!("reached STOPPED: {stopped}"));
        let manual = p.start();
        let manual_up = p.connect(1, EXPECTED_BOUND).map(|c| c.at.as_millis());
        p.p(format_args!("manual start connect-back at {manual_up:?}ms"));
        let stop = p.stop();
        let stopped_again = p.wait_state(ServiceState::Stopped, EXPECTED_BOUND);
        p.p(format_args!("stopped again: {stopped_again}"));
        let bound = absence_bound(delay);
        let third = p.connect(2, bound).cloned();
        let r = describe(third.as_ref(), bound);
        p.result(format_args!(
            "failed, 15s action pending, manual start -> {manual:?}, then stop() -> {stop:?}: {r}; final {}",
            p.status()
        ));
        r
    }

    // P5 ==============================================================================================================

    fn p5(label: &'static str, first_run: &'static str) -> String {
        let tag = format!("P5[{label}]");
        let delay = Duration::from_millis(1000);
        let mut p = Probe::new(&tag, &format!("{first_run}|{STAY_UP}"), 1000);
        let _ = p.start();
        p.connect(0, EXPECTED_BOUND);
        let bound = absence_bound(delay);
        let second = p.connect(1, bound).cloned();
        let r = describe(second.as_ref(), bound);
        p.result(format_args!(
            "host script `{first_run}` (the timeline above shows the SCM record just after the report/exit): {r}; final {}",
            p.status()
        ));
        r
    }

    // P6 ==============================================================================================================

    fn p6() -> String {
        let tag = "P6";
        let fail = "run,connect,stopped:w32:1";
        let mut p = Probe::new(tag, &format!("{fail}|{fail}|{fail}|{fail}|{STAY_UP}"), 1000);
        let _ = p.start();
        // Runs 0..=3 fail, run 4 stays up: 4 failures, so 4 restarts if the
        // single action repeats.
        let mut at = Vec::new();
        for n in 0..5 {
            match p.connect(n, EXPECTED_BOUND) {
                Some(c) => at.push(c.at.as_millis()),
                None => break,
            }
        }
        let r = format!(
            "{} runs connected back (expected 5 = 4 restarts), at ms {at:?}",
            at.len()
        );
        p.result(format_args!(
            "4 consecutive failures, one restart/1000 action: {r}; final {}",
            p.status()
        ));
        r
    }

    // Driver ==========================================================================================================

    pub fn run_all() {
        type Job = Box<dyn FnOnce() -> String + Send>;
        let jobs: Vec<(&'static str, Job)> = vec![
            ("P1[w32-13]", Box::new(|| p1("w32-13", "stopped:w32:13"))),
            ("P1[w32-31]", Box::new(|| p1("w32-31", "stopped:w32:31"))),
            ("P1[w32-1003]", Box::new(|| p1("w32-1003", "stopped:w32:1003"))),
            ("P1[spec-3]", Box::new(|| p1("spec-3", "stopped:spec:3"))),
            ("P1[spec-7]", Box::new(|| p1("spec-7", "stopped:spec:7"))),
            (
                "P1[spec-0xC0000005]",
                Box::new(|| p1("spec-0xC0000005", "stopped:spec:0xC0000005")),
            ),
            ("P2[1003]", Box::new(|| p2("1003", "w32:1003"))),
            ("P2[baseline-0]", Box::new(|| p2("baseline-0", "w32:0"))),
            (
                "P3[a-stopped-w32-2]",
                Box::new(|| p3("a-stopped-w32-2", "pending,stopped:w32:2")),
            ),
            ("P3[b-exit-1]", Box::new(|| p3("b-exit-1", "pending,exit:1"))),
            ("P4[stop]", Box::new(p4_stop)),
            ("P4[start-then-stop]", Box::new(p4_start_stop)),
            (
                "P5[spec7-then-exit0]",
                Box::new(|| p5("spec7-then-exit0", "run,connect,stopped:spec:7,exit:0")),
            ),
            (
                "P5[w32-0-then-exit7]",
                Box::new(|| p5("w32-0-then-exit7", "run,connect,stopped:w32:0,exit:7")),
            ),
            ("P6", Box::new(p6)),
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
