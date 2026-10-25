use std::time::Duration;

use cosca::identity::{Platform, ProcessId};

use super::*;

const BOOT: Option<u64> = Some(0x01DB_0000_0000_0001);
const OTHER_BOOT: Option<u64> = Some(0x01DB_0000_0000_9999);

fn at(ms: u64) -> Clock {
    Clock { boot: BOOT, ms }
}

fn failed(at_ms: u64, restart: RestartDue) -> StateRecord {
    let mut record = StateRecord::empty();
    record.failure = Some(FailureRecord {
        boot: BOOT,
        at_ms,
        restart,
    });
    record
}

fn shim_state(record: StateRecord, now_ms: u64) -> ShimState {
    ShimState {
        loaded: Loaded::Record(record),
        now: at(now_ms),
        disabled: false,
    }
}

/// This test process as a survivor record: alive for as long as the test runs.
fn this_process() -> cosca::identity::ProcessIdRecord {
    ProcessId::current().to_record().expect("this process has an identity")
}

// The pending rule: one row per sequence measured in 59-probes.md rounds 2-6 ==========================================

#[skuld::test]
fn no_file_is_no_record_and_nothing_is_pending() {
    let state = ShimState {
        loaded: Loaded::Absent,
        now: at(5),
        disabled: false,
    };

    assert!(!state.restart_pending(true));
    assert!(!state.restart_pending(false));
}

#[skuld::test]
fn a_stop_in_the_delay_after_a_failure_finds_the_restart_pending() {
    // P2/P4 baseline: failed at 1000 with a 1000 ms action; stopped 200 ms later.
    let state = shim_state(failed(1000, RestartDue::After(1000)), 1200);

    assert!(state.restart_pending(true));
}

#[skuld::test]
fn a_manual_start_in_the_delay_does_not_hide_the_restart() {
    // P4/P14b: the service was started by hand and is running, the record is the earlier failure's,
    // and the queued restart still fires after a stop. Status reads as a clean running service.
    let state = shim_state(failed(1000, RestartDue::After(6000)), 1500);

    assert!(state.restart_pending(false));
}

#[skuld::test]
fn a_service_still_running_when_the_delay_and_margin_are_over_has_nothing_pending() {
    // P14c: the entry expires silently when the service is not stopped at the deadline.
    let due = 1000 + 6000 + FIRE_MARGIN_MS;

    assert!(shim_state(failed(1000, RestartDue::After(6000)), due - 1).restart_pending(false));
    assert!(!shim_state(failed(1000, RestartDue::After(6000)), due).restart_pending(false));
}

#[skuld::test]
fn a_service_still_stopped_with_a_failure_after_the_deadline_is_pending() {
    // SCM has not fired yet (it lags the clock), or is about to.
    let state = shim_state(
        failed(1000, RestartDue::After(1000)),
        1000 + 1000 + FIRE_MARGIN_MS + 5000,
    );

    assert!(state.restart_pending(true));
    assert!(!state.restart_pending(false));
}

#[skuld::test]
fn a_failure_recorded_under_no_restart_policy_queued_nothing_whatever_policy_is_added_later() {
    // P14d: actions added after the failure queue nothing; the record says no restart was due.
    let state = shim_state(failed(1000, RestartDue::Never), 1100);

    assert!(!state.restart_pending(true));
    assert!(!shim_state(failed(1000, RestartDue::Never), 900_000).restart_pending(true));
}

#[skuld::test]
fn back_to_back_failures_leave_the_newest_one_pending() {
    // P14f: the restarted run fails at once; its record replaces the first, and the second restart
    // is queued from the second failure's time.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    note_failure(
        &path,
        FailureRecord {
            boot: BOOT,
            at_ms: 1000,
            restart: RestartDue::After(1000),
        },
    )
    .expect("first failure");
    note_failure(
        &path,
        FailureRecord {
            boot: BOOT,
            at_ms: 2010,
            restart: RestartDue::After(1000),
        },
    )
    .expect("second failure");

    let state = ShimState {
        loaded: read(&path).expect("read"),
        now: at(2010 + 1000 + FIRE_MARGIN_MS - 1),
        disabled: false,
    };

    // Past the first failure's deadline, inside the second's.
    assert!(state.restart_pending(true));
}

#[skuld::test]
fn a_disabled_service_has_no_restart_pending() {
    // P12b/c: SCM's attempt fails with ERROR_SERVICE_DISABLED and leaves the status untouched, so a
    // stopped service with a failure would otherwise read as pending forever.
    let late = ShimState {
        disabled: true,
        ..shim_state(failed(1000, RestartDue::After(1000)), 900_000)
    };
    let early = ShimState {
        disabled: true,
        ..shim_state(failed(1000, RestartDue::After(1000)), 1100)
    };

    assert!(!late.restart_pending(true));
    assert!(!early.restart_pending(true));
}

#[skuld::test]
fn a_record_from_an_earlier_boot_is_stale() {
    let state = ShimState {
        loaded: Loaded::Record(failed(1000, RestartDue::Unknown)),
        now: Clock {
            boot: OTHER_BOOT,
            ms: 1100,
        },
        disabled: false,
    };

    assert!(!state.restart_pending(true));
}

#[skuld::test]
fn without_a_readable_boot_identity_a_record_from_the_future_is_from_an_earlier_boot() {
    let mut record = failed(500_000, RestartDue::Unknown);
    record.failure.as_mut().expect("failure").boot = None;
    let state = ShimState {
        loaded: Loaded::Record(record.clone()),
        now: Clock { boot: None, ms: 100 },
        disabled: false,
    };
    let same_boot = ShimState {
        loaded: Loaded::Record(record),
        now: Clock {
            boot: None,
            ms: 600_000,
        },
        disabled: false,
    };

    assert!(!state.restart_pending(true), "the tick count went backwards: a reboot");
    assert!(same_boot.restart_pending(true));
}

#[skuld::test]
fn an_unknown_delay_counts_as_pending_however_long_ago() {
    // The shim could not read the failure actions. Pending until the next run starts.
    let state = shim_state(failed(1000, RestartDue::Unknown), 100_000_000);

    assert!(state.restart_pending(true));
    assert!(state.restart_pending(false));
}

#[skuld::test]
fn a_record_that_cannot_be_read_counts_as_an_unknown_delay() {
    let state = ShimState {
        loaded: Loaded::Unreadable("torn".to_string()),
        now: at(100),
        disabled: false,
    };

    assert!(state.restart_pending(true));
    assert!(state.restart_pending(false));
}

#[skuld::test]
fn a_record_with_only_a_survivor_has_no_restart_pending() {
    let mut record = StateRecord::empty();
    record.survivor = Some(this_process());

    assert!(!shim_state(record, 100).restart_pending(true));
}

// The format ==========================================================================================================

#[skuld::test]
fn a_record_round_trips_through_its_text() {
    let mut record = failed(123_456, RestartDue::After(2500));
    record.survivor = Some(this_process());

    let decoded = StateRecord::decode(&record.encode()).expect("decode");

    assert_eq!(decoded, record);
}

#[skuld::test]
fn every_restart_due_round_trips() {
    for restart in [
        RestartDue::Never,
        RestartDue::After(0),
        RestartDue::After(u64::MAX),
        RestartDue::Unknown,
    ] {
        let record = failed(1, restart);

        assert_eq!(StateRecord::decode(&record.encode()).expect("decode"), record);
    }
}

#[skuld::test]
fn an_empty_file_is_an_empty_record() {
    assert_eq!(StateRecord::decode("").expect("decode"), StateRecord::empty());
    assert_eq!(StateRecord::decode("  \r\n").expect("decode"), StateRecord::empty());
}

#[skuld::test]
fn a_record_of_another_version_or_garbage_does_not_decode() {
    let future = r#"{"v":2,"failure":null,"survivor":null}"#;

    assert!(StateRecord::decode(future).is_err());
    assert!(StateRecord::decode("{not json").is_err());
    assert!(StateRecord::decode(r#"{"failure":null}"#).is_err(), "no version");
}

#[skuld::test]
fn the_empty_record_names_the_version_this_build_writes() {
    assert_eq!(StateRecord::empty().v, VERSION);
    assert!(StateRecord::empty().encode().contains(&format!("\"v\":{VERSION}")));
}

// The file ============================================================================================================

#[skuld::test]
fn a_missing_file_reads_as_absent() {
    let dir = tempfile::tempdir().expect("temp dir");

    assert!(matches!(read(&dir.path().join("svc")), Ok(Loaded::Absent)));
}

#[skuld::test]
fn a_written_record_reads_back_and_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let record = failed(42, RestartDue::After(7));

    write(&path, &record).expect("write");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r == record));
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("svc")], "{names:?}");
}

#[skuld::test]
fn a_write_replaces_the_previous_record_whole() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let mut long = failed(42, RestartDue::After(7));
    long.survivor = Some(this_process());
    write(&path, &long).expect("first write");
    let short = StateRecord::empty();

    write(&path, &short).expect("second write");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r == short));
}

#[skuld::test]
fn a_file_that_does_not_decode_reads_as_unreadable_with_the_reason() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    std::fs::write(&path, "{not json").expect("write garbage");

    assert!(matches!(read(&path), Ok(Loaded::Unreadable(why)) if !why.is_empty()));
}

#[skuld::test]
fn remove_deletes_the_file_and_a_leftover_temporary_and_tolerates_neither() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &StateRecord::empty()).expect("write");
    std::fs::write(temporary(&path), "x").expect("leftover temporary");

    remove(&path).expect("remove");
    remove(&path).expect("removing nothing is fine");

    assert!(!path.exists());
    assert!(!temporary(&path).exists());
}

#[skuld::test]
fn the_temporary_name_cannot_be_another_daemon_id() {
    // Ids match ^[A-Za-z0-9._-]{1,80}$, so a name with `~` is never one, and `svc.tmp` is.
    let temp = temporary(std::path::Path::new("dir/svc.tmp"));

    assert!(temp.file_name().expect("name").to_string_lossy().contains('~'));
}

// A run starting ======================================================================================================

#[skuld::test]
fn a_run_starting_before_the_deadline_keeps_the_failure_the_restart_is_queued_for() {
    // A manual start in the delay: the entry is live.
    let mut record = failed(1000, RestartDue::After(6000));

    assert!(!record.after_start(at(1500)));

    assert!(record.failure.is_some());
}

#[skuld::test]
fn a_run_starting_at_the_deadline_has_consumed_the_failure() {
    // SCM's restart fires no earlier than the failure plus the delay, so a run that starts then
    // is the restart (or a start after it): the entry is spent.
    let mut record = failed(1000, RestartDue::After(6000));

    assert!(record.after_start(at(7000)));

    assert_eq!(record, StateRecord::empty());
}

#[skuld::test]
fn a_run_starting_clears_failures_that_name_no_pending_restart() {
    for failure in [failed(1000, RestartDue::Never), failed(1000, RestartDue::Unknown)] {
        let mut record = failure;

        assert!(record.after_start(at(1001)));

        assert_eq!(record.failure, None);
    }
}

#[skuld::test]
fn a_run_starting_clears_a_failure_from_an_earlier_boot() {
    let mut record = failed(1000, RestartDue::After(u64::MAX / 2));

    let changed = record.after_start(Clock {
        boot: OTHER_BOOT,
        ms: 10,
    });

    assert!(changed);
    assert_eq!(record.failure, None);
}

#[skuld::test]
fn a_run_starting_keeps_the_survivor_record_for_the_liveness_check_to_judge() {
    let mut record = StateRecord::empty();
    record.survivor = Some(this_process());

    assert!(!record.after_start(at(1)));

    assert!(record.survivor.is_some());
}

fn begin(record: &StateRecord, now_ms: u64) -> (Begin, Loaded, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, record).expect("write");
    let outcome = begin_run(&path, at(now_ms)).expect("begin_run");
    let after = read(&path).expect("read");
    (outcome, after, path, dir)
}

#[skuld::test]
fn a_start_with_no_file_proceeds_and_creates_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");

    assert!(matches!(begin_run(&path, at(1)), Ok(Begin::Proceed)));

    assert!(!path.exists());
}

#[skuld::test]
fn a_start_over_a_live_survivor_is_blocked_and_leaves_the_record_alone() {
    let mut record = failed(1000, RestartDue::After(6000));
    record.survivor = Some(this_process());

    let (outcome, after, _path, _dir) = begin(&record, 1500);

    match outcome {
        Begin::Blocked { pid, survivor } => {
            assert_eq!(pid, std::process::id());
            assert_eq!(survivor, Survivor::Alive);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(after, Loaded::Record(r) if r == record));
}

#[skuld::test]
fn a_start_over_a_survivor_that_has_gone_clears_it_and_proceeds() {
    let mut gone = this_process();
    gone.token += 1;
    let mut record = StateRecord::empty();
    record.survivor = Some(gone);

    let (outcome, after, _path, _dir) = begin(&record, 1);

    assert!(matches!(outcome, Begin::Proceed), "{outcome:?}");
    assert!(matches!(after, Loaded::Record(r) if r.survivor.is_none()));
}

#[skuld::test]
fn a_start_after_the_deadline_writes_back_a_record_without_the_spent_failure() {
    let record = failed(1000, RestartDue::After(1000));

    let (outcome, after, _path, _dir) = begin(&record, 2000);

    assert!(matches!(outcome, Begin::Proceed));
    assert!(matches!(after, Loaded::Record(r) if r == StateRecord::empty()));
}

#[skuld::test]
fn a_start_before_the_deadline_leaves_the_file_as_it_was() {
    let record = failed(1000, RestartDue::After(6000));

    let (outcome, after, _path, _dir) = begin(&record, 1500);

    assert!(matches!(outcome, Begin::Proceed));
    assert!(matches!(after, Loaded::Record(r) if r == record));
}

#[skuld::test]
fn a_start_over_an_unreadable_file_replaces_it_and_says_why() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    std::fs::write(&path, "{not json").expect("write garbage");

    let outcome = begin_run(&path, at(1)).expect("begin_run");

    assert!(matches!(&outcome, Begin::Reset(why) if !why.is_empty()), "{outcome:?}");
    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r == StateRecord::empty()));
}

// Recording ===========================================================================================================

#[skuld::test]
fn a_failure_is_recorded_beside_a_survivor_without_dropping_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    note_survivor(&path, &this_process()).expect("survivor");

    note_failure(
        &path,
        FailureRecord {
            boot: BOOT,
            at_ms: 9,
            restart: RestartDue::Unknown,
        },
    )
    .expect("failure");

    let Loaded::Record(record) = read(&path).expect("read") else {
        panic!("a record");
    };
    assert!(record.survivor.is_some());
    assert_eq!(record.failure.expect("failure").at_ms, 9);
}

#[skuld::test]
fn a_failure_is_recorded_over_an_unreadable_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    std::fs::write(&path, "{not json").expect("write garbage");

    note_failure(
        &path,
        FailureRecord {
            boot: BOOT,
            at_ms: 9,
            restart: RestartDue::Never,
        },
    )
    .expect("failure");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r.failure.is_some()));
}

// Survivors ===========================================================================================================

#[skuld::test]
fn a_record_naming_a_running_process_is_a_live_survivor() {
    assert_eq!(survivor_of(&this_process()), Survivor::Alive);
}

#[skuld::test]
fn a_record_naming_the_pid_of_a_different_process_is_gone() {
    // The same pid with another creation time is a recycled pid, not the survivor.
    let mut record = this_process();
    record.token += 1;

    assert_eq!(survivor_of(&record), Survivor::Gone);
}

#[skuld::test]
fn a_record_from_another_platform_names_nothing_here() {
    let mut record = this_process();
    record.platform = Platform::Other("plan9".to_string());

    assert_eq!(survivor_of(&record), Survivor::Gone);
}

#[skuld::test]
fn only_a_survivor_that_lives_or_cannot_be_told_blocks() {
    let mut record = StateRecord::empty();
    assert_eq!(blocking_survivor(&Loaded::Record(record.clone())), None);
    assert_eq!(blocking_survivor(&Loaded::Absent), None);

    record.survivor = Some(this_process());
    assert_eq!(
        blocking_survivor(&Loaded::Record(record.clone())),
        Some((std::process::id(), Survivor::Alive))
    );

    let mut gone = this_process();
    gone.token += 1;
    record.survivor = Some(gone);
    assert_eq!(blocking_survivor(&Loaded::Record(record)), None);
}

// The restart a failure queued ========================================================================================

#[skuld::test]
fn a_restart_action_with_the_flag_set_is_due_after_its_delay() {
    assert_eq!(
        restart_due_from(true, &[Duration::from_millis(1500)]),
        RestartDue::After(1500)
    );
}

#[skuld::test]
fn the_longest_restart_delay_bounds_whichever_action_a_failure_count_selects() {
    let delays = [Duration::from_secs(1), Duration::from_secs(60), Duration::from_secs(5)];

    assert_eq!(restart_due_from(true, &delays), RestartDue::After(60_000));
}

#[skuld::test]
fn without_the_flag_a_non_crash_failure_queues_nothing() {
    assert_eq!(restart_due_from(false, &[Duration::from_secs(1)]), RestartDue::Never);
}

#[skuld::test]
fn without_a_restart_action_nothing_is_queued() {
    assert_eq!(restart_due_from(true, &[]), RestartDue::Never);
}
