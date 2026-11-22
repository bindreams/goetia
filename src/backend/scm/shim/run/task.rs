//! Task mode: the shim as the action of a Task Scheduler task. It reads the task it was started
//! for, applies the spec's `env` and `logs` to the program, runs the program through the same
//! supervised pass a service uses ([`run_pass`]), and writes one record of how the run ended
//! ([`Runs`]), which `goetia daemon run` and status read.
//!
//! The arguments say which mode the shim is in: a task's action ends in
//! `--goetia-task`, the task's id and the registration's token ([`requested`]), and anything else
//! is service mode. Task mode never reports to SCM.
//!
//! **A stop** reaches the shim as `CTRL_CLOSE_EVENT` on its console, from `IRunningTask::Stop`, and
//! reaches the program too, on the console they share: goetia sends nothing. The handler is registered
//! before anything is read, so a stop at any point is held by the bus. It only requests the stop and then
//! never returns: Windows ends the process about a second after the event whatever the handler does, and
//! the run thread alone ends it with the run's own code.
//!
//! What the run thread does with an accepted stop, in this order: it writes the run's record as
//! [`RunOutcome::Stopped`] first, since Windows may end the shim before anything later; it waits for the
//! program's own exit, with no cap of goetia's ([`StopPlan::WaitOnly`]); it tears down what the program left;
//! and it ends with [`STOPPED_EXIT`], or with the incomplete stop's code when the teardown fails, the record
//! replaced by the same write. A shim that Windows ends first leaves the `Stopped` record, which says that a
//! stop was accepted, not that the tree ended; the Job Object's `KILL_ON_JOB_CLOSE` ends the tree. A stop
//! accepted before the spawn starts no program, which the close event would never reach: the bus is looked at
//! right before the spawn, and the one stop that arrives between that look and the spawn starts the program,
//! which runs until Windows ends the shim and the job closes over it. A run in flight at a system shutdown
//! is ended by Windows and may leave no record: nothing here handles `CTRL_SHUTDOWN_EVENT`.

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
use super::stop_bus::{StopBus, StopPlan, Supervisor, WaitEnd};
use crate::backend::scm::exit_status::Reported;
use crate::backend::scm::folder::program_data;
use crate::backend::scm::instance::{InstanceId, TaskPath};
use crate::backend::scm::manager::task::api::Registered;
use crate::backend::scm::manager::task::clock::file_time_of;
use crate::backend::scm::manager::task::{Com, TaskApi};
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs, Writing};
use crate::backend::scm::task_xml::{TaskXml, Token, ending, extract_from_data};
use crate::error::{Chain, Error};
use crate::spec::{DaemonSpec, Id};

/// What a task ends with when a stop ended it: `STATUS_CONTROL_C_EXIT`, which a console program
/// that a close event ends leaves as its code. `LastTaskResult` is `0x41306` after any stop whatever
/// the process exits with, so the run's record is what carries the outcome.
const STOPPED_EXIT: u32 = 0xC000_013A;

const NOT_A_TASK: &str = "this is a service's blob; task mode runs only tasks";

// Entry ===============================================================================================================

/// The task's id and the token of the registration the shim was started under, when `args` (the
/// process's arguments after the program) have an [`ending`]: task mode. The token is `None` for an
/// ending that has no well-formed one, which [`run_in`] reports. Anything else is service mode.
pub(super) fn requested(args: &[OsString]) -> Option<(String, Option<Token>)> {
    let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
    ending(&args).map(|(id, token)| (id.to_owned(), token))
}

/// Runs the task `id` started under the registration `token` and ends the process with the run's code.
/// Never returns.
pub(super) fn run(id: &str, token: Option<Token>) -> ! {
    let real = Arc::new(Real::default());
    let program_data = program_data();
    // Closed to the default log until the pin is taken: a line before then tells the Event Log alone.
    let log = Arc::new(Log::from_program_data(real.clone(), &program_data).closed_until_held());
    exit::keep_log(Arc::clone(&log));
    exit::name_run(id);
    let supervisor = Supervisor::new(real, Arc::clone(&log));
    // Before anything is read: from here on a stop is held by the bus until the run reaches it.
    if let Err(e) = register_close_handler(&supervisor.bus) {
        log.failure(id, &format!("register the console control handler: {}", Chain(&e)));
        exit::end_process(Reported::shim_failure(e.raw_os_error()).process_exit_code());
    }
    let code = run_in(
        id,
        token,
        &supervisor,
        &Com,
        program_data.as_deref(),
        std::process::id(),
    );
    exit::end_process(code as i32)
}

/// Runs the task `id` as the process `own_pid`, started under the registration `token`, and returns the
/// code the process is to end with: the program's own, or the stop's ([`STOPPED_EXIT`]), or the
/// incomplete stop's, or a shim failure's.
///
/// The program starts only when every step below passed, in this order; a step that fails logs the case
/// (the default log when it is held, the Event Log always), releases what the shim holds and ends the
/// process with a shim failure, carrying the OS error the step met or none. A task that is not an id is
/// refused first, and reads nothing. The shim's log is closed to the default file until the pin is taken
/// ([`Log::closed_until_held`]), so a step before it tells the Event Log alone.
///
/// 1. Takes a pin on the run records ([`Runs::hold`]), first: an uninstall or an account change places the
///    marker and then reads every pin, so it reads this pin or this shim sees its marker. **Any** failure
///    ends the shim (the marker stands, the directory is denied, gone or a link, or `%ProgramData%` could
///    not be located).
/// 2. Holds the default log ([`Log::hold_default`]); a failure of that is told and the shim goes on.
/// 3. Reads the task through `api`: a task that is gone, a read that fails, and a registration that is not
///    the one this shim was started under (by `token`) end the shim; a blob that is foreign, undecodable or a
///    service's is refused.
/// 4. Waits for its own instance by `own_pid` to be listed ([`own_instance`]); a listing that cannot be
///    read, and a registration that went while it waited, end the shim. A stop during the wait starts no
///    program: the run is recorded stopped and ends with the stop's code. A stop after the wait is held by
///    the bus until step 6 looks at it.
/// 5. Looks for the removal's marker again ([`Runs::is_closed`]), before anything opens a log or spawns:
///    a marker that stands, or a look that cannot be made, ends the shim.
/// 6. Sets the diagnostics log, writes the run's marker as the first line of `logs` when there is one, looks at
///    the bus once more and, for a stop accepted by then, starts no program: the record is `Stopped`, as in
///    step 4. Otherwise runs the program and writes the one record, under the pin, which is released as the
///    record is renamed in place ([`Runs::write`]). An accepted stop writes `Stopped` before the wait, with
///    the pin lent, so that the run stays one that is alive; the final write replaces it. A record that
///    cannot be written is logged and does not change the code.
pub(super) fn run_in(
    id: &str,
    token: Option<Token>,
    supervisor: &Supervisor,
    api: &dyn TaskApi,
    program_data: Result<&Path, &Error>,
    own_pid: u32,
) -> u32 {
    let log = &*supervisor.log;
    let end = |stop: SpecStop| {
        log.failure(id, &stop.message);
        stop.reported.process_exit_code() as u32
    };
    let task = match Id::try_from(id) {
        Ok(task) => task,
        Err(e) => {
            return end(SpecStop {
                message: format!("`{id}` is not a goetia id: {}", e.report()),
                reported: Reported::Refused,
            });
        }
    };
    let program_data = match program_data {
        Ok(program_data) => program_data,
        Err(e) => {
            return end(SpecStop {
                message: format!(
                    "no program was started: the run records cannot be held, since the machine's %ProgramData% \
                     could not be located: {}",
                    e.report()
                ),
                reported: Reported::shim_failure(e.raw_os_error()),
            });
        }
    };
    let runs = &Runs::of(program_data, &task);
    let pin = match runs.hold() {
        Ok(pin) => pin,
        Err(e) => {
            return end(SpecStop {
                message: format!(
                    "hold the run records {}: {}: no program was started",
                    runs.dir().display(),
                    Chain(&e)
                ),
                reported: Reported::shim_failure(e.raw_os_error()),
            });
        }
    };
    log.hold_default(id);
    let (spec, path) = match own_spec(&task, id, token, api) {
        Ok(read) => read,
        Err(stop) => return end(stop),
    };
    let instance = match own_instance(id, token, api, &path, own_pid, supervisor) {
        OwnInstanceOutcome::Listed(instance) => instance,
        OwnInstanceOutcome::Ended(stop) => return end(stop),
        OwnInstanceOutcome::Stopped(instance) => {
            record_run(
                log,
                id,
                runs,
                Writing::Given(pin),
                instance,
                file_time_now(),
                Some(RunOutcome::Stopped),
            );
            return STOPPED_EXIT;
        }
    };
    match runs.is_closed() {
        Ok(false) => {}
        Ok(true) => {
            return end(SpecStop {
                message: format!(
                    "the run records {} are being removed or handed to another account: no program was started",
                    runs.dir().display()
                ),
                reported: Reported::shim_failure(None),
            });
        }
        Err(e) => {
            return end(SpecStop {
                message: format!(
                    "look for the removal's marker of the run records {}: {}: no program was started",
                    runs.dir().display(),
                    Chain(&e)
                ),
                reported: Reported::shim_failure(e.raw_os_error()),
            });
        }
    }
    set_daemon_log(log, id, &spec);
    if let Some(logs) = spec.common().logs.as_deref() {
        write_marker(supervisor, id, logs, &instance);
    }

    let started = file_time_now();
    // Right before the spawn: a program started after the close event never receives it. A stop that arrives
    // after this look still starts the program (see the module's doc).
    if supervisor.bus.stop_accepted() {
        record_run(
            log,
            id,
            runs,
            Writing::Given(pin),
            Some(instance),
            started,
            Some(RunOutcome::Stopped),
        );
        return STOPPED_EXIT;
    }
    let outcome = run_pass(
        &spec,
        StopPlan::of(&spec),
        supervisor,
        id,
        || {},
        |end| {
            // Only for an accepted stop, never for the program's own exit: the record comes before the wait,
            // which Windows may end the shim in, and the pin stays held (the run is alive).
            if end == WaitEnd::Stop {
                record_run(
                    log,
                    id,
                    runs,
                    Writing::Lent(&pin),
                    Some(instance),
                    started,
                    Some(RunOutcome::Stopped),
                );
            }
        },
    );
    let (record, code) = ended_as(outcome);
    record_run(log, id, runs, Writing::Given(pin), Some(instance), started, record);
    code
}

/// Writes the run's record, under its own pin, when it has everything one needs; a write that fails is
/// logged. A pin that is given is released either way, as the function ends; one that is lent stays held and
/// nothing is pruned ([`Runs::write`]).
fn record_run(
    log: &Log,
    id: &str,
    runs: &Runs,
    pin: Writing<'_>,
    instance: Option<InstanceId>,
    started: u64,
    outcome: Option<RunOutcome>,
) {
    if let (Some(instance), Some(outcome)) = (instance, outcome) {
        match runs.write(
            &RunRecord {
                instance,
                started,
                outcome,
            },
            pin,
        ) {
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

/// The task's own spec, and its path. A task that is not goetia's, undecodable or a service's is refused;
/// the failures of [`registration`] and [`started_under`] are shim failures.
fn own_spec(task: &Id, id: &str, token: Option<Token>, api: &dyn TaskApi) -> Result<(DaemonSpec, TaskPath), SpecStop> {
    let refused = |message: String| SpecStop {
        message,
        reported: Reported::Refused,
    };
    let path = TaskPath::of(task);
    let registered = registration(&path, api)?;
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
    started_under(&path, id, token, &registered.xml)?;
    Ok((spec, path))
}

/// The task's registration as Task Scheduler holds it now. A task that is gone is a shim failure, as is a
/// read that did not complete, which carries the OS error it met.
fn registration(path: &TaskPath, api: &dyn TaskApi) -> Result<Registered, SpecStop> {
    match api.get(path) {
        Ok(Some(registered)) => Ok(registered),
        Ok(None) => Err(SpecStop {
            message: format!("the task {} is gone: no program was started", path.as_str()),
            reported: Reported::shim_failure(None),
        }),
        Err(e) => Err(SpecStop {
            message: format!(
                "read the task {}: {}: no program was started",
                path.as_str(),
                e.report()
            ),
            reported: read_failed(&e),
        }),
    }
}

/// The shim failure of a read of Task Scheduler that did not complete: the OS error it met, or a stand-in
/// for one that carries none.
fn read_failed(e: &Error) -> Reported {
    Reported::shim_failure(Some(e.raw_os_error().unwrap_or(ERROR_INVALID_DATA as i32)))
}

/// Whether the registration with the stored `xml` is the one the shim was started under (`token`). It is
/// not when its token differs, either has none, or its XML cannot be read: a shim failure.
fn started_under(path: &TaskPath, id: &str, token: Option<Token>, xml: &str) -> Result<(), SpecStop> {
    let not_started_under = |message: String| SpecStop {
        message,
        reported: Reported::shim_failure(None),
    };
    match (TaskXml::token_of(xml, id), token) {
        (Ok(Some(registered)), Some(token)) if registered == token => Ok(()),
        (Ok(Some(_)), _) => Err(not_started_under(format!(
            "the task {}'s registration is not the one this shim was started under (another install registered it \
             again, or the shim was started by hand): no program was started",
            path.as_str()
        ))),
        (Ok(None), _) => Err(not_started_under(format!(
            "the task {}'s registration carries no goetia ending for this task, so it is not the one this shim was \
             started under; `goetia daemon install {id}` registers it again: no program was started",
            path.as_str()
        ))),
        (Err(e), _) => Err(not_started_under(format!(
            "read the registration of the task {}: {}: no program was started",
            path.as_str(),
            e.report()
        ))),
    }
}

/// How the wait for the run's own instance ended.
enum OwnInstanceOutcome {
    /// Task Scheduler lists an instance with the shim's pid.
    Listed(InstanceId),
    /// A stop arrived while no such instance was listed, so no program is to start. Holds the
    /// instance when the read the stop woke found it; when it did not, that is said in the log.
    Stopped(Option<InstanceId>),
    /// The listing could not be read, or the registration was replaced or is gone: no program is to start.
    Ended(SpecStop),
}

/// The pace of the re-reads of Task Scheduler's listing while it does not hold the shim yet. Each
/// read decides; the pace only keeps the loop from spinning.
const LISTING_PACE: Duration = Duration::from_millis(100);

/// Waits for the run's own instance: the one Task Scheduler lists with the shim's pid. Task Scheduler
/// lists a run once it has started the action's process, which can be after that process reads the
/// listing, so a listing without the shim is read again, paced and without a cap: the shim is the
/// instance's process, and a task that was stopped or deleted meanwhile ends it. A failed read ends
/// the wait ([`OwnInstanceOutcome::Ended`]), and the registration is read once to word why; a stop ends
/// it too, after one more read. Each listing that misses the shim has the registration read again: one
/// that is gone, or no longer the shim's (an account change registered the task again), never lists it,
/// so it ends the wait ([`OwnInstanceOutcome::Ended`]).
fn own_instance(
    id: &str,
    token: Option<Token>,
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
            Ok(None) => {
                if let Err(stop) = registration(path, api).and_then(|now| started_under(path, id, token, &now.xml)) {
                    return OwnInstanceOutcome::Ended(stop);
                }
            }
            Err(e) => {
                // The read says only how the line is worded: the shim ends with the listing's error either way.
                let why = match registration(path, api).and_then(|now| started_under(path, id, token, &now.xml)) {
                    Ok(()) => "no program was started".to_string(),
                    Err(stop) => stop.message,
                };
                return OwnInstanceOutcome::Ended(SpecStop {
                    message: format!("list the task's instances: {}: {why}", e.report()),
                    reported: read_failed(&e),
                });
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
