//! SCM wiring: dispatch, control handler, status reporting, and the
//! supervisor loop that spawns/waits/restarts the daemon `goetia.yaml`
//! describes.
//!
//! Everything that touches the child process tree — containment, waiting,
//! stdio capture — is `cosca`'s job, not this module's: `cosca::Command`'s
//! `.contain()` is a Windows Job Object with `KILL_ON_JOB_CLOSE`, and
//! `cosca::Child::wait()`/`wait_tree()` are real, event-driven kernel
//! waits. This module's own job is narrower: interrupting a blocking
//! `child.wait()` when SCM delivers `Stop` (see `stop_bus`, the one piece
//! `cosca` does not supply, since it has no notion of an external
//! cancellation source) and driving SCM's own status protocol.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use cosca::{Child, Fd, Stdio};
use goetia::backend::scm::manager::read_spec_blob;
use goetia::spec::{DaemonSpec, Restart};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::{define_windows_service, service_dispatcher};

use crate::logging;
use crate::stop_bus::{StopBus, WaitOutcome, Waiter};
use crate::supervisor::{self, ChildOutcome, RestartDecision};

// Exit codes ==========================================================================================================
//
// Distinguishable process exit codes: each failure class below both reports
// through `logging::log_failure` (fallback log path + Windows Event Log)
// and exits with its own code, so `sc.exe query`/Task Scheduler-style
// automation and a human reading `%ProgramData%\Goetia\logs\<id>.log`
// agree on what happened.

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
                id,
                &status_handle,
                ServiceExitCode::ServiceSpecific(EXIT_DECODE_FAILURE as u32),
            );
            std::process::exit(EXIT_DECODE_FAILURE);
        }
        Err(e) => {
            logging::log_failure(id, &format!("decode metadata blob: {e}"));
            report_stopped(
                id,
                &status_handle,
                ServiceExitCode::ServiceSpecific(EXIT_DECODE_FAILURE as u32),
            );
            std::process::exit(EXIT_DECODE_FAILURE);
        }
    };
    let spec = blob.spec;

    let code = supervisor_loop(&spec, &stop_bus, id, &status_handle);
    let exit_code = if code == EXIT_OK {
        ServiceExitCode::Win32(0)
    } else {
        ServiceExitCode::ServiceSpecific(code as u32)
    };
    report_stopped(id, &status_handle, exit_code);
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

/// `set_service_status` is the only mechanism by which SCM learns this
/// service's state; a failure here is logged like every other Win32/cosca
/// failure in this file, not silently discarded — see `logging`'s module
/// doc comment for why both the fallback log and the Windows Event Log
/// need to carry it: SCM believing the service is still in whatever state
/// it last confirmed is exactly the kind of thing a human debugging a
/// stuck `stop`/`uninstall` needs to be able to find.
fn report_running(id: &str, handle: &ServiceStatusHandle) {
    if let Err(e) = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }) {
        logging::log_failure(id, &format!("report SERVICE_RUNNING to SCM: {e}"));
    }
}

fn report_stopped(id: &str, handle: &ServiceStatusHandle, exit_code: ServiceExitCode) {
    if let Err(e) = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }) {
        logging::log_failure(id, &format!("report SERVICE_STOPPED to SCM: {e}"));
    }
}

// Supervisor loop =====================================================================================================

/// Spawn, wait, restart — until `supervisor::decide_restart` says stop.
/// Returns the process exit code `run_service` reports both to SCM and via
/// `std::process::exit`.
fn supervisor_loop(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, id: &str, status_handle: &ServiceStatusHandle) -> i32 {
    // `restart: on-failure`/`always` retries indefinitely regardless of any
    // one spawn's outcome, so for those policies `Running` means "the
    // supervisor itself is alive and will keep trying" — reported up
    // front, matching what `ScmManager::start`'s own real
    // `NotifyServiceStatusChangeW` wait (`scm::wait::start_via_notify`)
    // expects promptly. `restart: never` makes exactly one spawn attempt
    // ever, so reporting `Running` before that attempt resolves would race
    // an immediate `Stopped`: `start_via_notify`'s single-shot notify can
    // observe either the transient `Running` or the subsequent `Stopped`
    // depending purely on scheduling, so `goetia daemon start` could
    // report success for a daemon whose command does not even exist.
    // Deferring the report until the one attempt's outcome is known (below)
    // makes that a deterministic failed start instead.
    let defer_running_report = spec.restart == Restart::Never;
    if !defer_running_report {
        report_running(id, status_handle);
    }

    loop {
        // Checked before every spawn, not only after a wait: a stop
        // requested during the *previous* iteration's restart-delay wait
        // already returns from that wait's own call site below, but this
        // guard covers the first iteration too, where nothing has waited
        // yet.
        if stop_bus.is_stopping() {
            return EXIT_OK;
        }

        // A thread the OS refused and a daemon that could not be spawned are
        // one fact to the restart policy — nothing is running, and nothing
        // was launched — so they share this path deliberately, rather than
        // the first spawn and a later respawn differing by accident. That
        // makes a waiter-thread failure under `restart: never` exactly a
        // clean start failure with no daemon launched: it is the one policy
        // that has not yet told SCM `RUNNING` (see `defer_running_report`
        // above), and its loop body runs exactly once. Under
        // `on-failure`/`always` — already `RUNNING` before the first spawn,
        // by that same policy — it is instead a retry at `restart-delay`'s
        // cadence, on the same reasoning as a persistently-failing spawn:
        // thread exhaustion is a transient, machine-wide condition, and
        // tearing down a service the user asked to be kept running costs
        // more than one thread-creation attempt per delay.
        let Some((child, waiter)) = launch(spec, stop_bus, id) else {
            let stopping = stop_bus.is_stopping();
            match supervisor::decide_restart(spec.restart, ChildOutcome::SpawnFailed, stopping, spec.restart_delay) {
                RestartDecision::Stop => return if stopping { EXIT_OK } else { EXIT_CHILD_FAILURE },
                RestartDecision::Respawn { delay } => {
                    if stop_bus.wait_or_stop(delay) {
                        return EXIT_OK;
                    }
                    continue;
                }
            }
        };
        if defer_running_report {
            report_running(id, status_handle);
        }

        // `wait_for_child_or_stop` also performs the kill-tree-and-confirm
        // teardown on a stop (see its own doc comment for why that has to
        // happen inside the call rather than out here) — by the time it
        // returns `Stopping`, the whole tree is already confirmed dead, or,
        // if `kill_tree` itself failed and the fallback kill did not, the
        // direct child is and its descendants (if any) were unreachable.
        // Either way it is logged there, and nothing is left to reap here.
        let outcome = match stop_bus.wait_for_child_or_stop(waiter, &child, id) {
            WaitOutcome::ChildExited => {
                // The child has already exited — `wait()` reaps and returns
                // immediately rather than blocking.
                match child.wait() {
                    Ok(status) => ChildOutcome::Exited(status.code().unwrap_or_else(|| {
                        logging::log_failure(id, "child exited with no reportable exit code; treating as failed");
                        -1
                    })),
                    Err(e) => {
                        logging::log_failure(id, &format!("re-query child exit status: {e}"));
                        ChildOutcome::Exited(-1)
                    }
                }
            }
            // Unused by `decide_restart` below once `stopping` is true
            // (checked first, unconditionally) — see its own doc comment.
            WaitOutcome::Stopping => ChildOutcome::Exited(-1),
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

/// The daemon, running and already being waited on — or `None`, logged, with nothing launched.
///
/// Everything supervising the daemon needs is made before the daemon is: the [`Waiter`] thread
/// first, then the spawn, and nothing between them that could launch a daemon on the way.
/// `build_command` does sit between them, and the log-file open inside it fails routinely, but
/// that failure arm falls back to null stdio and runs nothing. A daemon is never launched with
/// nothing to wait on it, because the one thing that could refuse to make that waiter — the OS,
/// asked for a thread — is asked before anything is running. See [`Waiter`] for what the reverse
/// order cost.
fn launch(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, id: &str) -> Option<(Arc<Child>, Waiter)> {
    // TEMPORARY PROBE, reverted in the next commit: the pre-fix order, daemon
    // spawned before the waiter exists. Pushed only to record that BOTH the
    // counter assertion and the new on-disk witness go red against the defect.
    let mut cmd = build_command(spec, id);
    let child = match start(&mut cmd) {
        Ok(child) => child,
        Err(e) => {
            logging::log_failure(id, &format!("spawn {:?}: {e}", spec.command));
            return None;
        }
    };
    let waiter = match stop_bus.waiter(id) {
        Ok(waiter) => waiter,
        Err(e) => {
            logging::log_failure(id, &format!("make the thread that waits on {:?}: {e}", spec.command));
            return None;
        }
    };
    Some((Arc::new(child), waiter))
}

/// The spawn itself: [`launch`]'s last step, once the thread that will wait on the daemon has
/// already been made.
fn start(cmd: &mut cosca::Command) -> Result<Child, cosca::error::Error> {
    #[cfg(test)]
    test_hook::spawning();
    cmd.spawn()
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
            // Merges are resolved in a second pass at spawn time (`cosca`'s
            // `resolve_stdio`), not by `Command::fd` consulting whatever is
            // already in its fd map — so the two calls below are order
            // -independent. `Command::stdout`/`stderr` can fail only for a
            // slot whose target is itself ambiguous or another merge
            // (`Error::Unsupported`); neither applies to a concrete
            // `Stdio::from_file`/`Stdio::merge(Fd::STDOUT)` pair on
            // stdout/stderr, so the discarded `Result`s below are believed
            // infallible for this exact shape, not merely convenient to
            // ignore.
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
            // Same infallibility reasoning as above, for `Stdio::null()`.
            let _ = cmd.stdout(Stdio::null());
            let _ = cmd.stderr(Stdio::null());
        }
    }
    cmd
}

// test_hook ===========================================================================================================

/// Counts daemon spawns, on this thread only, so a test can assert that none happened. Mirrors
/// `goetia::backend::bounded`'s own hook, which is `#[cfg(test)]` inside the library and
/// therefore invisible here — see `stop_bus::Waiter`.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;

    thread_local! {
        static SPAWNS: Cell<usize> = const { Cell::new(0) };
    }

    /// How many daemons this thread has spawned through [`super::start`], or tried to: the count
    /// moves before `cmd.spawn()`, so a spawn that launched nothing is counted too — which is what
    /// makes "nothing was launched" assertions against it strictly stronger.
    pub(crate) fn spawns() -> usize {
        SPAWNS.get()
    }

    pub(super) fn spawning() {
        SPAWNS.set(SPAWNS.get() + 1);
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
