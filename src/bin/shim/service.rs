//! SCM wiring: dispatch, control handler, status reporting, and the
//! supervisor loop that spawns/waits/restarts the daemon `goetia.yaml`
//! describes.
//!
//! Everything that touches the child process tree — containment, waiting,
//! stdio capture — is `cosca`'s job, not this module's. `cosca::Command`'s
//! `.contain()` is a Windows Job Object with `KILL_ON_JOB_CLOSE`, the exact
//! mechanism this task's own brief first asked to have ported by hand from
//! `~/src/windows-service-manager/src/service/job_object.rs`; `cosca::Child::
//! wait()`/`wait_tree()` are real, event-driven kernel waits (a job's
//! membership-count edge for `wait_tree`), never the 50ms poll
//! `wsm`'s `wrapper.rs` uses and disclaims with its own TODO. This module's
//! own job is narrower: interrupting a blocking `child.wait()` when SCM
//! delivers `Stop` — see `stop_bus`, the one piece `cosca` does not supply,
//! since it has no notion of an external cancellation source.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use cosca::{Fd, Stdio};
use goetia::backend::scm::manager::read_spec_blob;
use goetia::spec::DaemonSpec;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::{define_windows_service, service_dispatcher};

use crate::logging;
use crate::stop_bus::{StopBus, WaitOutcome};
use crate::supervisor::{self, ChildOutcome, RestartDecision};

// Exit codes ==========================================================================================================
//
// Distinguishable process exit codes (constraint 4 of this task's brief):
// each failure class below both reports through `logging::log_failure`
// (fallback log path + Windows Event Log) and exits with its own code, so
// `sc.exe query`/Task Scheduler-style automation and a human reading
// `%ProgramData%\Goetia\logs\<id>.log` agree on what happened.

/// Commanded stop, or a `restart:` policy that does not respawn a clean
/// exit — not a failure.
const EXIT_OK: i32 = 0;
/// argv[1] (the service id) was not supplied. Only reachable from a
/// malformed manual invocation: SCM itself always supplies it (`ImagePath`
/// names `goetia-shim.exe <id>` — see `backend::scm::generate::registration`).
const EXIT_USAGE: i32 = 2;
/// The metadata blob under `Services\<id>\Parameters` could not be read or
/// decoded — an old shim against a newer/incompatible blob, or corruption.
const EXIT_DECODE_FAILURE: i32 = 3;
/// `service_control_handler::register`/`StartServiceCtrlDispatcherW` itself
/// failed, so the shim could never report anything else to SCM.
const EXIT_DISPATCH_FAILURE: i32 = 4;
/// The supervisor loop stopped with the daemon's most recent attempt a
/// failure (nonzero exit, or a spawn failure) under a `restart:` policy
/// that does not retry further (`never`, or the daemon simply never ran).
const EXIT_CHILD_FAILURE: i32 = 5;

// Entry ===============================================================================================================

/// Called from `main()`. Reads `argv[1]` (the service id), hands control to
/// SCM, and never returns normally: `run_service` (via `service_main`)
/// calls `std::process::exit` on every path, including every failure one,
/// so the process's own exit code is always one of the constants above.
pub fn run() -> ! {
    let args: Vec<String> = std::env::args().collect();
    let Some(id) = args.get(1) else {
        eprintln!("goetia-shim: usage: goetia-shim <service-id>");
        std::process::exit(EXIT_USAGE);
    };
    if let Err(e) = service_dispatcher::start(id, ffi_service_main) {
        logging::log_failure(id, &format!("connect to the SCM dispatcher: {e}"));
        std::process::exit(EXIT_DISPATCH_FAILURE);
    }
    // Unreachable in practice — every path inside `service_main` exits the
    // process directly, and SCM only lets `StartServiceCtrlDispatcherW`'s
    // wait return once the process is already being torn down — but exit
    // cleanly here too rather than falling off `main`.
    std::process::exit(EXIT_OK);
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_scm_args: Vec<OsString>) {
    // The service id argv[1] carried — re-read directly rather than
    // threaded through `service_main`'s own SCM-supplied arguments (whose
    // shape SCM controls, not this shim): `run` already confirmed argv[1]
    // is present before ever dispatching, and process argv does not change.
    let id = std::env::args()
        .nth(1)
        .expect("argv[1] was validated present in run() before service_dispatcher::start was ever called");
    run_service(&id);
}

fn run_service(id: &str) {
    let stop_bus = Arc::new(StopBus::new());
    let status_handle = match register_control_handler(id, &stop_bus) {
        Ok(h) => h,
        Err(e) => {
            logging::log_failure(id, &format!("register SCM control handler: {e}"));
            std::process::exit(EXIT_DISPATCH_FAILURE);
        }
    };

    let blob = match read_spec_blob(id) {
        Ok(Some(b)) => b,
        Ok(None) => {
            logging::log_failure(
                id,
                "no Goetia metadata found under Services\\<id>\\Parameters (Marker absent) — this service was \
                 not created by `goetia daemon install`",
            );
            report_stopped(
                &status_handle,
                ServiceExitCode::ServiceSpecific(EXIT_DECODE_FAILURE as u32),
            );
            std::process::exit(EXIT_DECODE_FAILURE);
        }
        Err(e) => {
            logging::log_failure(id, &format!("decode metadata blob: {e}"));
            report_stopped(
                &status_handle,
                ServiceExitCode::ServiceSpecific(EXIT_DECODE_FAILURE as u32),
            );
            std::process::exit(EXIT_DECODE_FAILURE);
        }
    };
    let spec = blob.spec;

    report_running(&status_handle);

    let code = supervisor_loop(&spec, &stop_bus, id);
    let exit_code = if code == EXIT_OK {
        ServiceExitCode::Win32(0)
    } else {
        ServiceExitCode::ServiceSpecific(code as u32)
    };
    report_stopped(&status_handle, exit_code);
    std::process::exit(code);
}

fn register_control_handler(id: &str, stop_bus: &Arc<StopBus>) -> windows_service::Result<ServiceStatusHandle> {
    let handler_bus = Arc::clone(stop_bus);
    let event_handler = move |control_event| match control_event {
        ServiceControl::Stop => {
            handler_bus.request_stop();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    service_control_handler::register(id, event_handler)
}

fn report_running(handle: &ServiceStatusHandle) {
    let _ = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}

fn report_stopped(handle: &ServiceStatusHandle, exit_code: ServiceExitCode) {
    let _ = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}

// Supervisor loop =====================================================================================================

/// Spawn, wait, restart — until `supervisor::decide_restart` says stop.
/// Returns the process exit code `run_service` reports both to SCM and via
/// `std::process::exit`.
fn supervisor_loop(spec: &DaemonSpec, stop_bus: &StopBus, id: &str) -> i32 {
    loop {
        // Checked before every spawn, not only after a wait: a stop
        // requested during the *previous* iteration's restart-delay wait
        // already returns from that wait's own call site below, but this
        // guard covers the first iteration too, where nothing has waited
        // yet.
        if stop_bus.is_stopping() {
            return EXIT_OK;
        }

        let mut cmd = build_command(spec, id);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                logging::log_failure(id, &format!("spawn {:?}: {e}", spec.command));
                let stopping = stop_bus.is_stopping();
                match supervisor::decide_restart(spec.restart, ChildOutcome::SpawnFailed, stopping, spec.restart_delay)
                {
                    RestartDecision::Stop => return if stopping { EXIT_OK } else { EXIT_CHILD_FAILURE },
                    RestartDecision::Respawn { delay } => {
                        if stop_bus.wait_or_stop(delay) {
                            return EXIT_OK;
                        }
                        continue;
                    }
                }
            }
        };

        let outcome = match stop_bus.wait_for_child_or_stop(&child) {
            WaitOutcome::ChildExited => {
                // The child has already exited — `wait()` reaps and returns
                // immediately rather than blocking.
                let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                ChildOutcome::Exited(code)
            }
            WaitOutcome::Stopping => {
                // Kill the whole tree (the direct child and every
                // descendant it spawned — the Job Object's job, not this
                // loop's), then confirm via `wait_tree`'s real kernel drain
                // edge that every member has actually exited before this
                // function's caller ever reports `SERVICE_STOPPED`. This
                // ordering — confirm dead, THEN decide (below), THEN report
                // stopped — is what makes "no new child appears after the
                // stop completes" true by construction rather than by
                // timing luck: nothing here can loop back to `Respawn`
                // once `stopping` is observed.
                if let Err(e) = child.kill_tree() {
                    logging::log_failure(id, &format!("kill_tree on stop: {e}"));
                }
                if let Err(e) = child.wait_tree() {
                    logging::log_failure(id, &format!("wait_tree confirming stop: {e}"));
                }
                let _ = child.wait();
                // Unused by `decide_restart` below once `stopping` is true
                // (checked first, unconditionally) — see its own doc
                // comment.
                ChildOutcome::Exited(-1)
            }
        };

        let stopping = stop_bus.is_stopping();
        match supervisor::decide_restart(spec.restart, outcome, stopping, spec.restart_delay) {
            RestartDecision::Stop => {
                let clean = stopping || matches!(outcome, ChildOutcome::Exited(0));
                return if clean { EXIT_OK } else { EXIT_CHILD_FAILURE };
            }
            RestartDecision::Respawn { delay } => {
                if stop_bus.wait_or_stop(delay) {
                    return EXIT_OK;
                }
                continue;
            }
        }
    }
}

/// Build the (unspawned) command for `spec`: argv, cwd, env, Job Object
/// containment, and stdout+stderr merged into the daemon's log file.
fn build_command(spec: &DaemonSpec, id: &str) -> cosca::Command {
    let mut cmd = cosca::run(spec.command.iter().cloned());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd.clone());
    }
    cmd.envs(spec.env.clone());
    cmd.contain();

    let log_path = spec.logs.clone().unwrap_or_else(|| logging::default_log_path(id));
    match logging::open_append(&log_path) {
        Ok(file) => {
            // `stderr` merges onto whatever `stdout` was just set to
            // (`Command::fd`'s resolution reads `Fd::STDOUT`'s already
            // -inserted target) — `stdout` must be set first.
            let _ = cmd.stdout(Stdio::from_file(file));
            let _ = cmd.stderr(Stdio::merge(Fd::STDOUT));
        }
        Err(e) => {
            logging::log_failure(
                id,
                &format!(
                    "open log file {}: {e} (this daemon's output will not be captured; it is still being run \
                     and supervised)",
                    log_path.display()
                ),
            );
            let _ = cmd.stdout(Stdio::null());
            let _ = cmd.stderr(Stdio::null());
        }
    }
    cmd
}
