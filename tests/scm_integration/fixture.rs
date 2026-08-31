//! The fixture process a `type: managed` daemon under test actually is:
//! this test binary, re-invoked as a real SCM service. `type: managed`
//! means SCM launches the daemon's own command directly (no shim), so
//! `goetia.yaml`'s `command` for these tests names this binary itself —
//! mirroring `tests/support/sentinel.rs`'s `SERVICE_HOST` mode, which
//! `tests/marker_inertness/scm.rs` already established this pattern with.
//!
//! `<exe> --goetia-scm-fixture <name> <start-port> <stop-port> [<mode>]`
//!
//! `mode` (default `plain`):
//! - `plain` — connect to `start-port` with no payload, then wait for
//!   `SERVICE_CONTROL_STOP`, report `STOPPED`, connect to `stop-port`.
//! - `env:<VAR>` — like `plain`, but writes `<VAR>`'s value read from this
//!   process's own environment (or `\0MISSING` if unset) to the
//!   `start-port` connection instead of just connecting — see
//!   `managed::managed_kind_environment_availability`.
//! - `refuse-stop` — reports `RUNNING` with an empty `controls_accepted`
//!   (declines `SERVICE_CONTROL_STOP` outright, so `ControlService` fails
//!   synchronously rather than ever needing a wait to time out — see
//!   `managed::uninstall_errors_when_service_will_not_stop`), connects to
//!   `start-port`, then blocks forever on a channel nobody sends to. Killed
//!   directly by its PID during that test's own cleanup, never via `stop`.

use std::ffi::OsString;
use std::net::{Ipv4Addr, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

pub const FIXTURE: &str = "--goetia-scm-fixture";

pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(FIXTURE) {
        return false;
    }
    // The ports and mode come from the image path SCM launched us with, not
    // from `StartServiceW`'s own arguments — read back off this process's
    // own command line, exactly like `support::sentinel::SERVICE_HOST`.
    let name = args[2].clone();
    service_dispatcher::start(&name, ffi_service_main).expect("connect to the SCM dispatcher");
    true
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_scm_arguments: Vec<OsString>) {
    let args: Vec<String> = std::env::args().collect();
    let name = args[2].clone();
    let start_port: u16 = args[3].parse().expect("start port");
    let stop_port: u16 = args[4].parse().expect("stop port");
    let mode = args.get(5).cloned().unwrap_or_else(|| "plain".to_string());
    let refuse_stop = mode == "refuse-stop";

    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop if !refuse_stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(&name, handler).expect("register control handler");

    let controls_accepted = if refuse_stop {
        ServiceControlAccept::empty()
    } else {
        ServiceControlAccept::STOP
    };
    let running = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle
        .set_service_status(running.clone())
        .expect("report RUNNING");

    if let Some(var) = mode.strip_prefix("env:") {
        let value = std::env::var(var).unwrap_or_else(|_| "\u{0}MISSING".to_string());
        send_line(start_port, &value);
    } else {
        connect(start_port);
    }

    if refuse_stop {
        // See the module doc comment: nothing ever signals this, by design
        // — the test exercising this mode kills the process directly.
        let (_never_tx, never_rx) = mpsc::channel::<()>();
        let _ = never_rx.recv();
        return;
    }

    // A real rendezvous with the control handler thread: this returns when
    // SCM delivers the stop, and not before.
    stop_rx.recv().expect("stop control");
    let stopped = ServiceStatus {
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        ..running
    };
    status_handle.set_service_status(stopped).expect("report STOPPED");
    connect(stop_port);
}

/// Reporting in is best-effort: a failure to connect is indistinguishable to
/// this process from never having been started, and it is the test — which
/// holds the listener — that decides what that means.
fn connect(port: u16) {
    drop(TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
}

fn send_line(port: u16, value: &str) {
    use std::io::Write as _;
    if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
        let _ = stream.write_all(value.as_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
}
