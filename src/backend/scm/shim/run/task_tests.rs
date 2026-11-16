//! Task mode over the shim's `Fake` OS and the task boundary's fake: what `run_in` reads, runs and
//! records. The process-wide half of `run` (real COM, the registered console handler, the end of the
//! process) is not reached here; the real Task Scheduler starts the shim in `shim_integration`.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, FALSE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};

use super::*;
use crate::backend::scm::instance::{InstanceId, TaskPath};
use crate::backend::scm::manager::task::api::{Instance, Registered, TaskApi};
use crate::backend::scm::manager::task::fake::Fake as TaskFake;
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::backend::scm::shim::run::os::fake::{FakeGuard, FakeProcess, KillReply, Op, WaitReply, WaiterEnd};
use crate::backend::scm::shim::run::test_support::{
    assert_failures, assert_one_failure, fallback_refused_failure, not_found, supervisor, tree_refused_failure,
};
use crate::elevation::test_support::process_user_sid;
use crate::error::Error;
use crate::spec::{DaemonFields, DaemonSpec, ServiceSpec, TaskFields, TaskSpec};
use crate::testing::base_fields;

const ID: &str = "goetia-task-test";
/// The process id the task's listed instance carries: the shim's own.
const OWN_PID: u32 = 4242;
const STOPPED: u32 = 0xC000_013A;
const LOGS: &str = r"C:\goetia-task-test\task.log";
const PROGRAM: &str = "C:/Windows/System32/cmd.exe";

fn instance() -> InstanceId {
    InstanceId::parse("{6F9619FF-8B86-D011-B42D-00C04FC964FF}").unwrap()
}

/// A task that nothing runs: the `Fake` states what happens to it.
fn spec_logging_to(logs: Option<&str>) -> DaemonSpec {
    let base = base_fields(ID, vec![PROGRAM.to_string(), "/c".to_string(), "exit".to_string()]);
    TaskSpec::new(TaskFields {
        common: DaemonFields {
            logs: logs.map(PathBuf::from),
            ..base.common
        },
        schedule: None,
    })
    .map(DaemonSpec::from)
    .unwrap()
}

fn data_of(spec: &DaemonSpec) -> Registered {
    Registered {
        xml: String::new(),
        data: crate::blob::metadata_text(spec),
    }
}

fn listed() -> Vec<Instance> {
    vec![
        Instance {
            id: InstanceId::parse(&format!("{:032x}", 7)).unwrap(),
            pid: OWN_PID + 1,
        },
        Instance {
            id: instance(),
            pid: OWN_PID,
        },
    ]
}

/// One run of task mode: the shim's `Fake`, the task boundary's, and a real directory for the
/// records, removed with the fixture.
struct Run {
    fake: FakeGuard,
    supervisor: Arc<Supervisor>,
    api: TaskFake,
    runs: Runs,
    _dir: TempDir,
}

impl Run {
    /// A task whose own read answers `reply`, and whose directory of records is made for this
    /// process's own account.
    fn reading(reply: crate::error::Result<Option<Registered>>) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let id = Id::try_from(ID).unwrap();
        let runs = Runs::of(dir.path(), &id);
        runs.ensure(&process_user_sid()).unwrap();
        let fake = FakeGuard::new();
        let supervisor = supervisor(&fake);
        let api = TaskFake::new();
        api.on_get(reply);
        Run {
            fake,
            supervisor,
            api,
            runs,
            _dir: dir,
        }
    }

    /// A task whose own read returns `spec`.
    fn of(spec: &DaemonSpec) -> Run {
        Run::reading(Ok(Some(data_of(spec))))
    }

    /// The task's instance is listed, among another's.
    fn listed(self) -> Run {
        self.api.on_instances(Ok(listed()));
        self
    }

    /// The program is spawned, with a thread made to wait on it first.
    fn program(self, process: FakeProcess) -> Run {
        self.fake.will_make_thread();
        self.fake.will_spawn(process);
        self
    }

    fn run(&self) -> u32 {
        run_in(ID, &self.supervisor, &self.api, Some(&self.runs), OWN_PID)
    }

    /// What the run wrote, if anything.
    fn record(&self) -> Option<RunRecord> {
        self.runs.read(&instance()).unwrap()
    }

    /// The program's thread ended, and the failures the run logged, taken.
    fn finish(&self) -> Vec<String> {
        assert_eq!(self.fake.waiter_end(0), WaiterEnd::Returned);
        self.api.finish();
        self.fake.take_failures()
    }
}

fn exits(code: i32) -> FakeProcess {
    FakeProcess::new().exited(code).send_kill_tree(KillReply::Ends)
}

/// A program a stop ends: it runs until the tree is killed.
fn runs_until_stopped() -> FakeProcess {
    FakeProcess::running()
}

fn recorded(outcome: RunOutcome) -> Option<RunRecord> {
    Some(RunRecord {
        instance: instance(),
        started: 0,
        outcome,
    })
}

/// The record without its start time, which the run takes from the clock and only orders runs by.
fn without_start(record: Option<RunRecord>) -> Option<RunRecord> {
    record.map(|record| RunRecord { started: 0, ..record })
}

// Arguments ===========================================================================================================

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

#[skuld::test]
fn task_mode_is_requested_by_the_trailing_arguments_only() {
    assert_eq!(requested(&args(&["--goetia-task", "frpc"])), Some("frpc"));
    assert_eq!(
        requested(&args(&["--host-flag", "value", "--goetia-task", "frpc"])),
        Some("frpc")
    );
}

#[skuld::test]
fn arguments_not_ending_in_the_task_flag_are_service_mode() {
    assert_eq!(requested(&args(&[])), None);
    assert_eq!(requested(&args(&["frpc"])), None);
    assert_eq!(requested(&args(&["--goetia-task"])), None);
    assert_eq!(requested(&args(&["--goetia-task", "frpc", "extra"])), None);
    assert_eq!(requested(&args(&["--other", "frpc"])), None);
    let not_utf8 = OsString::from_wide(&[0xD800]);
    assert_eq!(requested(&[OsString::from("--goetia-task"), not_utf8]), None);
}

// What the task says ==================================================================================================

#[skuld::test]
fn a_service_blob_is_refused_by_task_mode() {
    let service = ServiceSpec::new(base_fields(ID, vec![PROGRAM.to_string()])).unwrap();
    let run = Run::of(&DaemonSpec::from(service));

    let code = run.run();

    assert_eq!(code, Reported::Refused.process_exit_code() as u32);
    assert_one_failure(
        &run.fake.take_failures(),
        ID,
        "this is a service's blob; task mode runs only tasks",
    );
    assert_eq!(run.fake.calls(Op::Spawn), 0, "a service's program was run by task mode");
    assert_eq!(run.record(), None);
    run.api.finish();
}

#[skuld::test]
fn a_task_that_is_gone_or_not_goetias_is_refused_and_nothing_runs() {
    let cases = [
        (
            Ok(None),
            format!(r"the task \Goetia\{ID} is gone: no program was started"),
        ),
        (
            Ok(Some(Registered {
                xml: String::new(),
                data: "an administrator's note".to_string(),
            })),
            "the task's Data holds no Goetia marker: this task was not created by `goetia daemon install`".to_string(),
        ),
    ];
    for (reply, message) in cases {
        let run = Run::reading(reply);

        let code = run.run();

        assert_eq!(code, Reported::Refused.process_exit_code() as u32);
        assert_one_failure(&run.fake.take_failures(), ID, &message);
        assert_eq!(run.fake.calls(Op::Spawn), 0);
        run.api.finish();
    }
}

#[skuld::test]
fn a_task_that_cannot_be_read_is_a_logged_shim_failure_with_the_os_error_and_nothing_runs() {
    let run = Run::reading(Err(Error::os(
        "read the task",
        io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32),
    )));

    let code = run.run();

    assert_eq!(code, ERROR_FILE_NOT_FOUND);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!(
                "goetia-shim[{ID}]: read the task {}: ",
                r"\Goetia\goetia-task-test"
            )),
        "{failures:?}"
    );
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    run.api.finish();
}

// How the run ends ====================================================================================================

#[skuld::test]
fn a_clean_exit_records_the_code() {
    let run = Run::of(&spec_logging_to(None)).listed().program(exits(0));

    let code = run.run();

    assert_eq!(code, 0);
    assert_eq!(without_start(run.record()), recorded(RunOutcome::Exited(0)));
    assert_eq!(run.finish(), Vec::<String>::new());
}

#[skuld::test]
fn a_failing_exit_records_it_unchanged() {
    // An NTSTATUS, as a crashed program leaves, keeps every bit.
    for code in [7, 0xC000_0005_u32 as i32] {
        let run = Run::of(&spec_logging_to(None)).listed().program(exits(code));

        let returned = run.run();

        assert_eq!(returned, code as u32);
        assert_eq!(without_start(run.record()), recorded(RunOutcome::Exited(code as u32)));
        assert_eq!(run.finish(), Vec::<String>::new());
    }
}

/// The program's own exit code is what the run ends with and records: a program that ends with the
/// code of a stop, or of the shim's incomplete stop, is still exited, not stopped.
#[skuld::test]
fn an_exit_that_looks_like_a_stop_is_an_exit() {
    for code in [STOPPED as i32, 1003] {
        let run = Run::of(&spec_logging_to(None)).listed().program(exits(code));

        let returned = run.run();

        assert_eq!(returned, code as u32);
        assert_eq!(without_start(run.record()), recorded(RunOutcome::Exited(code as u32)));
        assert_eq!(run.finish(), Vec::<String>::new());
    }
}

#[skuld::test]
fn a_program_that_cannot_be_spawned_is_a_logged_shim_failure_and_is_not_recorded() {
    let run = Run::of(&spec_logging_to(None)).listed();
    run.fake.will_make_thread();
    run.fake.will_refuse_spawn(not_found());

    let code = run.run();

    assert_eq!(code, ERROR_FILE_NOT_FOUND);
    assert_eq!(run.record(), None);
    let failures = run.finish();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with(&format!("goetia-shim[{ID}]: spawn [")),
        "{failures:?}"
    );
}

// A stop ==============================================================================================================

#[skuld::test]
fn an_accepted_stop_records_stopped_whatever_the_exit_code() {
    for exit in [Some(0), Some(7), None] {
        let run = Run::of(&spec_logging_to(None)).listed();
        let process = match exit {
            Some(code) => FakeProcess::new()
                .wait(WaitReply::Exits(code))
                .send_kill_tree(KillReply::Ends),
            None => runs_until_stopped(),
        };
        let run = run.program(process);
        assert!(run.supervisor.bus.request_stop());

        let code = run.run();

        assert_eq!(code, STOPPED, "the program's exit {exit:?}");
        assert_eq!(without_start(run.record()), recorded(RunOutcome::Stopped));
        assert_eq!(run.finish(), Vec::<String>::new());
    }
}

#[skuld::test]
fn a_stop_ends_with_0xc000013a() {
    let run = Run::of(&spec_logging_to(None)).listed().program(runs_until_stopped());
    assert!(run.supervisor.bus.request_stop());

    assert_eq!(run.run(), 0xC000_013A);
    run.finish();
}

/// The handler is registered before the task is read, so a stop that arrives before the program is
/// spawned is held by the bus until the run reaches it.
#[skuld::test]
fn a_stop_before_the_spawn_is_not_lost() {
    let run = Run::of(&spec_logging_to(None)).listed().program(runs_until_stopped());
    assert!(run.supervisor.bus.request_stop());

    let code = run.run();

    assert_eq!(code, STOPPED);
    assert_eq!(without_start(run.record()), recorded(RunOutcome::Stopped));
    assert_eq!(run.finish(), Vec::<String>::new());
}

/// Which read of Task Scheduler a [`StopsDuring`] asks for the stop in.
#[derive(Clone, Copy, Debug)]
enum Read {
    Get,
    Instances,
}

/// A task boundary that asks for a stop while the read `at` is under way, as a close event on the
/// handler's thread does. Task mode makes no other call.
struct StopsDuring {
    inner: TaskFake,
    bus: Arc<StopBus>,
    at: Read,
}

impl TaskApi for StopsDuring {
    fn get(&self, path: &TaskPath) -> crate::error::Result<Option<Registered>> {
        if matches!(self.at, Read::Get) {
            assert!(self.bus.request_stop());
        }
        self.inner.get(path)
    }

    fn instances(&self, path: &TaskPath) -> crate::error::Result<Vec<Instance>> {
        if matches!(self.at, Read::Instances) {
            assert!(self.bus.request_stop());
        }
        self.inner.instances(path)
    }

    fn names(&self) -> crate::error::Result<Vec<String>> {
        unreachable!("task mode reads its own task only")
    }

    fn register(
        &self,
        _: &TaskPath,
        _: &str,
        _: &crate::backend::scm::instance::TaskAccount,
        _: Option<&str>,
        _: bool,
    ) -> crate::error::Result<crate::backend::scm::manager::task::api::Registration> {
        unreachable!("task mode reads its own task only")
    }

    fn set_enabled(&self, _: &TaskPath, _: bool) -> crate::error::Result<()> {
        unreachable!("task mode reads its own task only")
    }

    fn delete(&self, _: &TaskPath) -> crate::error::Result<bool> {
        unreachable!("task mode reads its own task only")
    }

    fn info(&self, _: &TaskPath) -> crate::error::Result<crate::backend::scm::manager::task::api::TaskInfo> {
        unreachable!("task mode reads its own task only")
    }

    fn run(&self, _: &TaskPath) -> crate::error::Result<crate::backend::scm::manager::task::api::Started> {
        unreachable!("task mode reads its own task only")
    }

    fn stop(&self, _: &TaskPath, _: &InstanceId) -> crate::error::Result<()> {
        unreachable!("task mode reads its own task only")
    }
}

#[skuld::test]
fn a_stop_during_the_com_reads_is_not_lost() {
    for at in [Read::Get, Read::Instances] {
        let run = Run::of(&spec_logging_to(None)).listed().program(runs_until_stopped());
        let api = StopsDuring {
            inner: run.api.clone(),
            bus: Arc::clone(&run.supervisor.bus),
            at,
        };

        let code = run_in(ID, &run.supervisor, &api, Some(&run.runs), OWN_PID);

        assert_eq!(code, STOPPED, "a stop during {at:?}");
        assert_eq!(without_start(run.record()), recorded(RunOutcome::Stopped), "{at:?}");
        assert_eq!(run.finish(), Vec::<String>::new());
    }
}

#[skuld::test]
fn an_incomplete_stop_is_recorded_as_incomplete_and_returns_1003() {
    let process = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let run = Run::of(&spec_logging_to(None)).listed().program(process.clone());
    assert!(run.supervisor.bus.request_stop());

    let code = run.run();

    assert_eq!(code, 1003);
    assert_eq!(without_start(run.record()), recorded(RunOutcome::IncompleteStop));
    // Both kills were refused, so the program outlives the run: the test ends it.
    process.end(1);
    assert_eq!(
        run.finish(),
        vec![
            format!("goetia-shim[{ID}]: {}", tree_refused_failure()),
            format!("goetia-shim[{ID}]: {}", fallback_refused_failure()),
        ]
    );
}

#[skuld::test]
fn the_close_handler_only_requests_a_stop() {
    let run = Run::of(&spec_logging_to(None)).listed().program(runs_until_stopped());
    let bus = Arc::clone(&run.supervisor.bus);
    // The handler's thread is held for the life of the process, as the system's is: it is never joined.
    std::thread::spawn(move || on_event(&bus, CTRL_CLOSE_EVENT));

    // Only a stop that reached the bus ends a program that runs until stopped.
    let code = run.run();

    assert_eq!(code, STOPPED);
    assert_eq!(without_start(run.record()), recorded(RunOutcome::Stopped));
    assert_eq!(run.finish(), Vec::<String>::new());
}

#[skuld::test]
fn other_control_events_return_false() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);

    for event in [CTRL_C_EVENT, CTRL_BREAK_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT] {
        assert_eq!(on_event(&supervisor.bus, event), FALSE, "event {event}");
    }

    // None of them asked for a stop: the bus still accepts the failure a run decides.
    assert_eq!(
        supervisor.bus.settle(Reported::DaemonExit(7)),
        Reported::DaemonExit(7),
        "an event other than the close event reached the bus as a stop"
    );
}

// Output ==============================================================================================================

#[skuld::test]
fn the_marker_line_is_the_first_line_of_the_log() {
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(0));

    assert_eq!(run.run(), 0);

    // Opened for the marker first, then for the program's own output.
    assert_eq!(run.fake.calls(Op::OpenLog), 2);
    assert_eq!(
        run.fake.opened_log_text(0),
        "goetia-run 6f9619ff8b86d011b42d00c04fc964ff\n",
        "the first thing in the log is the run's marker, named by the instance's 32 hex digits"
    );
    assert_eq!(run.fake.opened_log_path(0), Path::new(LOGS));
    assert_eq!(run.finish(), Vec::<String>::new());
}

#[skuld::test]
fn without_logs_the_programs_output_goes_to_nul() {
    let run = Run::of(&spec_logging_to(None)).listed().program(exits(0));

    assert_eq!(run.run(), 0);

    // No log is opened, not even a default one, and nothing is said of its absence: the output of a
    // task with no `logs` is discarded by design.
    assert_eq!(run.fake.calls(Op::OpenLog), 0);
    assert_eq!(run.finish(), Vec::<String>::new());
}

/// With no instance there is no marker to write, since no run can be named, and the program still
/// runs.
#[skuld::test]
fn a_run_with_no_instance_writes_no_marker() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    run.api.on_instances(Ok(vec![]));
    let run = run.program(exits(0));

    assert_eq!(run.run(), 0);

    assert_eq!(run.fake.calls(Op::OpenLog), 1, "only the program's own output");
    run.finish();
}

/// The marker's open and the program's own open are of one file: when it cannot be opened that is
/// one fault, said once (by the program's open, which names that its output is not captured).
#[skuld::test]
fn a_log_that_cannot_be_opened_is_reported_once() {
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(0));
    run.fake.will_fail_open_log(Path::new(LOGS));
    run.fake.will_fail_open_log(Path::new(LOGS));

    assert_eq!(run.run(), 0);

    let failures = run.finish();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with(&format!("goetia-shim[{ID}]: open log file {LOGS}: ")),
        "{failures:?}"
    );
}

/// A record says when the run began, as a `FILETIME` of the machine's clock.
#[skuld::test]
fn a_record_carries_the_time_the_run_began() {
    let run = Run::of(&spec_logging_to(None)).listed().program(exits(0));

    assert_eq!(run.run(), 0);

    // 2020-01-01 as a `FILETIME`: any clock that is right is past it.
    assert!(run.record().unwrap().started > 132_223_104_000_000_000);
    run.finish();
}

// What cannot be recorded =============================================================================================

#[skuld::test]
fn a_missing_instance_runs_the_program_and_writes_no_record() {
    let cases: [(crate::error::Result<Vec<Instance>>, String); 2] = [
        (
            Ok(vec![Instance {
                id: instance(),
                pid: OWN_PID + 1,
            }]),
            format!(
                "no listed instance of this task has process id {OWN_PID}: the run is not recorded; the program is run anyway"
            ),
        ),
        (
            Err(Error::os("list the task's instances", io::Error::other("refused"))),
            "list the task's instances: refused".to_string(),
        ),
    ];
    for (reply, message) in cases {
        let run = Run::of(&spec_logging_to(None));
        run.api.on_instances(reply);
        let run = run.program(exits(5));
        let pin = run.runs.pin().unwrap();

        let code = run.run();

        assert_eq!(code, 5, "the program's code is returned all the same");
        assert_eq!(
            run.runs.appeared(&pin).unwrap(),
            vec![],
            "a record was written for no run"
        );
        let failures = run.finish();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].starts_with(&format!("goetia-shim[{ID}]: ")) && failures[0].contains(&message),
            "{failures:?}"
        );
    }
}

#[skuld::test]
fn a_failed_record_write_still_returns_the_programs_code() {
    let dir = tempfile::tempdir().unwrap();
    // No `ensure`: the directory is not there, so the write fails.
    let unmade = Runs::of(dir.path(), &Id::try_from(ID).unwrap());
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(5));

    let code = run_in(ID, &run.supervisor, &run.api, Some(&unmade), OWN_PID);

    assert_eq!(code, 5);
    assert_eq!(unmade.read(&instance()).unwrap(), None);
    // The log is the program's own now that the task is read, and the failure is said there.
    let calls = run.fake.take_channel_calls();
    let said: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            crate::backend::scm::shim::run::os::fake::ChannelCall::Append { path, line, .. } => {
                Some((path.as_path(), line.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(said.len(), 1, "{calls:?}");
    assert_eq!(said[0].0, Path::new(LOGS));
    assert!(
        said[0]
            .1
            .starts_with(&format!("goetia-shim[{ID}]: write the run record: ")),
        "{said:?}"
    );
    assert_eq!(run.fake.waiter_end(0), WaiterEnd::Returned);
    run.api.finish();
}

/// A run with no `%ProgramData%` to keep records in is run, and not recorded.
#[skuld::test]
fn a_run_with_nowhere_to_record_still_runs() {
    let run = Run::of(&spec_logging_to(None)).listed().program(exits(3));

    let code = run_in(ID, &run.supervisor, &run.api, None, OWN_PID);

    assert_eq!(code, 3);
    assert_eq!(run.record(), None);
    assert_eq!(run.finish(), Vec::<String>::new());
}

// What it never does ==================================================================================================

/// Failures before the program start are logged once each, and the two kinds of refusal a bad id and
/// a bad task name are not read from Task Scheduler at all.
#[skuld::test]
fn an_id_that_is_not_an_id_reads_nothing_and_runs_nothing() {
    let fake = FakeGuard::new();
    let api = TaskFake::new();

    let code = run_in("not an id!", &supervisor(&fake), &api, None, OWN_PID);

    assert_eq!(code, Reported::Refused.process_exit_code() as u32);
    let failures = fake.take_failures();
    assert_failures(
        &failures,
        "not an id!",
        &[format!("`not an id!` is not a goetia id: {}", id_error("not an id!"))],
    );
    assert_eq!(fake.calls(Op::Spawn), 0);
    api.finish();
}

fn id_error(id: &str) -> String {
    Id::try_from(id).unwrap_err().to_string()
}
