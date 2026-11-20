//! Task mode over the shim's `Fake` OS and the task boundary's fake: what `run_in` reads, runs and
//! records. The process-wide half of `run` (real COM, the registered console handler, the end of the
//! process) is not reached here; the real Task Scheduler starts the shim in `shim_integration`.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, FALSE, TRUE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};

use super::*;
use crate::backend::scm::instance::{InstanceId, TaskPath};
use crate::backend::scm::manager::task::api::{Instance, Registered, TaskApi};
use crate::backend::scm::manager::task::fake::Fake as TaskFake;
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::backend::scm::shim::acl::Descriptor;
use crate::backend::scm::shim::run::os::fake::{
    ChannelCall, FAKE_LOGS, FakeGuard, FakeProcess, KillReply, Op, WaitReply, WaiterEnd,
};
use crate::backend::scm::shim::run::os::{LogKind, Severity};
use crate::backend::scm::shim::run::test_support::{
    assert_failures, assert_one_failure, fallback_refused_failure, not_found, supervisor, tree_refused_failure,
};
use crate::backend::scm::task_xml::Token;
use crate::backend::scm::test_support::{
    A_STRANGER, DirWatch, NameChange, TASK_TOKEN, delete_pending, directory_with_a_file, make_mount_point,
    registered_task, registered_task_with, set_dacl, task_token,
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
    /// A task that states no reply, so that any call on it fails the test, and whose directory of
    /// records is made for this process's own account.
    fn unread() -> Run {
        let dir = tempfile::tempdir().unwrap();
        let id = Id::try_from(ID).unwrap();
        let runs = Runs::of(dir.path(), &id);
        // Made plainly: naming Administrators as an owner needs an elevated token.
        std::fs::create_dir_all(dir.path().join("Goetia").join("runs").join(ID)).unwrap();
        runs.ensure(&[process_user_sid()]).unwrap();
        let fake = FakeGuard::new();
        let supervisor = Arc::new(Supervisor::new(fake.os(), fake.task_log()));
        Run {
            fake,
            supervisor,
            api: TaskFake::new(),
            runs,
            _dir: dir,
        }
    }

    /// A task whose own read answers `reply`.
    fn reading(reply: crate::error::Result<Option<Registered>>) -> Run {
        let run = Run::unread();
        run.api.on_get(reply);
        run
    }

    /// Another handle on the task's records.
    fn hand(&self) -> Runs {
        Runs::of(self._dir.path(), &Id::try_from(ID).unwrap())
    }

    /// The `%ProgramData%` the records are under.
    fn program_data(&self) -> &Path {
        self._dir.path()
    }

    /// The task's directory of records.
    fn records_dir(&self) -> PathBuf {
        self._dir.path().join("Goetia").join("runs").join(ID)
    }

    /// Nothing started: no program, no log opened for it, no default log held, no record, and every pin
    /// this run took is gone (a removal that waits for a pin would call its note, which panics).
    fn started_nothing(&self) {
        assert_eq!(self.fake.calls(Op::Spawn), 0, "a program was started");
        assert_eq!(self.fake.calls(Op::OpenLog), 0, "a log was opened");
        assert_eq!(self.record(), None);
        self.no_pin_is_left();
    }

    /// No pin is held: the records are removed, which waits for a held pin and so would panic here. Ends
    /// the test's use of the records.
    fn no_pin_is_left(&self) {
        self.runs
            .remove_all(|path, _| panic!("the shim still holds {}", path.display()))
            .unwrap();
    }

    /// The task's default log was held, by the shim, so many times.
    fn default_log_holds(calls: &[ChannelCall]) -> usize {
        calls
            .iter()
            .filter(|call| matches!(call, ChannelCall::HoldDefault { .. }))
            .count()
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

    /// The registration is read `times` more, as each listing that misses the shim has it checked.
    fn read_again(self, times: usize) -> Run {
        for _ in 0..times {
            self.api.on_get(Ok(Some(data_of(&spec_logging_to(None)))));
        }
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
            Some(task_token()),
            &self.supervisor,
            &self.api,
            Ok(self.program_data()),
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

fn requested_of(list: &[&str]) -> Option<(String, Option<Token>)> {
    requested(&args(list))
}

#[skuld::test]
fn task_mode_is_requested_by_the_trailing_arguments_only() {
    assert_eq!(
        requested_of(&["--goetia-task", "frpc", TASK_TOKEN]),
        Some(("frpc".to_owned(), Some(task_token())))
    );
    assert_eq!(
        requested_of(&["--host-flag", "value", "--goetia-task", "frpc", TASK_TOKEN]),
        Some(("frpc".to_owned(), Some(task_token())))
    );
}

/// A registration edited by hand has an ending Task Scheduler passes on as it is: task mode all the same,
/// with no token to run under.
#[skuld::test]
fn a_task_ending_with_no_well_formed_token_is_task_mode_with_none() {
    let named = |token| Some(("frpc".to_owned(), token));
    assert_eq!(requested_of(&["--goetia-task", "frpc"]), named(None));
    assert_eq!(requested_of(&["--host-flag", "--goetia-task", "frpc"]), named(None));
    assert_eq!(requested_of(&["--goetia-task", "frpc", "0123"]), named(None));
    assert_eq!(requested_of(&["--goetia-task", "frpc", &"g".repeat(32)]), named(None));
    let not_utf8 = OsString::from_wide(&[0xD800]);
    assert_eq!(
        requested(&[
            OsString::from("--goetia-task"),
            OsString::from("frpc"),
            not_utf8.clone()
        ]),
        named(None)
    );
}

#[skuld::test]
fn arguments_not_ending_in_the_task_flag_and_an_id_are_service_mode() {
    assert_eq!(requested_of(&[]), None);
    assert_eq!(requested_of(&["frpc"]), None);
    assert_eq!(requested_of(&["--goetia-task"]), None);
    assert_eq!(requested_of(&["--goetia-task", "frpc", TASK_TOKEN, "extra"]), None);
    assert_eq!(requested_of(&["--other", "frpc", TASK_TOKEN]), None);
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

/// A blob that is not a task's, or cannot be read as one, is refused: it is the task's content that is
/// wrong, which no run of the shim changes.
#[skuld::test]
fn a_foreign_or_undecodable_blob_is_refused_and_nothing_runs() {
    let cases = [
        (
            "an administrator's note".to_string(),
            "the task's Data holds no Goetia marker: this task was not created by `goetia daemon install`".to_string(),
        ),
        ("Marker: goetia\nSchema: not-a-schema".to_string(), String::new()),
    ];
    for (data, message) in cases {
        let run = Run::reading(Ok(Some(Registered {
            xml: String::new(),
            data,
            enabled: Some(true),
        })));

        let code = run.run();

        assert_eq!(code, Reported::Refused.process_exit_code() as u32);
        let failures = run.fake.take_failures();
        let expected = if message.is_empty() {
            "decode the task's Data: "
        } else {
            message.as_str()
        };
        assert!(
            failures.len() == 1 && failures[0].starts_with(&format!("goetia-shim[{ID}]: {expected}")),
            "{failures:?}"
        );
        run.started_nothing();
        run.api.finish();
    }
}

/// A task that is gone is a failure of the shim's, not a refusal: a deleted task is the one case an
/// uninstall or an account change makes of a shim that was late.
#[skuld::test]
fn a_deleted_task_ends_the_shim_with_the_shim_failure_code() {
    let run = Run::reading(Ok(None));

    let code = run.run();

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_one_failure(
        &run.fake.take_failures(),
        ID,
        &format!(r"the task \Goetia\{ID} is gone: no program was started"),
    );
    run.started_nothing();
    run.api.finish();
}

fn not_the_registration_started_under() -> String {
    format!(
        "the task \\Goetia\\{ID}'s registration is not the one this shim was started under (another install \
         registered it again, or the shim was started by hand): no program was started"
    )
}

fn no_ending_for_this_task() -> String {
    format!(
        "the task \\Goetia\\{ID}'s registration carries no goetia ending for this task, so it is not the one \
         this shim was started under; `goetia daemon install {ID}` registers it again: no program was started"
    )
}

/// A shim whose registration was replaced starts no program and waits for no listing.
#[skuld::test]
fn a_registration_with_another_token_starts_no_program() {
    let run = Run::of(&spec_logging_to(None));

    let other = Token::parse(OTHER_TOKEN).unwrap();
    let code = run_in(
        ID,
        Some(other),
        &run.supervisor,
        &run.api,
        Ok(run.program_data()),
        OWN_PID,
    );

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_one_failure(&run.fake.take_failures(), ID, &not_the_registration_started_under());
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    assert_eq!(run.record(), None);
    run.api.finish();
}

/// A registration whose arguments end with no goetia ending for this task (a hand edit) is not the one
/// the shim was started under. Each case runs with the arguments Task Scheduler passes: a shim started
/// after the edit gets the edited ending, one started before it the old one.
#[skuld::test]
fn a_registration_with_no_ending_for_this_task_starts_no_program() {
    let stored = registered_task(&spec_logging_to(None), false);
    let ending = format!(" {ID} {TASK_TOKEN}</Arguments>");
    let before = ["--goetia-task", ID, TASK_TOKEN];
    let cases = [
        (
            stored.xml.replace(&ending, &format!(" {ID} 0123</Arguments>")),
            ["--goetia-task", ID, "0123"].as_slice(),
        ),
        (
            stored.xml.replace(&ending, &format!(" {ID}</Arguments>")),
            ["--goetia-task", ID].as_slice(),
        ),
        (stored.xml.replace(&ending, "</Arguments>"), before.as_slice()),
        (
            stored.xml.replace(&ending, &format!(" other {TASK_TOKEN}</Arguments>")),
            before.as_slice(),
        ),
    ];
    for (edited, argv) in cases {
        assert_ne!(edited, stored.xml, "the edit did not apply: {argv:?}");
        let run = Run::reading(Ok(Some(Registered {
            xml: edited,
            ..stored.clone()
        })));
        let (id, token) = requested_of(argv).expect("an ending with the task flag is task mode");

        let code = run_in(&id, token, &run.supervisor, &run.api, Ok(run.program_data()), OWN_PID);

        assert_eq!(
            code,
            Reported::shim_failure(None).process_exit_code() as u32,
            "{argv:?}"
        );
        assert_one_failure(&run.fake.take_failures(), ID, &no_ending_for_this_task());
        assert_eq!(run.fake.calls(Op::Spawn), 0);
        run.api.finish();
    }
}

/// A shim started with no token, under a registration that has one, is not under that registration.
#[skuld::test]
fn a_shim_started_with_no_token_under_a_registration_with_one_starts_no_program() {
    let run = Run::of(&spec_logging_to(None));

    let code = run_in(ID, None, &run.supervisor, &run.api, Ok(run.program_data()), OWN_PID);

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_one_failure(&run.fake.take_failures(), ID, &not_the_registration_started_under());
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    run.api.finish();
}

/// An account change that registers the task again while a shim waits for its listing ends the wait: the
/// new registration never lists the old shim.
#[skuld::test]
fn a_registration_replaced_while_waiting_for_the_listing_ends_the_wait() {
    let spec = spec_logging_to(None);
    let other = Token::parse(OTHER_TOKEN).unwrap();
    let cases = [
        (
            Ok(Some(registered_task_with(&spec, false, &other))),
            not_the_registration_started_under(),
        ),
        (
            Ok(Some(Registered {
                xml: registered_task(&spec, false).xml.replace(TASK_TOKEN, "0123"),
                ..registered_task(&spec, false)
            })),
            no_ending_for_this_task(),
        ),
    ];
    for (second_read, message) in cases {
        let run = Run::of(&spec);
        run.api.on_instances(Ok(vec![])).on_get(second_read);

        let code = run.run();

        assert_eq!(
            code,
            Reported::shim_failure(None).process_exit_code() as u32,
            "{message}"
        );
        assert_one_failure(&run.fake.take_failures(), ID, &message);
        assert_eq!(run.fake.calls(Op::Spawn), 0);
        assert_eq!(run.record(), None);
        run.api.finish();
    }
}

/// A task deleted while the shim, which holds a pin, waits for its listing is gone, as when it is read
/// first: the wait ends, the pin is released, and the uninstall's removal of the records is not held up.
#[skuld::test]
fn a_pinned_shim_ends_when_the_task_is_deleted() {
    let run = Run::of(&spec_logging_to(None));
    run.api.on_instances(Ok(vec![])).on_get(Ok(None));

    let code = run.run();

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_one_failure(
        &run.fake.take_failures(),
        ID,
        &format!(r"the task \Goetia\{ID} is gone: no program was started"),
    );
    run.started_nothing();
    run.api.finish();
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

/// The removal's marker stands, as an uninstall or an account change leaves it: the shim's pin is refused.
#[skuld::test]
fn refused_pin_ends_the_shim_before_it_reads_the_task() {
    let run = Run::unread();
    run.runs.close().unwrap();

    let code = run.run();

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_eq!(run.api.calls(), vec![], "Task Scheduler was asked");
    assert_eq!(run.fake.take_failures().len(), 1, "the refusal is told once");
    run.started_nothing();
    run.api.finish();
}

#[skuld::test]
fn refused_shim_opens_no_log() {
    let run = Run::unread();
    run.runs.close().unwrap();

    run.run();

    assert_eq!(run.fake.calls(Op::OpenLog), 0);
    assert_eq!(
        run.fake.default_log_opens(),
        0,
        "the refusal's line opened the default log"
    );
    let calls = run.fake.take_channel_calls();
    assert_eq!(Run::default_log_holds(&calls), 0);
    assert!(
        matches!(calls.as_slice(), [ChannelCall::EventLog { severity: Severity::Error, line }]
            if line.contains("is being, or did not finish being, installed or uninstalled")),
        "the refusal is told to the Event Log alone: {calls:?}"
    );
}

/// The pin is taken before the default log is held: a shim whose pin fails never holds it, and one whose
/// pin is made does.
#[skuld::test]
fn shim_pins_before_it_holds_the_default_log() {
    let refused = Run::unread();
    std::fs::remove_dir(refused.records_dir()).unwrap();
    refused.run();
    assert_eq!(Run::default_log_holds(&refused.fake.take_channel_calls()), 0);

    let run = Run::of(&spec_logging_to(None)).listed().program(exits(0));
    run.run();
    assert_eq!(Run::default_log_holds(&run.fake.take_channel_calls()), 1);
    run.finish();
}

/// A refused run is a missed one, and the log says so, naming the records and why they are not held.
#[skuld::test]
fn shim_refused_by_the_marker_of_a_refused_change_logs_it() {
    let run = Run::unread();
    run.runs.close().unwrap();

    run.run();

    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!(
                "goetia-shim[{ID}]: hold the run records {}: ",
                run.records_dir().display()
            ))
            && failures[0].contains("is being, or did not finish being, installed or uninstalled")
            && failures[0].ends_with(": no program was started"),
        "{failures:?}"
    );
}

/// The pin's refusal at the first step, whatever its cause, ends the shim with the OS error it met or none,
/// and the line names the records.
fn pin_failure_starts_no_program(run: &Run) -> u32 {
    let code = run.run();

    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!(
                "goetia-shim[{ID}]: hold the run records {}: ",
                run.records_dir().display()
            ))
            && failures[0].ends_with(": no program was started"),
        "{failures:?}"
    );
    assert_eq!(run.api.calls(), vec![], "Task Scheduler was asked");
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    assert_eq!(run.fake.calls(Op::OpenLog), 0);
    assert_eq!(Run::default_log_holds(&run.fake.take_channel_calls()), 0);
    run.api.finish();
    code
}

struct Restore(PathBuf);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Ok(everyone) = Descriptor::from_sddl("D:P(A;;FA;;;WD)") {
            let _ = set_dacl(&everyone, &self.0);
        }
    }
}

#[skuld::test]
fn a_pin_that_is_denied_starts_no_program() {
    let run = Run::unread();
    let _restore = Restore(run.records_dir());
    let stranger_only = Descriptor::from_sddl(&format!("D:P(A;;FA;;;{A_STRANGER})")).unwrap();
    set_dacl(&stranger_only, &run.records_dir()).unwrap();

    let code = pin_failure_starts_no_program(&run);

    assert_eq!(code, ERROR_ACCESS_DENIED);
}

#[skuld::test]
fn a_pin_in_a_directory_that_is_gone_starts_no_program() {
    let run = Run::unread();
    std::fs::remove_dir(run.records_dir()).unwrap();

    let code = pin_failure_starts_no_program(&run);

    assert_eq!(code, ERROR_FILE_NOT_FOUND);
}

#[skuld::test]
fn a_pin_in_a_directory_that_is_a_link_starts_no_program() {
    let run = Run::unread();
    let target = tempfile::tempdir().unwrap();
    make_mount_point(&run.records_dir(), target.path());

    let code = pin_failure_starts_no_program(&run);

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_eq!(
        std::fs::read_dir(target.path()).unwrap().count(),
        0,
        "nothing was made in the target"
    );
}

/// A marker placed while the shim waits for its listing refuses the program: it was listed, and its pin
/// was made before the marker, so the removal waits for it; the shim looks again after the listing, before
/// it opens a log or spawns, and a marker that stands is a start it must not make. Its pin is released.
#[skuld::test]
fn shim_rechecks_the_marker_after_its_listing() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    let removal = run.hand();
    run.api.on_instances(Ok(listed())).during(move || {
        removal.close().unwrap();
    });

    let code = run.run();

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!(
                "goetia-shim[{ID}]: the run records {} are being removed or handed to another account: ",
                run.records_dir().display()
            ))
            && failures[0].ends_with("no program was started"),
        "{failures:?}"
    );
    run.started_nothing();
    run.api.finish();
}

/// A look for the marker that cannot be made after the listing ends the shim with the error it met, with no
/// program started and no log opened. Here the listing leaves a marker whose delete is pending, which Windows
/// answers with access denied.
#[skuld::test]
fn a_marker_look_that_fails_starts_no_program() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    let marker = run.records_dir().with_file_name(format!("{ID}~removing"));
    let held = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&held);
    run.api.on_instances(Ok(listed())).during(move || {
        *slot.lock().unwrap() = Some(delete_pending(&marker));
    });

    let code = run.run();
    drop(held.lock().unwrap().take());

    assert_eq!(code, ERROR_ACCESS_DENIED);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!(
                "goetia-shim[{ID}]: look for the removal's marker of the run records {}: ",
                run.records_dir().display()
            ))
            && failures[0].ends_with(": no program was started"),
        "{failures:?}"
    );
    run.started_nothing();
    run.api.finish();
}

/// The run's pin is held through the listing and the run, and released as its record is renamed in place,
/// before its own prune: the records of other runs are kept while it is held and gone after, and the
/// directory reports the record's rename before the pin's removal, so no other run's prune could have
/// found the record's temp file with no pin held.
#[skuld::test]
fn a_run_records_under_its_own_pin_and_drops_it_after_the_rename() {
    let run = Run::of(&spec_logging_to(None));
    let mut watch = DirWatch::new(&run.records_dir());
    let other = run.hand();
    let earlier = |n: u8| RunRecord {
        instance: InstanceId::parse(&format!("{n:032x}")).unwrap(),
        started: u64::from(n),
        outcome: RunOutcome::Exited(0),
    };
    let (first, second) = (earlier(5), earlier(6));
    let seen = (first.clone(), second.clone());
    run.api.on_instances(Ok(listed())).during(move || {
        other.write(&seen.0, None).unwrap();
        other.write(&seen.1, None).unwrap();
        assert!(
            other.read(&seen.0.instance).unwrap().is_some(),
            "the shim's pin did not keep an earlier record from the next write's prune"
        );
    });
    let run = run.program(exits(0));

    assert_eq!(run.run(), 0);

    assert_eq!(without_start(run.record()), recorded(RunOutcome::Exited(0)));
    assert_eq!(
        run.runs.read(&first.instance).unwrap(),
        None,
        "pruned once the pin is gone"
    );
    assert_eq!(run.runs.read(&second.instance).unwrap(), None);
    // The directory's own account of the order: the last name is the test's, made once the run is over.
    std::fs::write(run.records_dir().join("done"), "").unwrap();
    let changes = watch.until_added("done");
    let position = |wanted: &dyn Fn(&NameChange) -> bool| changes.iter().position(wanted);
    let own = format!("{}.json", instance().hex());
    let renamed_in = position(&|change| *change == NameChange::RenamedTo(own.clone()));
    let pin_removed = position(&|change| matches!(change, NameChange::Removed(name) if name.ends_with(".pin")));
    assert!(
        renamed_in.is_some() && pin_removed.is_some() && renamed_in < pin_removed,
        "the record was renamed in at {renamed_in:?}, the pin removed at {pin_removed:?}: {changes:?}"
    );
    drop(watch);
    run.finish();
    run.no_pin_is_left();
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

        let code = run_in(
            ID,
            Some(task_token()),
            &run.supervisor,
            &api,
            Ok(run.program_data()),
            OWN_PID,
        );

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

// A listing that cannot be read =======================================================================================

/// The OS error a failed listing carries, which the shim ends with.
const LISTING_FAILED: u32 = ERROR_FILE_NOT_FOUND;

fn listing_fails() -> crate::error::Error {
    Error::os(
        "list the task's instances",
        io::Error::from_raw_os_error(LISTING_FAILED as i32),
    )
}

/// A listing that cannot be read, with the task gone, starts no program: the shim cannot tell its own
/// instance from another's, and nothing it could record would name its run.
#[skuld::test]
fn failed_listing_with_the_task_gone_starts_no_program() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    run.api.on_instances(Err(listing_fails())).on_get(Ok(None));

    let code = run.run();

    assert_eq!(code, LISTING_FAILED);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!("goetia-shim[{ID}]: list the task's instances: "))
            && failures[0].ends_with(&format!(r"the task \Goetia\{ID} is gone: no program was started")),
        "{failures:?}"
    );
    run.started_nothing();
    run.api.finish();
}

/// The `get` after the failed listing only words the line: with the task present the code is the same.
#[skuld::test]
fn failed_listing_with_the_task_present_starts_no_program() {
    let run = Run::of(&spec_logging_to(Some(LOGS)));
    run.api
        .on_instances(Err(listing_fails()))
        .on_get(Ok(Some(data_of(&spec_logging_to(Some(LOGS))))));

    let code = run.run();

    assert_eq!(code, LISTING_FAILED);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!("goetia-shim[{ID}]: list the task's instances: "))
            && failures[0].ends_with(": no program was started")
            && !failures[0].contains("is gone"),
        "{failures:?}"
    );
    run.started_nothing();
    run.api.finish();
}

/// With another registration in the task's place the line says so, and the code is the listing's still.
#[skuld::test]
fn failed_listing_with_another_registration_starts_no_program() {
    let spec = spec_logging_to(Some(LOGS));
    let run = Run::of(&spec);
    let other = Token::parse(OTHER_TOKEN).unwrap();
    run.api
        .on_instances(Err(listing_fails()))
        .on_get(Ok(Some(registered_task_with(&spec, false, &other))));

    let code = run.run();

    assert_eq!(code, LISTING_FAILED);
    let failures = run.fake.take_failures();
    assert!(
        failures.len() == 1
            && failures[0].starts_with(&format!("goetia-shim[{ID}]: list the task's instances: "))
            && failures[0].ends_with(&not_the_registration_started_under()),
        "{failures:?}"
    );
    run.started_nothing();
    run.api.finish();
}

// The wait for the run's own instance =================================================================================

/// Task Scheduler lists a run once it has started it, which can be after the shim reads the listing:
/// the shim reads again, and the program starts when its own instance is listed.
#[skuld::test]
fn a_listing_that_misses_the_shim_is_read_again_and_the_run_is_recorded() {
    let run = Run::of(&spec_logging_to(Some(LOGS))).read_again(1);
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
    let run = Run::of(&spec_logging_to(None)).read_again(1);
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
    let run = Run::of(&spec_logging_to(Some(LOGS))).read_again(1);
    run.api.on_instances(Ok(vec![])).on_instances(Ok(listed()));
    let api = StopsDuring {
        inner: run.api.clone(),
        bus: Arc::clone(&run.supervisor.bus),
        at: Read::Instances,
    };

    let code = run_in(
        ID,
        Some(task_token()),
        &run.supervisor,
        &api,
        Ok(run.program_data()),
        OWN_PID,
    );

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
    let run = Run::of(&spec_logging_to(None)).read_again(1);
    run.api.on_instances(Ok(vec![])).on_instances(Ok(vec![]));
    let api = StopsDuring {
        inner: run.api.clone(),
        bus: Arc::clone(&run.supervisor.bus),
        at: Read::Instances,
    };
    let pin = run.runs.pin().unwrap();

    let code = run_in(
        ID,
        Some(task_token()),
        &run.supervisor,
        &api,
        Ok(run.program_data()),
        OWN_PID,
    );

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
    let run = Run::of(&spec_logging_to(Some(LOGS))).listed().program(exits(5));
    // A directory where the record's temp file is to be made: the pin is made, and the write fails.
    directory_with_a_file(&run.records_dir().join(format!("{}.json.tmp", instance().hex())));

    let code = run.run();

    assert_eq!(code, 5);
    assert_eq!(run.record(), None);
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
    run.no_pin_is_left();
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

/// A shim with no `%ProgramData%` holds no pin and so starts no program, and reads nothing.
#[skuld::test]
fn no_program_data_starts_no_program() {
    let run = Run::unread();
    let cause = Error::os("locate %ProgramData%", io::Error::other("no such known folder"));

    let code = run_in(ID, Some(task_token()), &run.supervisor, &run.api, Err(&cause), OWN_PID);

    assert_eq!(code, Reported::shim_failure(None).process_exit_code() as u32);
    assert_one_failure(
        &run.fake.take_failures(),
        ID,
        &format!(
            "no program was started: the run records cannot be held, since the machine's %ProgramData% could not be located: {}",
            cause.report()
        ),
    );
    assert_eq!(run.fake.calls(Op::Spawn), 0);
    assert_eq!(run.api.calls(), vec![], "the task was read");
    run.api.finish();
}

// What it never does ==================================================================================================

/// Failures before the program start are logged once each, and the two kinds of refusal a bad id and
/// a bad task name are not read from Task Scheduler at all.
#[skuld::test]
fn an_id_that_is_not_an_id_reads_nothing_and_runs_nothing() {
    let fake = FakeGuard::new();
    let api = TaskFake::new();

    let code = run_in(
        "not an id!",
        Some(task_token()),
        &supervisor(&fake),
        &api,
        Ok(Path::new("unused")),
        OWN_PID,
    );

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
