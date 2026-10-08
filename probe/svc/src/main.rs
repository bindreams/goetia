//! Throwaway probe: a service that, on SERVICE_CONTROL_STOP, ends the way
//! `<mode>` says. `probe-svc.exe <name> <clean|specific6|crash>`.
use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

define_windows_service!(ffi_main, svc_main);

/// The probe's release gate: a manual-reset event the probe opens and sets.
fn gate(name: &str) -> HANDLE {
    let w: Vec<u16> = format!("Global\\goetiaprobe-gate-{name}").encode_utf16().chain([0]).collect();
    let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, w.as_ptr()) };
    assert!(!ev.is_null(), "CreateEventW");
    ev
}

fn main() {
    let name = std::env::args().nth(1).expect("name");
    service_dispatcher::start(name, ffi_main).expect("dispatcher");
}

fn status(state: ServiceState, accept: ServiceControlAccept, exit: ServiceExitCode) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: exit,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn svc_main(_: Vec<OsString>) {
    let args: Vec<String> = std::env::args().collect();
    let (name, mode) = (args[1].clone(), args[2].clone());
    let (tx, rx) = mpsc::channel();
    let h = service_control_handler::register(&name, move |c| match c {
        ServiceControl::Stop => {
            let _ = tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .expect("register");
    if mode == "pendstart" {
        let ev = gate(&name);
        let mut st = status(ServiceState::StartPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0));
        st.checkpoint = 1;
        st.wait_hint = Duration::from_secs(120);
        h.set_service_status(st).expect("start pending");
        unsafe { WaitForSingleObject(ev, INFINITE) };
    }
    h.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP, ServiceExitCode::Win32(0)))
        .expect("running");
    rx.recv().expect("stop");
    match mode.as_str() {
        "pendstop" => {
            let ev = gate(&name);
            let mut st = status(ServiceState::StopPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0));
            st.checkpoint = 1;
            st.wait_hint = Duration::from_secs(120);
            let _ = h.set_service_status(st);
            unsafe { WaitForSingleObject(ev, INFINITE) };
            let _ = h.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), ServiceExitCode::ServiceSpecific(6)));
            std::process::exit(6);
        }
        "pendstart" => {
            let _ = h.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), ServiceExitCode::Win32(0)));
            std::process::exit(0);
        }
        "clean" => {
            let _ = h.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), ServiceExitCode::Win32(0)));
            std::process::exit(0);
        }
        "specific6" => {
            let _ = h.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), ServiceExitCode::ServiceSpecific(6)));
            std::process::exit(6);
        }
        _ => std::process::exit(1), // crash: never reports STOPPED
    }
}
