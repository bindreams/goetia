//! Task mode: the shim as the action of a Task Scheduler task. It reads the task it was started
//! for, applies the spec's `env` and `logs` to the program, runs the program through the same
//! supervised pass a service uses ([`run_pass`]), and writes one record of how the run ended
//! ([`Runs`]), which `goetia daemon run` and status read.
//!
//! The arguments say which mode the shim is in: a task's action ends in
//! [`TASK_MODE_ARG`], the task's id and the registration's token ([`requested`]), and anything else
//! is service mode. Task mode never reports to SCM.
//!
//! **A stop** reaches the shim as `CTRL_CLOSE_EVENT` on its console, from `IRunningTask::Stop`.
//! The handler is registered before anything is read, so a stop at any point is held by the bus. It
//! only requests the stop and then never returns: Windows ends the process about a second after the
//! event whatever the handler does, and the run thread alone ends it with the run's own code.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use windows_sys::Win32::Foundation::{ERROR_INVALID_DATA, FALSE, TRUE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler};
use windows_sys::core::BOOL;

use super::exit;
use super::logging::Log;
use super::os::Real;
use super::service::{PassOutcome, SpecStop, run_pass, set_daemon_log};
use super::stop_bus::{StopBus, Supervisor};
use crate::backend::scm::exit_status::Reported;
use crate::backend::scm::folder::program_data;
use crate::backend::scm::instance::{InstanceId, TaskPath};
use crate::backend::scm::manager::task::clock::file_time_of;
use crate::backend::scm::manager::task::{Com, TaskApi};
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::backend::scm::task_xml::{TASK_MODE_ARG, TaskXml, extract_from_data};
use crate::error::Chain;
use crate::spec::{DaemonSpec, Id};

/// What a task ends with when a stop ended it: `STATUS_CONTROL_C_EXIT`, which a console program
/// that a close event ends leaves as its code. `LastTaskResult` is `0x41306` after any stop whatever
/// the process exits with, so the run's record is what carries the outcome.
const STOPPED_EXIT: u32 = 0xC000_013A;

const NOT_A_TASK: &str = "this is a service's blob; task mode runs only tasks";

// Entry ===============================================================================================================

/// The task's id and the token of the registration the shim was started under, when `args` (the
/// process's arguments after the program) end with [`TASK_MODE_ARG`], an id and a token: task mode.
/// Anything else is service mode.
pub(super) fn requested(args: &[OsString]) -> Option<(&str, &str)> {
    match args {
        [.., flag, id, token] if *flag == *TASK_MODE_ARG => Some((id.to_str()?, token.to_str()?)),
        _ => None,
    }
}

/// Runs the task `id` started under the registration `token` and ends the process with the run's code.
/// Never returns.
pub(super) fn run(id: &str, token: &str) -> ! {
    let real = Arc::new(Real::default());
    let log = Arc::new(Log::real(Arc::clone(&real)));
    log.hold_default(id);
    exit::keep_log(Arc::clone(&log));
    exit::name_run(id);
    let supervisor = Supervisor::new(real, Arc::clone(&log));
    // Before anything is read: from here on a stop is held by the bus until the run reaches it.
    if let Err(e) = register_close_handler(&supervisor.bus) {
        log.failure(id, &format!("register the console control handler: {}", Chain(&e)));
        exit::end_process(Reported::shim_failure(e.raw_os_error()).process_exit_code());
    }
    let runs = match (program_data(), Id::try_from(id)) {
        (Ok(program_data), Ok(id)) => Some(Runs::of(&program_data, &id)),
        (Err(e), _) => {
            log.failure(
                id,
                &format!(
                    "the run is not recorded: the machine's %ProgramData% could not be located: {}",
                    e.report()
                ),
            );
            None
        }
        // A task that is not an id is refused by `run_in`.
        (Ok(_), Err(_)) => None,
    };
    let code = run_in(id, token, &supervisor, &Com, runs.as_ref(), std::process::id());
    exit::end_process(code as i32)
}

/// Runs the task `id` as the process `own_pid`, started under the registration `token`, and returns the
/// code the process is to end with: the program's own, or the stop's ([`STOPPED_EXIT`]), or the
/// incomplete stop's, or a shim failure's.
///
/// In order, every failure before the program starts logged: reads the task through `api` (a service's
/// blob is refused, and so is a registration whose token is not `token`: the task was registered again
/// since this shim was started), waits for its own instance by `own_pid` to be listed
/// ([`own_instance`]), sets the diagnostics log, writes the run's marker as the first line of `logs` when there is one, runs the
/// program, and writes the one record into `runs`. A listing that cannot be read, and a record that
/// cannot be written, are logged and do not stop the program or change its code. A stop during the
/// wait starts no program: the run is recorded stopped, or logged unrecorded, and ends with the stop's
/// code.
pub(super) fn run_in(
    id: &str,
    token: &str,
    supervisor: &Supervisor,
    api: &dyn TaskApi,
    runs: Option<&Runs>,
    own_pid: u32,
) -> u32 {
    let log = &*supervisor.log;
    let (spec, path) = match own_spec(id, token, api) {
        Ok(read) => read,
        Err(stop) => {
            log.failure(id, &stop.message);
            return stop.reported.process_exit_code() as u32;
        }
    };
    let instance = match own_instance(id, api, &path, own_pid, supervisor) {
        OwnInstanceOutcome::Listed(instance) => Some(instance),
        OwnInstanceOutcome::Unread => None,
        OwnInstanceOutcome::Stopped(instance) => {
            record_run(log, id, runs, instance, file_time_now(), Some(RunOutcome::Stopped));
            return STOPPED_EXIT;
        }
    };
    set_daemon_log(log, id, &spec);
    if let (Some(logs), Some(instance)) = (spec.common().logs.as_deref(), &instance) {
        write_marker(supervisor, id, logs, instance);
    }

    let started = file_time_now();
    let (record, code) = ended_as(run_pass(&spec, supervisor, id, || {}, || {}));
    record_run(log, id, runs, instance, started, record);
    code
}

/// Writes the run's record when it has everything one needs; a write that fails is logged.
fn record_run(
    log: &Log,
    id: &str,
    runs: Option<&Runs>,
    instance: Option<InstanceId>,
    started: u64,
    outcome: Option<RunOutcome>,
) {
    if let (Some(runs), Some(instance), Some(outcome)) = (runs, instance, outcome) {
        match runs.write(&RunRecord {
            instance,
            started,
            outcome,
        }) {
            Ok(problems) => {
                for problem in problems {
                    log.warning(id, &format!("prune the run records: {problem}"));
                }
            }
            Err(e) => log.failure(id, &format!("write the run record: {}", Chain(&e))),
        }
    }
}

// The task ------------------------------------------------------------------------------------------------------------

/// The task's own spec, and its path. A task that is gone, not goetia's, undecodable or a service's is
/// refused; a read that did not complete is a shim failure carrying the OS error it met. So is a
/// registration whose token is not `token`, the one this shim was started under, or that has none.
fn own_spec(id: &str, token: &str, api: &dyn TaskApi) -> Result<(DaemonSpec, TaskPath), SpecStop> {
    let refused = |message: String| SpecStop {
        message,
        reported: Reported::Refused,
    };
    let task = Id::try_from(id).map_err(|e| refused(format!("`{id}` is not a goetia id: {}", e.report())))?;
    let path = TaskPath::of(&task);
    let registered = match api.get(&path) {
        Ok(Some(registered)) => registered,
        Ok(None) => {
            return Err(refused(format!(
                "the task {} is gone: no program was started",
                path.as_str()
            )));
        }
        Err(e) => {
            return Err(SpecStop {
                message: format!("read the task {}: {}", path.as_str(), e.report()),
                reported: Reported::shim_failure(Some(e.raw_os_error().unwrap_or(ERROR_INVALID_DATA as i32))),
            });
        }
    };
    let spec = match extract_from_data(&registered.data) {
        Ok(Some(blob)) if matches!(blob.spec, DaemonSpec::Task(_)) => blob.spec,
        Ok(Some(_)) => return Err(refused(NOT_A_TASK.to_string())),
        Ok(None) => {
            return Err(refused(
                "the task's Data holds no Goetia marker: this task was not created by `goetia daemon install`"
                    .to_string(),
            ));
        }
        Err(e) => return Err(refused(format!("decode the task's Data: {}", e.report()))),
    };
    let not_started_under = |message: String| SpecStop {
        message,
        reported: Reported::shim_failure(None),
    };
    match TaskXml::token_of(&registered.xml) {
        Ok(Some(registered)) if registered.as_str() == token => Ok((spec, path)),
        Ok(Some(_)) => Err(not_started_under(format!(
            "the task {} has a newer registration than the one this shim was started under: no program was started",
            path.as_str()
        ))),
        Ok(None) => Err(not_started_under(format!(
            "the task {}'s registration carries no token, so it is not the one this shim was started under: no \
             program was started",
            path.as_str()
        ))),
        Err(e) => Err(not_started_under(format!(
            "read the registration of the task {}: {}",
            path.as_str(),
            e.report()
        ))),
    }
}

/// How the wait for the run's own instance ended.
enum OwnInstanceOutcome {
    /// Task Scheduler lists an instance with the shim's pid.
    Listed(InstanceId),
    /// The listing could not be read; said in the log. The run has no name to record under, and the
    /// program runs anyway.
    Unread,
    /// A stop arrived while no such instance was listed, so no program is to start. Holds the
    /// instance when the read the stop woke found it; when it did not, that is said in the log.
    Stopped(Option<InstanceId>),
}

/// The pace of the re-reads of Task Scheduler's listing while it does not hold the shim yet. Each
/// read decides; the pace only keeps the loop from spinning.
const LISTING_PACE: Duration = Duration::from_millis(100);

/// Waits for the run's own instance: the one Task Scheduler lists with the shim's pid. Task Scheduler
/// lists a run once it has started the action's process, which can be after that process reads the
/// listing, so a listing without the shim is read again, paced and without a cap: the shim is the
/// instance's process, and a task that was stopped or deleted meanwhile ends it. A failed read ends
/// the wait ([`OwnInstanceOutcome::Unread`]); a stop does too, after one more read.
fn own_instance(
    id: &str,
    api: &dyn TaskApi,
    path: &TaskPath,
    own_pid: u32,
    supervisor: &Supervisor,
) -> OwnInstanceOutcome {
    let log = &supervisor.log;
    let read = || {
        api.instances(path).map(|instances| {
            instances
                .iter()
                .find(|instance| instance.pid == own_pid)
                .map(|instance| instance.id)
        })
    };
    loop {
        match read() {
            Ok(Some(own)) => return OwnInstanceOutcome::Listed(own),
            Ok(None) => {}
            Err(e) => {
                log.failure(
                    id,
                    &format!(
                        "list the task's instances: {}: the run is not recorded; the program is run anyway",
                        e.report()
                    ),
                );
                return OwnInstanceOutcome::Unread;
            }
        }
        if supervisor.bus.stopped_within(LISTING_PACE) {
            let own = match read() {
                Ok(own) => own,
                Err(e) => {
                    log.failure(id, &format!("list the task's instances: {}", e.report()));
                    None
                }
            };
            if own.is_none() {
                log.failure(
                    id,
                    &format!(
                        "stopped before the program started, and no listed instance of this task has process id \
                         {own_pid}: the run is not recorded"
                    ),
                );
            }
            return OwnInstanceOutcome::Stopped(own);
        }
    }
}

// The run -------------------------------------------------------------------------------------------------------------

/// Writes the run's marker as a line of its own into the log, before the program is spawned: the
/// program's output is appended after it. The log is the one the program's output goes to ([`Log::daemon_log`]):
/// the default log, through the handle the shim holds on it, when `logs` is that. A log that cannot be opened,
/// and a write that fails, are logged (an open fault is told once in a launch, [`Log::marker_open_failed`]) and
/// the program runs.
fn write_marker(supervisor: &Supervisor, id: &str, logs: &Path, instance: &InstanceId) {
    let log = &*supervisor.log;
    let (path, kind) = log
        .daemon_log(id, Some(logs))
        .expect("a `logs` path is the daemon's own log or the default log");
    let mut file = match supervisor.os.open_log(&path, kind) {
        Ok(file) => file,
        Err(e) => return log.marker_open_failed(id, &path, &e),
    };
    let line = format!("{}\n", instance.marker());
    if let Err(e) = file.write_all(line.as_bytes()) {
        log.failure(
            id,
            &format!(
                "write the run's marker to {}: {}; a reader cannot find this run's output in it",
                path.display(),
                Chain(&e)
            ),
        );
    }
}

/// How a pass ended as the record of the run, if it has one, and the code the process ends with.
///
/// A run with no record is one the shim failed: no exit code of the program's was had, or none ran.
/// `run` then reports that the run is unrecorded, and the log says why.
fn ended_as(outcome: PassOutcome) -> (Option<RunOutcome>, u32) {
    match outcome {
        PassOutcome::Exited(Some(code)) => (Some(RunOutcome::Exited(code as u32)), code as u32),
        PassOutcome::Stopped => (Some(RunOutcome::Stopped), STOPPED_EXIT),
        // The program may be running: a launch that may have started it says so as a stop does.
        PassOutcome::IncompleteStop | PassOutcome::NotRun(Reported::IncompleteStop) => (
            Some(RunOutcome::IncompleteStop),
            Reported::incomplete_stop().process_exit_code() as u32,
        ),
        PassOutcome::Exited(None) | PassOutcome::WaitFailed(_) | PassOutcome::NotRun(_) => {
            (None, outcome.reported().process_exit_code() as u32)
        }
    }
}

/// Now, as a `FILETIME`: what a record's `started` holds, which only orders runs, so a clock that
/// cannot be read as one is 0.
fn file_time_now() -> u64 {
    file_time_of(SystemTime::now()).map_or(0, |time| {
        u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime)
    })
}

// The close event -----------------------------------------------------------------------------------------------------

/// The bus the console handler asks for a stop on. A console handler has no context, so the bus is
/// reached through this static, which holds it, and so keeps it alive, for the life of the process.
static CLOSE_BUS: OnceLock<Arc<StopBus>> = OnceLock::new();

fn register_close_handler(bus: &Arc<StopBus>) -> io::Result<()> {
    let first = CLOSE_BUS.set(Arc::clone(bus)).is_ok();
    debug_assert!(first, "task mode registered its console handler twice");
    // SAFETY: `control_handler` is a function, so it lives for the whole process; `TRUE` adds it.
    if unsafe { SetConsoleCtrlHandler(Some(control_handler), TRUE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

extern "system" fn control_handler(event: u32) -> BOOL {
    match CLOSE_BUS.get() {
        Some(bus) => on_event(bus, event),
        None => FALSE,
    }
}

/// The handler's answer to `event`:
///
/// - `CTRL_CLOSE_EVENT` requests a stop and never returns.
/// - `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` are swallowed (`TRUE`). The program shares the shim's
///   console, so one it raises or one aimed at the console's group arrives at the shim too; the program
///   gets its own copy and decides for itself, and the default handler would end the shim, and the run
///   with it, with no record.
/// - Every other event (`CTRL_LOGOFF_EVENT`, `CTRL_SHUTDOWN_EVENT`) is left to the default handler
///   (`FALSE`).
fn on_event(bus: &StopBus, event: u32) -> BOOL {
    match event {
        CTRL_CLOSE_EVENT => {
            bus.request_stop();
            hold_until_the_process_ends()
        }
        CTRL_C_EVENT | CTRL_BREAK_EVENT => TRUE,
        _ => FALSE,
    }
}

/// Holds the system's handler thread: Windows ends the process once the handler returns or its
/// window passes, and the run thread, which alone ends it, must be the one to say with what code.
fn hold_until_the_process_ends() -> ! {
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod task_tests;
