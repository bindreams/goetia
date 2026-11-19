//! Task mode over the shim's `Fake` OS and the task boundary's fake: what `run_in` reads, runs and
//! records. The process-wide half of `run` (real COM, the registered console handler, the end of the
//! process) is not reached here; the real Task Scheduler starts the shim in `shim_integration`.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, FALSE, TRUE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};

use super::*;
use crate::backend::scm::instance::{InstanceId, TaskPath};
use crate::backend::scm::manager::task::api::{Instance, Registered, TaskApi};
use crate::backend::scm::manager::task::fake::Fake as TaskFake;
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::backend::scm::shim::run::os::LogKind;
use crate::backend::scm::shim::run::os::fake::{
    FAKE_LOGS, FakeGuard, FakeProcess, KillReply, Op, WaitReply, WaiterEnd,
};
use crate::backend::scm::shim::run::test_support::{
    assert_failures, assert_one_failure, fallback_refused_failure, not_found, supervisor, tree_refused_failure,
};
use crate::backend::scm::task_xml::Token;
use crate::backend::scm::test_support::{TASK_TOKEN, registered_task, task_token};
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

/// What Task Scheduler holds for `spec`: a task's registration carries [`TASK_TOKEN`]; a service's blob
/// is in no registration of a task, so it has no XML.
fn data_of(spec: &DaemonSpec) -> Registered {
    match spec {
        DaemonSpec::Task(_) => registered_task(spec, false),
        DaemonSpec::Service(_) => Registered {
            xml: String::new(),
            data: crate::blob::metadata_text(spec),
            enabled: Some(true),
        },
    }
}

/// A token no registration here carries.
const OTHER_TOKEN: &str = "00112233445566778899aabbccddeeff";

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
        // Made plainly: naming Administrators as an owner needs an elevated token.
        std::fs::create_dir_all(dir.path().join("Goetia").join("runs").join(ID)).unwrap();
        runs.ensure(&[process_user_sid()]).unwrap();
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
        run_in(
            ID,
            &task_token(),
            &self.supervisor,
            &self.api,
            Some(&self.runs),
            OWN_PID,
        )
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
    assert_eq!(
        requested(&args(&["--goetia-task", "frpc", TASK_TOKEN])),
        Some(("frpc", task_token()))
    );
    assert_eq!(
        requested(&args(&["--host-flag", "value", "--goetia-task", "frpc", TASK_TOKEN])),
        Some(("frpc", task_token()))
    );
}

#[skuld::test]
fn arguments_not_ending_in_the_task_flag_the_id_and_the_token_are_service_mode() {
    assert_eq!(requested(&args(&[])), None);
    assert_eq!(requested(&args(&["frpc"])), None);
    assert_eq!(requested(&args(&["--goetia-task"])), None);
    assert_eq!(requested(&args(&["--goetia-task", "frpc"])), None);
    assert_eq!(requested(&args(&["--goetia-task", "frpc", TASK_TOKEN, "extra"])), None);
    // A last argument that is no token is no ending of goetia's.
    assert_eq!(requested(&args(&["--goetia-task", "frpc", "0123"])), None);
    assert_eq!(requested(&args(&["--goetia-task", "frpc", &"g".repeat(32)])), None);
    assert_eq!(requested(&args(&["--other", "frpc", TASK_TOKEN])), None);
    let not_utf8 = OsString::from_wide(&[0xD800]);
    assert_eq!(
        requested(&[
            OsString::from("--goetia-task"),
            OsString::from("frpc"),
            not_utf8.clone()
        ]),
        None
    );
    assert_eq!(
        requested(&[OsString::from("--goetia-task"), not_utf8, OsString::from(TASK_TOKEN)]),
        None
    );
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
                enabled: Some(true),
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

/// A shim that another registration has outlived: the task it reads is not the one it was started under,
/// so it starts no program, and states no listing of its own to wait for.
#[skuld::test]
fn a_registration_with_another_token_starts_no_program() {
    let run = Run::of(&spec_logging_to(None));

    let other = Token::parse(OTHER_TOKEN).unwrap();
    let code = run_in(ID, &other, &run.supervisor, &run.api, Some(&run.runs), OWN_PID);

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_one_failure(
        &run.fake.take_failures(),
        ID,
        &format!(
            "the task \\Goetia\\{ID}'s registration is not the one this shim was started under (another install \
             registered it again, or the shim was started by hand): no program was started"
        ),
    );
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    assert_eq!(run.record(), None);
    run.api.finish();
}

/// A registration whose arguments end with no goetia ending for this task (a hand edit: no ending, the ending
/// of another id, or a malformed token) is not the one the shim was started under.
#[skuld::test]
fn a_registration_with_no_ending_for_this_task_starts_no_program() {
    let stored = registered_task(&spec_logging_to(None), false);
    for edited in [
        stored
            .xml
            .replace(&format!(" {TASK_TOKEN}</Arguments>"), " 0123</Arguments>"),
        stored
            .xml
            .replace(&format!(" {ID} {TASK_TOKEN}</Arguments>"), "</Arguments>"),
        stored.xml.replace(
            &format!(" {ID} {TASK_TOKEN}</Arguments>"),
            &format!(" other {TASK_TOKEN}</Arguments>"),
        ),
    ] {
        let run = Run::reading(Ok(Some(Registered {
            xml: edited,
            ..stored.clone()
        })));

        let code = run.run();

        assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
        assert_one_failure(
            &run.fake.take_failures(),
            ID,
            &format!(
                "the task \\Goetia\\{ID}'s registration carries no goetia ending for this task, so it is not the \
                 one this shim was started under; `goetia daemon install {ID}` registers it again: no program \
                 was started"
            ),
        );
        assert_eq!(run.fake.calls(Op::Spawn), 0);
        run.api.finish();
    }
}

/// XML that cannot be read says nothing of the registration, so no program is started under it.
#[skuld::test]
fn a_registration_whose_xml_cannot_be_read_starts_no_program() {
    let stored = registered_task(&spec_logging_to(None), false);
    let run = Run::reading(Ok(Some(Registered {
        xml: "<Task><Actions>".to_owned(),
        ..stored
    })));

    let code = run.run();

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!(
                "goetia-shim[{ID}]: read the registration of the task \\Goetia\\{ID}: "
            )),
        "{failures:?}"
    );
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    run.api.finish();
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

        let code = run_in(ID, &task_token(), &run.supervisor, &api, Some(&run.runs), OWN_PID);

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

/// The program shares the shim's console, so a CTRL_C or CTRL_BREAK that the program raises (or that
/// reaches the console's group) arrives at the shim too. The program gets its own copy and decides for
/// itself; the shim must not end with it, or the run would end with no record.
#[skuld::test]
fn ctrl_c_and_ctrl_break_are_swallowed_and_ask_for_nothing() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);

    for event in [CTRL_C_EVENT, CTRL_BREAK_EVENT] {
        assert_eq!(on_event(&supervisor.bus, event), TRUE, "event {event}");
    }

    assert_eq!(
        supervisor.bus.settle(Reported::DaemonExit(7)),
        Reported::DaemonExit(7),
        "a CTRL_C or CTRL_BREAK reached the bus as a stop"
    );
}

#[skuld::test]
fn logoff_and_shutdown_are_left_to_the_default_handler() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);

    for event in [CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT] {
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
    assert_eq!(
        run.fake.opened_log_kind(0),
        LogKind::Own,
        "the user's own `logs` has its folders made"
    );
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

/// The marker goes where the program's output goes: a `logs:` equal to the task's default log as written is
/// the default log, which the shim opens as an install made it (through the handle it holds), not as the user's
/// own.
#[skuld::test]
fn the_marker_goes_to_the_default_log_when_logs_is_the_default_log() {
    let default = format!(r"{FAKE_LOGS}\{ID}.log");
    let run = Run::of(&spec_logging_to(Some(&default))).listed().program(exits(0));

    assert_eq!(run.run(), 0);

    assert_eq!(run.fake.calls(Op::OpenLog), 2);
    for n in 0..2 {
        assert_eq!(run.fake.opened_log_path(n), Path::new(&default), "open {n}");
        assert_eq!(run.fake.opened_log_kind(n), LogKind::Default, "open {n}");
    }
    assert_eq!(
        run.fake.opened_log_text(0),
        "goetia-run 6f9619ff8b86d011b42d00c04fc964ff\n"
    );
    assert_eq!(run.finish(), Vec::<String>::new());
}

/// A marker whose log cannot be opened is said, so that a reader who cannot find the run's output in the log
/// knows why, even when the program's own open then succeeds. The program's open of the same file is told for its
/// own cost (`a_log_that_cannot_be_opened_is_reported_for_the_marker_and_for_the_output`).
#[skuld::test]
fn a_marker_whose_log_cannot_be_opened_is_reported() {
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(0));
    run.fake.will_fail_open_log(Path::new(LOGS));

    assert_eq!(run.run(), 0);

    let failures = run.finish();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with(&format!("goetia-shim[{ID}]: open log file {LOGS}: ")),
        "{failures:?}"
    );
    assert!(
        failures[0].contains("the run marker could not be written"),
        "{failures:?}"
    );
    assert!(!failures[0].contains("will not be captured"), "{failures:?}");
}

/// With no instance there is no marker to write, since no run can be named, and the program still
/// runs. A listing that cannot be read is the one way a run has none.
#[skuld::test]
fn a_run_with_no_instance_writes_no_marker() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    run.api
        .on_instances(Err(Error::os("list the task's instances", io::Error::other("refused"))));
    let run = run.program(exits(0));

    assert_eq!(run.run(), 0);

    assert_eq!(run.fake.calls(Op::OpenLog), 1, "only the program's own output");
    run.finish();
}

/// The marker's open and the program's own open are of one file, and each fails and is told for what it costs:
/// the marker's line first, then the output's. A failed marker does not silence the output's failure.
#[skuld::test]
fn a_log_that_cannot_be_opened_is_reported_for_the_marker_and_for_the_output() {
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(0));
    run.fake.will_fail_open_log(Path::new(LOGS));
    run.fake.will_fail_open_log(Path::new(LOGS));

    assert_eq!(run.run(), 0);

    let failures = run.finish();
    let [marker, output] = failures.as_slice() else {
        panic!("the marker's line and the output's: {failures:?}");
    };
    for line in [marker, output] {
        assert!(
            line.starts_with(&format!("goetia-shim[{ID}]: open log file {LOGS}: ")),
            "{line}"
        );
    }
    assert!(marker.contains("the run marker could not be written"), "{marker}");
    assert!(output.contains("this daemon's output will not be captured"), "{output}");
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
fn an_unreadable_listing_runs_the_program_and_writes_no_record() {
    let run = Run::of(&spec_logging_to(None));
    run.api
        .on_instances(Err(Error::os("list the task's instances", io::Error::other("refused"))));
    let run = run.program(exits(5));
    let pin = run.runs.pin().unwrap();

    let code = run.run();

    assert_eq!(code, 5, "the program's code is returned all the same");
    assert_eq!(pin.appeared().unwrap(), vec![], "a record was written for no run");
    let failures = run.finish();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with(&format!("goetia-shim[{ID}]: "))
            && failures[0].contains("list the task's instances: refused")
            && failures[0].contains("the run is not recorded; the program is run anyway"),
        "{failures:?}"
    );
}

// The wait for the run's own instance =================================================================================

/// Task Scheduler lists a run once it has started it, which can be after the shim reads the listing:
/// the shim reads again, and the program starts when its own instance is listed.
#[skuld::test]
fn a_listing_that_misses_the_shim_is_read_again_and_the_run_is_recorded() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    run.api.on_instances(Ok(vec![])).on_instances(Ok(listed()));
    let run = run.program(exits(0));

    assert_eq!(run.run(), 0);

    assert_eq!(without_start(run.record()), recorded(RunOutcome::Exited(0)));
    assert_eq!(
        run.fake.opened_log_text(0),
        "goetia-run 6f9619ff8b86d011b42d00c04fc964ff\n",
        "the marker names the instance the second read found"
    );
    assert_eq!(run.finish(), Vec::<String>::new());
}

/// A listing that holds other processes' instances only is not the shim's: it is read again.
#[skuld::test]
fn a_listing_of_other_processes_is_read_again() {
    let others = vec![Instance {
        id: InstanceId::parse(&format!("{:032x}", 7)).unwrap(),
        pid: OWN_PID + 1,
    }];
    let run = Run::of(&spec_logging_to(None));
    run.api.on_instances(Ok(others)).on_instances(Ok(listed()));
    let run = run.program(exits(3));

    assert_eq!(run.run(), 3);

    assert_eq!(without_start(run.record()), recorded(RunOutcome::Exited(3)));
    assert_eq!(run.finish(), Vec::<String>::new());
}

/// A stop that arrives while the instance is not listed starts no program. The run is recorded stopped
/// when the instance is found by the read the stop wakes.
#[skuld::test]
fn a_stop_while_the_instance_is_not_listed_starts_nothing_and_records_the_run_stopped() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    run.api.on_instances(Ok(vec![])).on_instances(Ok(listed()));
    let api = StopsDuring {
        inner: run.api.clone(),
        bus: Arc::clone(&run.supervisor.bus),
        at: Read::Instances,
    };

    let code = run_in(ID, &task_token(), &run.supervisor, &api, Some(&run.runs), OWN_PID);

    assert_eq!(code, STOPPED);
    assert_eq!(without_start(run.record()), recorded(RunOutcome::Stopped));
    assert_eq!(run.fake.calls(Op::Spawn), 0, "no program was started");
    assert_eq!(
        run.fake.calls(Op::OpenLog),
        0,
        "no marker was written for a run that began nothing"
    );
    assert_eq!(run.fake.take_failures(), Vec::<String>::new());
    run.api.finish();
}

/// When the read the stop wakes still does not hold the instance, the run has no name to record under:
/// that is logged, and the process still ends with the stop's code.
#[skuld::test]
fn a_stop_while_the_instance_is_not_listed_and_never_found_is_logged_unrecorded() {
    let run = Run::of(&spec_logging_to(None));
    run.api.on_instances(Ok(vec![])).on_instances(Ok(vec![]));
    let api = StopsDuring {
        inner: run.api.clone(),
        bus: Arc::clone(&run.supervisor.bus),
        at: Read::Instances,
    };
    let pin = run.runs.pin().unwrap();

    let code = run_in(ID, &task_token(), &run.supervisor, &api, Some(&run.runs), OWN_PID);

    assert_eq!(code, STOPPED);
    assert_eq!(pin.appeared().unwrap(), vec![]);
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    let failures = run.fake.take_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains(&format!("no listed instance of this task has process id {OWN_PID}"))
            && failures[0].contains("stopped before the program started")
            && failures[0].contains("the run is not recorded"),
        "{failures:?}"
    );
    run.api.finish();
}

#[skuld::test]
fn a_failed_record_write_still_returns_the_programs_code() {
    let dir = tempfile::tempdir().unwrap();
    // No `ensure`: the directory is not there, so the write fails.
    let unmade = Runs::of(dir.path(), &Id::try_from(ID).unwrap());
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(5));

    let code = run_in(ID, &task_token(), &run.supervisor, &run.api, Some(&unmade), OWN_PID);

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

/// A prune that cannot remove a record (here a directory where the record should be) is a warning in
/// the log, and the run still ends with the program's code and its own record.
#[skuld::test]
fn a_prune_problem_is_logged_and_does_not_change_the_runs_end() {
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(5));
    let stuck = format!("{:032x}.json", 9);
    let stuck_path = run._dir.path().join("Goetia").join("runs").join(ID).join(&stuck);
    crate::backend::scm::test_support::directory_with_a_file(&stuck_path);

    let code = run.run();

    assert_eq!(code, 5);
    assert_eq!(run.fake.waiter_end(0), WaiterEnd::Returned);
    run.api.finish();
    assert!(run.record().is_some(), "the run's own record is written");
    let said: Vec<String> = run
        .fake
        .take_channel_calls()
        .into_iter()
        .filter_map(|call| match call {
            crate::backend::scm::shim::run::os::fake::ChannelCall::Append { line, .. } => Some(line),
            _ => None,
        })
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("prune the run records") && said[0].contains(&stuck),
        "{said:?}"
    );
}

/// A run with no `%ProgramData%` to keep records in is run, and not recorded.
#[skuld::test]
fn a_run_with_nowhere_to_record_still_runs() {
    let run = Run::of(&spec_logging_to(None)).listed().program(exits(3));

    let code = run_in(ID, &task_token(), &run.supervisor, &run.api, None, OWN_PID);

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

    let code = run_in("not an id!", &task_token(), &supervisor(&fake), &api, None, OWN_PID);

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
