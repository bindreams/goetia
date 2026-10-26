//! The control handler registration of the test service hosts (`sentinel`'s `SERVICE_HOST` and
//! `scm_integration::fixture`), made directly through `RegisterServiceCtrlHandlerExW` the way the
//! shim does. `windows-service` 0.8.1's `register` frees its closure after any Stop, Shutdown or
//! Preshutdown whatever the handler returned, while the registration stays live; `clippy.toml`
//! forbids it. The context here is leaked and so outlives every control.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt as _;
use std::sync::mpsc::Sender;

use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus,
};

/// What the handler does with `SERVICE_CONTROL_STOP`: send on the channel, or (`None`) refuse it.
struct HandlerContext {
    stop: Option<Sender<()>>,
}

#[derive(Clone, Copy)]
pub struct StatusHandle(SERVICE_STATUS_HANDLE);

/// Register the handler for `name`. A `Some` sender is signalled on `STOP`.
pub fn register(name: &str, stop: Option<Sender<()>>) -> StatusHandle {
    let name: Vec<u16> = OsStr::new(name).encode_wide().chain(std::iter::once(0)).collect();
    let context: *mut HandlerContext = Box::into_raw(Box::new(HandlerContext { stop }));
    // SAFETY: `name` is NUL-terminated and outlives the call; the context is never freed.
    let handle = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), context.cast()) };
    assert!(
        !handle.is_null(),
        "register control handler: {}",
        std::io::Error::last_os_error()
    );
    StatusHandle(handle)
}

/// Report `RUNNING`, accepting `STOP` only if `accept_stop`.
pub fn report_running(handle: StatusHandle, accept_stop: bool) {
    report(
        handle,
        SERVICE_RUNNING,
        if accept_stop { SERVICE_ACCEPT_STOP } else { 0 },
    );
}

/// Report `STOPPED` with exit code 0.
pub fn report_stopped(handle: StatusHandle) {
    report(handle, SERVICE_STOPPED, 0);
}

fn report(handle: StatusHandle, state: u32, controls_accepted: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: controls_accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: `handle` came from `RegisterServiceCtrlHandlerExW`; `status` is a valid, live struct.
    let ok = unsafe { SetServiceStatus(handle.0, &status) };
    assert!(ok != 0, "report service status: {}", std::io::Error::last_os_error());
}

unsafe extern "system" fn handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    context: *mut c_void,
) -> u32 {
    // SAFETY: `context` is the leaked `HandlerContext` `register` passed.
    let context = unsafe { &*context.cast::<HandlerContext>() };
    match control {
        SERVICE_CONTROL_STOP => match &context.stop {
            Some(stop) => {
                let _ = stop.send(());
                NO_ERROR
            }
            None => ERROR_CALL_NOT_IMPLEMENTED,
        },
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}
