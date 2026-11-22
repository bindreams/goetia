//! Which run a `run` follows, and how it follows it, over the table-driven fake of Task Scheduler, with
//! the run records in a temporary directory and the machine's processes a table the test states.

use std::fs;
use std::io;
use std::path::Path;

use super::super::support::{
    Machine, PATH, id, instance, instance_id, pins_in, record, records, scm, shim_writes, shim_writes_stopping,
    snapshot_of,
};
use super::*;
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::runs::RunOutcome;
use crate::backend::scm::test_support::{directory_with_a_file, task_spec};
use crate::spec::{DaemonSpec, TaskSpec};

fn named(n: u8, pid: u32) -> Started {
    Started {
        id: instance_id(n),
        pid: Some(pid),
    }
}

fn unnamed(n: u8) -> Started {
    Started {
        id: instance_id(n),
        pid: None,
    }
}

fn written(n: u8, started: u64, outcome: RunOutcome) -> RunRecord {
    RunRecord {
        instance: instance_id(n),
        started,
        outcome,
    }
}

// identify ------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_named_instance_that_is_live_is_followed_by_the_pid_it_is_listed_with() {
    let live = [instance(2, 200), instance(1, 100)];

    assert_eq!(
        identify(Some(&named(1, 4242)), &live, &[]),
        Identity::Live {
            id: instance_id(1),
            pid: 100
        }
    );
}

#[skuld::test]
fn a_named_instance_that_is_not_live_reads_its_own_record() {
    let appeared = [written(1, 5, RunOutcome::Exited(3))];

    assert_eq!(
        identify(Some(&named(1, 4242)), &[], &appeared),
        Identity::Done(appeared[0].clone())
    );
}

#[skuld::test]
fn a_named_instance_that_is_not_live_and_has_no_record_is_unrecorded() {
    assert_eq!(identify(Some(&named(1, 4242)), &[], &[]), Identity::Unrecorded);
}

/// A scheduled run that ended before this one's start leaves its record in the directory too: the
/// record the `Run` named is the one that counts, and no other.
#[skuld::test]
fn a_named_instances_record_is_not_taken_for_a_scheduled_runs_earlier_one() {
    let scheduled = written(9, 1, RunOutcome::Exited(7));
    let own = written(1, 2, RunOutcome::Exited(0));

    assert_eq!(
        identify(Some(&named(1, 4242)), &[], &[scheduled.clone(), own.clone()]),
        Identity::Done(own.clone())
    );
    assert_eq!(
        identify(Some(&named(1, 4242)), &[], &[own.clone(), scheduled]),
        Identity::Done(own)
    );
}

/// Two records and neither is the one named: nothing says which is ours.
#[skuld::test]
fn a_named_instance_with_other_records_only_is_unrecorded() {
    let others = [
        written(8, 1, RunOutcome::Exited(1)),
        written(9, 2, RunOutcome::Exited(2)),
    ];

    assert_eq!(identify(Some(&named(1, 4242)), &[], &others), Identity::Unrecorded);
}

/// A named instance is followed even when a scheduled run is live beside it.
#[skuld::test]
fn a_named_live_instance_is_followed_beside_another_live_one() {
    let live = [instance(9, 900), instance(1, 100)];

    assert_eq!(
        identify(Some(&named(1, 100)), &live, &[]),
        Identity::Live {
            id: instance_id(1),
            pid: 100
        }
    );
}

/// M7: a `Run` against a live run under `IgnoreNew` returns a fresh GUID with no pid, which is never
/// listed. One run is live, and it began after the command was asked: it is the run followed.
#[skuld::test]
fn a_start_that_named_nothing_follows_the_one_live_instance() {
    assert_eq!(
        identify(Some(&unnamed(1)), &[instance(9, 900)], &[]),
        Identity::Live {
            id: instance_id(9),
            pid: 900
        }
    );
    assert_eq!(
        identify(None, &[instance(9, 900)], &[]),
        Identity::Live {
            id: instance_id(9),
            pid: 900
        }
    );
}

/// A `Run` whose `EnginePID` read `SCHED_E_TASK_NOT_RUNNING` returns the instance's GUID and no pid: the
/// run began and had ended already. Its own record, whichever runs are alive or recorded beside it, is
/// the run followed.
#[skuld::test]
fn a_start_that_named_no_pid_but_whose_own_run_left_a_record_follows_that_record() {
    let own = written(1, 7, RunOutcome::Exited(0));
    // Another run's record that began earlier: the earliest rule would take it.
    let earlier = written(9, 2, RunOutcome::Exited(5));

    // None alive.
    assert_eq!(
        identify(Some(&unnamed(1)), &[], &[earlier.clone(), own.clone()]),
        Identity::Done(own.clone())
    );
    // One other run alive: not the one followed.
    assert_eq!(
        identify(Some(&unnamed(1)), &[instance(9, 900)], std::slice::from_ref(&own)),
        Identity::Done(own.clone())
    );
    // Several others alive: not unrecorded.
    assert_eq!(
        identify(
            Some(&unnamed(1)),
            &[instance(8, 800), instance(9, 900)],
            &[earlier, own.clone()]
        ),
        Identity::Done(own)
    );
}

/// The instance `Run` named, listed live, is the run followed whatever pid `Run` returned and whatever else
/// is live or recorded: a run that is alive is waited for until its process exits.
#[skuld::test]
fn an_instance_listed_live_is_followed_whether_or_not_run_returned_a_pid() {
    let others = [instance(9, 900), instance(1, 100), instance(8, 800)];
    let live = Identity::Live {
        id: instance_id(1),
        pid: 100,
    };

    assert_eq!(identify(Some(&unnamed(1)), &others, &[]), live);
    assert_eq!(identify(Some(&named(1, 4242)), &others, &[]), live);
    assert_eq!(identify(Some(&unnamed(1)), &[instance(1, 100)], &[]), live);
}

/// A record can appear while the run's instance is still listed (the shim writes it before the process
/// is gone): the listing says the run is alive, so it is followed until it exits, and its record is read
/// then.
#[skuld::test]
fn an_instance_listed_live_is_followed_though_its_record_has_appeared() {
    let own = written(1, 7, RunOutcome::Exited(0));
    let live = Identity::Live {
        id: instance_id(1),
        pid: 100,
    };

    assert_eq!(
        identify(Some(&unnamed(1)), &[instance(1, 100)], std::slice::from_ref(&own)),
        live
    );
    assert_eq!(
        identify(Some(&named(1, 4242)), &[instance(1, 100)], std::slice::from_ref(&own)),
        live
    );
}

#[skuld::test]
fn a_run_that_started_nothing_is_not_live_and_is_unrecorded() {
    assert_eq!(identify(Some(&unnamed(1)), &[], &[]), Identity::Unrecorded);
}

#[skuld::test]
fn a_start_that_named_nothing_reads_the_one_record_that_appeared() {
    let appeared = [written(9, 5, RunOutcome::Stopped)];

    assert_eq!(
        identify(Some(&unnamed(1)), &[], &appeared),
        Identity::Done(appeared[0].clone())
    );
}

/// The user's own concurrent start: several runs ended since the pin, and the earliest to begin is the
/// one taken, whatever order the records are given in.
#[skuld::test]
fn a_start_that_named_nothing_reads_the_earliest_of_several_records() {
    let late = written(1, 9, RunOutcome::Exited(1));
    let early = written(2, 3, RunOutcome::Exited(2));

    assert_eq!(
        identify(Some(&unnamed(7)), &[], &[late.clone(), early.clone()]),
        Identity::Done(early)
    );
}

/// Several live and none named (L2's overlap, or the user's own concurrent start): nothing says which
/// is ours.
#[skuld::test]
fn several_live_and_none_named_is_unrecorded() {
    let live = [instance(8, 800), instance(9, 900)];

    assert_eq!(identify(Some(&unnamed(1)), &live, &[]), Identity::Unrecorded);
    assert_eq!(
        identify(Some(&unnamed(1)), &live, &[written(7, 1, RunOutcome::Exited(0))]),
        Identity::Unrecorded
    );
}

// begin_run -----------------------------------------------------------------------------------------------------------

fn spec_with_logs(logs: &Path) -> DaemonSpec {
    let DaemonSpec::Task(task) = task_spec(None) else {
        unreachable!("`task_spec` builds a task")
    };
    let mut fields = task.into_fields();
    fields.common.logs = Some(logs.to_owned());
    DaemonSpec::from(TaskSpec::new(fields).expect("a task with logs"))
}

fn begin<P: Processes>(fake: &Fake, spec: &DaemonSpec, runs: Runs, processes: P) -> Result<TaskRun<P>> {
    scm(fake).task_begin_run_with(snapshot_of(spec, false), runs, processes)
}

/// The error of a `begin_run` that failed: a run is no `Debug`, so `unwrap_err` is not for it.
fn failure<T>(begun: Result<T>) -> Error {
    match begun {
        Ok(_) => panic!("the run began"),
        Err(e) => e,
    }
}

/// The pin is taken, and the directory listed, before `Run` is called: a record that appeared after
/// the pin is this run's candidate, and one that was already there is not.
#[skuld::test]
fn begin_run_takes_the_pin_and_lists_before_run_is_called() {
    let (dir, path, runs) = records();
    let older = Runs::of(dir.path(), &id());
    older.write(&written(9, 0, RunOutcome::Exited(7)), None).unwrap();
    let fake = Fake::new();
    let shim = Runs::of(dir.path(), &id());
    fake.on_run(Ok(unnamed(1)))
        .during(move || {
            assert_eq!(pins_in(&path), 1, "the pin is held when Run is called");
            shim.write(&written(1, 5, RunOutcome::Exited(0)), None).unwrap();
        })
        .on_instances(Ok(Vec::new()));

    let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();

    assert_eq!(
        fake.calls(),
        [Call::Run(PATH.to_owned()), Call::Instances(PATH.to_owned())]
    );
    assert_eq!(run.wait().unwrap(), Exit::Code(0), "the older record is not this run's");
    fake.finish();
}

#[skuld::test]
fn begin_run_does_not_reach_run_when_the_pin_cannot_be_taken() {
    let (_dir, path, runs) = records();
    fs::remove_dir_all(&path).unwrap();
    let fake = Fake::new();

    let failed = failure(begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])));

    assert!(failed.report().to_string().contains("run records"), "{failed}");
    assert_eq!(fake.calls(), [], "nothing was sent");
}

#[skuld::test]
fn a_failed_run_is_the_calls_error_and_reads_nothing() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Err(Error::os("start", io::Error::other("denied"))));

    let failed = failure(begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])));

    assert!(!matches!(failed, Error::RunUnrecorded { .. }), "{failed:?}");
    assert_eq!(fake.calls(), [Call::Run(PATH.to_owned())]);
}

/// The run began and goetia cannot tell which it was: the run may have happened.
#[skuld::test]
fn a_listing_that_fails_after_the_start_leaves_the_run_unrecorded() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Err(Error::os("list the runs", io::Error::other("busy"))));

    let failed = failure(begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])));

    assert!(matches!(failed, Error::RunUnrecorded { .. }), "{failed:?}");
    assert!(failed.report().to_string().contains("busy"), "{failed}");
}

/// A first `Stopped` that `run` reads while its run is alive is not confirmed by a listing: Task Scheduler stops listing
/// an instance within milliseconds of a stop while its shim lives about a second more. The run is followed as ended, and
/// `wait` settles it by the pin its writer holds: here the shim is gone (its pin is), and nothing recorded after, so the
/// answer is the first `Stopped`, after one read of the listing.
#[skuld::test]
fn a_first_stopped_of_a_run_whose_shim_is_gone_is_stopped_after_one_listing() {
    let (dir, _path, runs) = records();
    let first = Runs::of(dir.path(), &id());
    let fake = Fake::new();
    fake.on_run(Ok(unnamed(9)))
        .during(move || shim_writes_stopping(&first, 1))
        .on_instances(Ok(Vec::new()));

    let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();

    assert_eq!(run.wait().unwrap(), Exit::Code(CLOSED as i32));
    assert_eq!(
        fake.calls(),
        [Call::Run(PATH.to_owned()), Call::Instances(PATH.to_owned())],
        "no second read of the listing"
    );
    fake.finish();
}

/// No record costs a second listing, whatever its outcome.
#[skuld::test]
fn a_record_costs_no_second_listing_whatever_its_outcome() {
    for outcome in [RunOutcome::Exited(0), RunOutcome::Stopped, RunOutcome::IncompleteStop] {
        let (dir, _path, runs) = records();
        let shim = Runs::of(dir.path(), &id());
        let fake = Fake::new();
        fake.on_run(Ok(unnamed(9)))
            .during(move || shim_writes(&shim, &record(1, outcome)))
            .on_instances(Ok(Vec::new()));

        let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();
        drop(run);

        assert_eq!(
            fake.calls(),
            [Call::Run(PATH.to_owned()), Call::Instances(PATH.to_owned())],
            "{outcome:?}"
        );
        fake.finish();
    }
}

/// A directory by the name of a pin, which a delete cannot judge: what a provisional file that names it can only be
/// settled with an error.
fn unjudgeable_pin(path: &Path) -> String {
    let name = format!("pin-{}.pin", "0".repeat(32));
    directory_with_a_file(&path.join(&name));
    name
}

/// A first `Stopped` whose writer's pin cannot be told held or not is no answer: the run is unrecorded, with the failed
/// delete as its source, naming the pin.
#[skuld::test]
fn a_first_stopped_whose_writers_pin_cannot_be_judged_is_an_unrecorded_run_naming_the_pin() {
    let (_dir, path, runs) = records();
    let pin = unjudgeable_pin(&path);
    let provisional = path.join(format!("{}.stopping", instance_id(1).hex()));
    let text = format!(r#"{{"started":1,"pin":"{pin}"}}"#);
    let fake = Fake::new();
    // Written after the pin was taken, as a stop that finds the run begun writes it.
    fake.on_run(Ok(unnamed(9)))
        .during(move || fs::write(&provisional, &text).unwrap())
        .on_instances(Ok(Vec::new()));

    let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();
    let failed = run.wait().unwrap_err();

    assert!(matches!(failed, Error::RunUnrecorded { .. }), "{failed:?}");
    assert!(std::error::Error::source(&failed).is_some(), "{failed:?}");
    assert!(failed.report().to_string().contains(&pin), "{failed}");
}

/// The same for a run whose process was gone when it was opened: its end is read through the pin too.
#[skuld::test]
fn a_process_gone_at_the_open_settles_a_first_stopped_by_the_writers_pin() {
    let (_dir, path, runs) = records();
    let pin = unjudgeable_pin(&path);
    fs::write(
        path.join(format!("{}.stopping", instance_id(1).hex())),
        format!(r#"{{"started":1,"pin":"{pin}"}}"#),
    )
    .unwrap();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let mut machine = Machine::new(&fake, &[4242]);
    machine.holdable.clear();

    let run = begin(&fake, &task_spec(None), runs, machine).unwrap();
    let failed = run.wait().unwrap_err();

    assert!(matches!(failed, Error::RunUnrecorded { .. }), "{failed:?}");
    assert!(failed.report().to_string().contains(&pin), "{failed}");
}

/// The process is waited for, and only then is the record read: the shim writes it as it ends.
#[skuld::test]
fn wait_blocks_on_the_held_process_and_then_reads_the_record() {
    let (dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let mut machine = Machine::new(&fake, &[4242]);
    let shim = Runs::of(dir.path(), &id());
    machine.on_exit = Box::new(move |_| shim_writes(&shim, &record(1, RunOutcome::Exited(3))));

    let run = begin(&fake, &task_spec(None), runs, machine).unwrap();
    assert_eq!(run.wait().unwrap(), Exit::Code(3));
    fake.finish();
}

/// A process that is gone when it is opened has ended: its record, which its shim wrote before it
/// exited, is read by its instance.
#[skuld::test]
fn a_process_gone_at_the_open_reads_the_record_by_the_instance() {
    let (dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let mut machine = Machine::new(&fake, &[4242]);
    machine.holdable.clear();
    Runs::of(dir.path(), &id())
        .write(&record(1, RunOutcome::Exited(4)), None)
        .unwrap();

    let run = begin(&fake, &task_spec(None), runs, machine).unwrap();

    assert_eq!(run.wait().unwrap(), Exit::Code(4));
    fake.finish();
}

#[skuld::test]
fn wait_is_unrecorded_when_the_process_ended_and_no_record_exists() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let machine = Machine::new(&fake, &[4242]);

    let run = begin(&fake, &task_spec(None), runs, machine).unwrap();
    let failed = run.wait().unwrap_err();

    assert!(
        matches!(&failed, Error::RunUnrecorded { reason, .. } if reason == UNRECORDED),
        "{failed:?}"
    );
    assert_eq!(
        UNRECORDED,
        "it left no record: its program may have run and failed to record, its shim may have started no \
         program, it may have been killed or failed to log on, or its program was still running about a second \
         after a stop, when Windows ends the shim together with it before anything could be recorded; the \
         task's log and the Event Log may say more"
    );
}

#[skuld::test]
fn exited_stopped_and_incomplete_records_map_to_their_results() {
    for (outcome, expected) in [
        (RunOutcome::Exited(0), Some(Exit::Code(0))),
        (RunOutcome::Exited(7), Some(Exit::Code(7))),
        (
            RunOutcome::Exited(0xC0DE_0011),
            Some(Exit::Code(0xC0DE_0011_u32 as i32)),
        ),
        (RunOutcome::Stopped, Some(Exit::Code(0xC000_013A_u32 as i32))),
        (RunOutcome::IncompleteStop, None),
    ] {
        let (dir, _path, runs) = records();
        let shim = Runs::of(dir.path(), &id());
        let fake = Fake::new();
        // The run ended while `Run` was being answered.
        fake.on_run(Ok(named(1, 4242)))
            .during(move || shim_writes(&shim, &record(1, outcome)))
            .on_instances(Ok(Vec::new()));

        let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();
        let waited = run.wait();

        match expected {
            Some(exit) => assert_eq!(waited.unwrap(), exit, "{outcome:?}"),
            None => {
                let failed = waited.unwrap_err();
                let report = failed.report().to_string();
                assert!(report.contains("could not end the program's process tree"), "{report}");
            }
        }
    }
}

/// L2: the ended run's entry is still listed, and its process is gone: the run's end is the process's,
/// and nothing reads the listing again to see it drop.
#[skuld::test]
fn the_end_of_a_run_does_not_wait_for_the_listing_to_drop() {
    let (dir, _path, runs) = records();
    let shim = Runs::of(dir.path(), &id());
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .during(move || {
            shim.write(&record(1, RunOutcome::Exited(0)), None).unwrap();
        })
        .on_instances(Ok(vec![instance(1, 4242)]));

    let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();

    assert_eq!(run.wait().unwrap(), Exit::Code(0));
    fake.finish();
}

// output --------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_run_with_logs_names_the_marker_of_its_instance_and_may_have_an_earlier_start() {
    let (dir, _path, runs) = records();
    let log = dir.path().join("job.log");
    fs::write(&log, b"earlier\n").unwrap();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let machine = Machine::new(&fake, &[4242]);

    let run = begin(&fake, &spec_with_logs(&log), runs, machine).unwrap();

    let OutputSource::LogFile { path, offset, marker } = run.output() else {
        panic!("a task with logs has a log file to follow");
    };
    assert_eq!(path, log);
    assert_eq!(offset, 8, "the log's length before the run was requested");
    let marker = marker.expect("every run marks its start");
    assert_eq!(marker.prefix, "goetia-run");
    assert_eq!(marker.line, format!("goetia-run {}", instance_id(1).hex()));
    assert_eq!(marker.id_len, 32);
    assert!(marker.earlier_start_possible);
}

#[skuld::test]
fn a_run_without_logs_has_no_output_source() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242))).on_instances(Ok(Vec::new()));

    let run = begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])).unwrap();

    assert!(matches!(run.output(), OutputSource::None));
}

/// A start that was ignored never writes a marker: the run still ends, unrecorded, and the marker it
/// names is the GUID `Run` returned.
#[skuld::test]
fn a_run_that_never_writes_its_marker_still_ends_and_is_unrecorded() {
    let (dir, _path, runs) = records();
    let log = dir.path().join("job.log");
    let fake = Fake::new();
    fake.on_run(Ok(unnamed(1))).on_instances(Ok(Vec::new()));

    let run = begin(&fake, &spec_with_logs(&log), runs, Machine::new(&fake, &[])).unwrap();

    let OutputSource::LogFile { offset, marker, .. } = run.output() else {
        panic!("a task with logs has a log file to follow");
    };
    assert_eq!(offset, 0, "a log that is not there is empty");
    assert_eq!(marker.unwrap().line, format!("goetia-run {}", instance_id(1).hex()));
    assert!(matches!(run.wait().unwrap_err(), Error::RunUnrecorded { .. }));
}

// The error of a run that could not be followed is the unrecorded run's source ----------------------------------------

/// A run that began and could not be told apart: the failure that said so is the error's source, so its OS
/// code is found and its remedy is collected, and the text is as it was.
#[skuld::test]
fn an_unfollowed_run_returns_the_failure_that_left_it_unfollowed() {
    let (_dir, _path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Err(Error::os("list the runs", io::Error::from_raw_os_error(1722))));

    let failed = failure(begin(&fake, &task_spec(None), runs, Machine::new(&fake, &[])));

    assert_eq!(failed.raw_os_error(), Some(1722), "{failed:?}");
    assert!(std::error::Error::source(&failed).is_some(), "{failed:?}");
    assert_eq!(
        failed.report().to_string(),
        format!(
            "the run of `nightly` ended without recording its exit status: goetia could not tell which run it \
             began: failed to list the runs: {}",
            io::Error::from_raw_os_error(1722)
        )
    );
}

/// A record that cannot be read leaves the run unrecorded, and the failed read is the error's source.
#[skuld::test]
fn a_run_whose_record_cannot_be_read_returns_the_failed_read() {
    let (_dir, path, runs) = records();
    let fake = Fake::new();
    fake.on_run(Ok(named(1, 4242)))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let mut machine = Machine::new(&fake, &[4242]);
    let record_path = path.join(format!("{}.json", instance_id(1).hex()));
    machine.on_exit = Box::new(move |_| fs::write(&record_path, b"not a record").unwrap());

    let run = begin(&fake, &task_spec(None), runs, machine).unwrap();
    let failed = run.wait().unwrap_err();

    assert!(matches!(failed, Error::RunUnrecorded { .. }), "{failed:?}");
    assert!(std::error::Error::source(&failed).is_some(), "{failed:?}");
    let report = failed.report().to_string();
    assert!(
        report.starts_with(
            "the run of `nightly` ended without recording its exit status: its record could not be read: failed to read it: "
        ),
        "{report}"
    );
}
