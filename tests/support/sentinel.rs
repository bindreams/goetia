//! Sentinel modes: the test binary re-invoked as the daemon under probe.
//!
//! The probes need a program the service manager can actually run, and before
//! any backend exists the only binary guaranteed to be on the host is this
//! one. `main` dispatches these modes before the harness starts, because in
//! them the process was launched by launchd or the SCM, not by a test runner.

use std::io::Write as _;
use std::net::{Ipv4Addr, TcpStream};

/// `<exe> --goetia-connect-back <port>`: connect to the test's listener and
/// exit. Used to prove a launchd job ran — or, by its absence after a
/// `bootout`, that it did not.
pub const CONNECT_BACK: &str = "--goetia-connect-back";

/// `<exe> --goetia-run-until-killed <port>`: connect to the test's listener
/// (proving the process is genuinely up and running, not merely that
/// `launchctl` returned success) and then block forever, so a test can go
/// on to observe `Running` state/pid and rely on it staying that way until
/// the manager stops it. Unlike `CONNECT_BACK`, which exits immediately
/// after reporting in and so could already be gone by the time a test's
/// next assertion runs.
pub const RUN_UNTIL_KILLED: &str = "--goetia-run-until-killed";

/// `<exe> --goetia-report-uid <port>`: connect to the test's listener and
/// send the process's real uid as decimal text, then exit. Used to prove
/// which account a job actually launched under — `UserName: root` in the
/// plist is a claim; this confirms launchd honoured it, proving whether
/// that key takes a name or a numeric uid (this backend always resolves
/// to a name; see `backend::launchd::manager::resolve_account`'s doc
/// comment).
#[cfg(unix)]
pub const REPORT_UID: &str = "--goetia-report-uid";

/// `<exe> --goetia-service-host <service-name> <start-port> <stop-port>`:
/// a minimal SCM-aware service. Without one, `sc start` would fail with
/// ERROR_SERVICE_REQUEST_TIMEOUT and the registry probe would be claiming to
/// have survived a start/stop cycle it never performed.
#[cfg(windows)]
pub const SERVICE_HOST: &str = "--goetia-service-host";

/// Returns whether this process was launched as a sentinel and has now
/// finished its job.
pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(CONNECT_BACK) => {
            connect(args[2].parse().expect("port argument"));
            true
        }
        Some(RUN_UNTIL_KILLED) => {
            connect(args[2].parse().expect("port argument"));
            loop {
                std::thread::park();
            }
        }
        #[cfg(unix)]
        Some(REPORT_UID) => {
            report_uid(args[2].parse().expect("port argument"));
            true
        }
        #[cfg(windows)]
        Some(SERVICE_HOST) => {
            windows::run(&args[2]);
            true
        }
        _ => false,
    }
}

/// Reporting in is best-effort: a failure to connect is indistinguishable to
/// this process from never having been started, and it is the test — which
/// holds the listener — that decides what that means.
fn connect(port: u16) {
    drop(TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
}

#[cfg(unix)]
fn report_uid(port: u16) {
    if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
        // SAFETY: `getuid` takes no arguments, reads no memory, and cannot fail.
        let uid = unsafe { libc::getuid() };
        let _ = write!(stream, "{uid}");
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::sync::mpsc;
    use std::time::Duration;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    define_windows_service!(ffi_service_main, service_main);

    pub fn run(name: &str) {
        service_dispatcher::start(name, ffi_service_main).expect("connect to the SCM dispatcher");
    }

    fn service_main(_scm_arguments: Vec<OsString>) {
        // The ports come from the image path, not from `StartService`, so they
        // are read back off this process's own command line.
        let args: Vec<String> = std::env::args().collect();
        let name = args[2].clone();
        let start_port: u16 = args[3].parse().expect("start port");
        let stop_port: u16 = args[4].parse().expect("stop port");

        let (stop_tx, stop_rx) = mpsc::channel();
        let handler = move |control| match control {
            ServiceControl::Stop => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = service_control_handler::register(&name, handler).expect("register control handler");

        let running = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        status_handle
            .set_service_status(running.clone())
            .expect("report RUNNING");
        super::connect(start_port);

        // A real rendezvous with the control handler thread: the recv returns
        // when SCM delivers the stop, and not before.
        stop_rx.recv().expect("stop control");
        let stopped = ServiceStatus {
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            ..running
        };
        status_handle.set_service_status(stopped).expect("report STOPPED");
        super::connect(stop_port);
    }
}
