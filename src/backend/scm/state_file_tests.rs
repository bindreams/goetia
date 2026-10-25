use std::time::Duration;

use cosca::identity::{Platform, ProcessId};

use super::*;

const BOOT: u64 = 11;
const OTHER_BOOT: u64 = 12;

fn at(ms: u64) -> Clock {
    Clock { boot: BOOT, ms }
}

fn failed(at_ms: u64, restart: RestartDue) -> StateRecord {
    let mut record = StateRecord::empty();
    record.failure = Some(FailureRecord::new(BOOT, at_ms, restart));
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
    note_failure(&path, FailureRecord::new(BOOT, 1000, RestartDue::After(1000))).expect("first failure");
    note_failure(&path, FailureRecord::new(BOOT, 2010, RestartDue::After(1000))).expect("second failure");

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
fn a_record_from_an_earlier_boot_is_stale_whatever_the_tick_count_says() {
    // A one-hour delay recorded at one hour of uptime, read after a reboot at 1 h 2 min: the tick
    // count alone cannot tell this boot's hour from the last one's.
    let one_hour = 3_600_000;
    let record = ShimState {
        loaded: Loaded::Record(failed(one_hour, RestartDue::After(one_hour))),
        now: Clock {
            boot: OTHER_BOOT,
            ms: one_hour + 120_000,
        },
        disabled: false,
    };

    assert!(!record.restart_pending(true));
    assert!(!record.restart_pending(false));
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

#[skuld::test]
fn only_a_decoded_record_with_a_survivor_names_one() {
    let mut with = StateRecord::empty();
    with.survivor = Some(this_process());

    assert!(shim_state(with, 100).names_a_survivor());
    assert!(!shim_state(StateRecord::empty(), 100).names_a_survivor());
    for loaded in [Loaded::Absent, Loaded::Unreadable("garbled".to_string())] {
        let state = ShimState {
            loaded,
            now: at(100),
            disabled: false,
        };
        assert!(!state.names_a_survivor());
    }
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

/// The names in `dir`, sorted.
fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

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
    assert_eq!(names_in(dir.path()), ["lock", "svc"], "no temporary is left behind");
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
fn remove_deletes_the_record_its_lock_and_a_leftover_temporary_and_the_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let service = dir.path().join("svc-x");
    std::fs::create_dir(&service).expect("the service directory");
    let path = service.join(RECORD);
    write(&path, &StateRecord::empty()).expect("write");
    std::fs::write(service.join("1-abc.tmp"), "x").expect("leftover temporary");

    remove(&path).expect("remove");
    remove(&path).expect("removing nothing is fine");

    assert!(!service.exists());
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
fn a_run_starting_clears_a_failure_that_queued_no_restart() {
    let mut record = failed(1000, RestartDue::Never);

    assert!(record.after_start(at(1001)));

    assert_eq!(record.failure, None);
}

#[skuld::test]
fn a_run_starting_does_not_spend_a_failure_whose_delay_nobody_knows() {
    // A start by hand does not cancel the entry SCM queued (P14b/c): with no due time to compare
    // the start against, the restart may still be coming, so the failure stands until the boot ends.
    let mut record = failed(1000, RestartDue::Unknown);

    assert!(!record.after_start(at(1001)));
    assert!(!record.after_start(at(100_000_000)));

    assert!(record.failure.is_some());
}

#[skuld::test]
fn a_run_starting_after_a_reboot_clears_a_failure_whose_delay_nobody_knew() {
    let mut record = failed(1000, RestartDue::Unknown);

    assert!(record.after_start(Clock {
        boot: OTHER_BOOT,
        ms: 5
    }));

    assert_eq!(record.failure, None);
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

    note_failure(&path, FailureRecord::new(BOOT, 9, RestartDue::Unknown)).expect("failure");

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

    note_failure(&path, FailureRecord::new(BOOT, 9, RestartDue::Never)).expect("failure");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r.failure.is_some()));
}

// A failure noted around an act =======================================================================================

/// A failure at `ms` that the failure actions, read now, say queues a restart after 5 s.
fn noted(ms: u64) -> NewFailure {
    NewFailure {
        now: at(ms),
        cause: Cause::Reported,
        live: Some(RestartDue::After(5000)),
    }
}

fn failure_at(ms: u64) -> FailureRecord {
    FailureRecord::new(BOOT, ms, RestartDue::After(5000))
}

#[skuld::test]
fn the_failure_is_on_disk_before_the_act_runs_and_stays_when_the_act_took_effect() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &StateRecord::empty()).expect("write");

    let seen = noting_failure_around(
        &path,
        &noted(9),
        || Ok(read(&path).expect("read during the act")),
        |_| true,
    )
    .expect("the act");

    assert!(matches!(seen, Loaded::Record(r) if r.failure == Some(failure_at(9))));
    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r.failure == Some(failure_at(9))));
}

#[skuld::test]
fn the_failure_is_noted_beside_a_survivor_without_dropping_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let mut before = StateRecord::empty();
    before.survivor = Some(this_process());
    write(&path, &before).expect("write");

    noting_failure_around(&path, &noted(9), || Ok(()), |_| true).expect("the act");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r.survivor == before.survivor && r.failure.is_some()));
}

#[skuld::test]
fn an_act_that_failed_leaves_the_previous_record_in_place() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let before = failed(1, RestartDue::After(100));
    write(&path, &before).expect("write");

    let result: io::Result<()> =
        noting_failure_around(&path, &noted(9), || Err(io::Error::other("refused")), |()| true);

    assert!(result.is_err());
    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r == before));
}

#[skuld::test]
fn an_act_that_did_not_take_effect_leaves_the_previous_record_in_place() {
    // The process was already on its way out: nothing was failed by this act, and the record
    // would only make a stop wait for a restart that nobody queued.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let mut before = StateRecord::empty();
    before.survivor = Some(this_process());
    write(&path, &before).expect("write");

    noting_failure_around(&path, &noted(9), || Ok(false), |&took_effect| took_effect).expect("the act");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r == before));
}

#[skuld::test]
fn an_unreadable_file_is_put_back_as_an_empty_record_when_the_act_did_not_take_effect() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    std::fs::write(&path, "{not json").expect("write garbage");

    noting_failure_around(&path, &noted(9), || Ok(()), |_| false).expect("the act");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r == StateRecord::empty()));
}

#[skuld::test]
fn a_service_with_no_state_file_gets_none_and_the_act_still_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");

    let ran = noting_failure_around(&path, &noted(9), || Ok("ran"), |_| true).expect("the act");

    assert_eq!(ran, "ran");
    assert!(!path.exists());
}

#[skuld::test]
fn a_failure_that_cannot_be_recorded_stops_the_act_and_says_so() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    // A directory where the record should be: it can be neither read nor replaced.
    std::fs::create_dir(&path).expect("block the record");
    let mut ran = false;

    let result = noting_failure_around(
        &path,
        &noted(9),
        || {
            ran = true;
            Ok(())
        },
        |_| true,
    );

    let message = result.expect_err("the record could not be written").to_string();
    assert!(!ran, "an act that is not recorded must not run");
    assert!(message.contains("could not record the failure"), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");
}

// Releasing a file ====================================================================================================

#[skuld::test]
fn releasing_removes_a_file_with_no_survivor_and_its_temporary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &failed(1, RestartDue::After(100))).expect("write");
    std::fs::write(dir.path().join("1-abc.tmp"), "x").expect("leftover temporary");

    release(&path).expect("release");
    release(&path).expect("releasing nothing is fine");

    assert!(!dir.path().exists(), "the service's directory goes with its record");
}

#[skuld::test]
fn releasing_removes_a_file_whose_survivor_has_gone_or_that_does_not_decode() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let mut gone = this_process();
    gone.token += 1;
    let mut record = StateRecord::empty();
    record.survivor = Some(gone);
    write(&path, &record).expect("write");
    release(&path).expect("release");
    assert!(!path.exists());

    std::fs::create_dir_all(dir.path()).expect("the service directory again");
    std::fs::write(&path, "{not json").expect("write garbage");
    release(&path).expect("release");
    assert!(!path.exists());
}

#[skuld::test]
fn releasing_keeps_a_live_survivor_and_drops_only_the_failure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let mut record = failed(1, RestartDue::After(100));
    record.survivor = Some(this_process());
    write(&path, &record).expect("write");

    release(&path).expect("release");

    let Loaded::Record(kept) = read(&path).expect("read") else {
        panic!("the file must stay while its survivor runs");
    };
    assert_eq!(kept.survivor, record.survivor);
    assert_eq!(kept.failure, None, "the service is gone, and so is its pending restart");
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

// The directory name of an id =========================================================================================

/// Windows device names: opening one under any extension reaches the device, not a file.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT0",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$", "CONOUT$",
];

/// Ids that are valid (`1..=80` of `[A-Za-z0-9._-]`) and that Windows would read as something else
/// than a plain file name.
fn awkward_ids() -> Vec<String> {
    let mut ids: Vec<String> = RESERVED.iter().map(|name| name.to_lowercase()).collect();
    ids.extend(RESERVED.iter().map(|name| format!("{name}.txt")));
    for id in [
        ".",
        "..",
        "...",
        "a",
        "a.",
        "a..",
        "a. ",
        ".a",
        "a.b",
        "a-b",
        "a_b",
        "nul.",
        "x".repeat(80).as_str(),
    ] {
        ids.push(id.to_string());
    }
    ids.push(format!("{}.", "x".repeat(79)));
    ids
}

#[skuld::test]
fn every_id_gets_a_directory_name_that_windows_reads_as_that_one_plain_name() {
    for id in awkward_ids() {
        let name = dir_name(&id);

        assert!(name != "." && name != "..", "{id:?} -> {name:?}");
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'%')),
            "{id:?} -> {name:?}: a character Windows may read as a separator, stream or dot"
        );
        let stem = name.split('.').next().unwrap_or_default().to_uppercase();
        assert!(
            !RESERVED.contains(&stem.as_str()),
            "{id:?} -> {name:?} is a device name"
        );
        assert!(name.len() <= 255, "{id:?} -> {name:?}");
    }
}

#[skuld::test]
fn two_ids_never_share_a_directory_name() {
    // `a` and `a.` are two SCM names that Windows reads as one path.
    let ids = awkward_ids();
    let mut seen = std::collections::HashMap::new();
    for id in &ids {
        if let Some(other) = seen.insert(dir_name(id).to_lowercase(), id) {
            panic!("{id:?} and {other:?} share {:?}", dir_name(id));
        }
    }
}

// Writers that do not meet ============================================================================================

#[skuld::test]
fn updaters_in_different_threads_lose_no_update_and_readers_never_see_a_torn_record() {
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 25;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let writing = std::sync::atomic::AtomicBool::new(true);
    /// Ends the reader's loop on every way out, a failed writer's panic included.
    struct Done<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    std::thread::scope(|scope| {
        let _done = Done(&writing);
        let reader = scope.spawn(|| {
            while writing.load(std::sync::atomic::Ordering::Acquire) {
                let loaded = read(&path).expect("read");
                assert!(matches!(loaded, Loaded::Record(_) | Loaded::Absent), "{loaded:?}");
            }
        });
        let writers: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..ROUNDS {
                        update(&path, |record| {
                            let n = record.failure.as_ref().map_or(0, |f| f.at_ms);
                            record.failure = Some(failure_at(n + 1));
                        })
                        .expect("update");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer");
        }
        drop(_done);
        reader.join().expect("the reader");
    });

    let Loaded::Record(record) = read(&path).expect("read") else {
        panic!("a record");
    };
    assert_eq!(record.failure.expect("failure").at_ms, THREADS * ROUNDS);
    assert_eq!(names_in(dir.path()), ["lock", "svc"], "no temporary is left behind");
}

#[skuld::test]
fn a_read_holds_the_lock_shared_and_an_update_holds_it_exclusive() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &StateRecord::empty()).expect("write");
    let lock = lock_path(&path);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let _hook = test_hook::on_locked({
        let seen = seen.clone();
        move |exclusive| {
            let other = std::fs::OpenOptions::new()
                .read(true)
                .open(&lock)
                .expect("a second handle");
            seen.borrow_mut()
                .push((exclusive, other.try_lock().is_err(), other.try_lock_shared().is_err()));
        }
    });

    read(&path).expect("read");
    update(&path, |_| ()).expect("update");

    // While a read is in flight nobody can write, but others can read; while an update is, nobody
    // can do either.
    assert_eq!(*seen.borrow(), [(false, true, false), (true, true, true)]);
}

#[skuld::test]
fn a_temporary_left_by_a_writer_that_died_is_removed_by_the_next_update() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &StateRecord::empty()).expect("write");
    std::fs::write(dir.path().join("4242-dead.tmp"), r#"{"v":1,"#).expect("a torn temporary");

    update(&path, |_| ()).expect("update");

    assert_eq!(names_in(dir.path()), ["lock", "svc"]);
}

#[skuld::test]
fn an_update_of_a_directory_that_does_not_exist_fails_without_creating_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("no-such-service").join("svc");

    assert!(update(&path, |_| ()).is_err());

    assert!(!path.parent().expect("parent").exists());
}

// The failure noted around an act, with other writers =================================================================

/// A failure from a process other than this one, at the same time and with the same delay.
fn failure_from_elsewhere(ms: u64) -> FailureRecord {
    FailureRecord {
        by: std::process::id() + 1,
        ..failure_at(ms)
    }
}

#[skuld::test]
fn a_failure_the_shim_wrote_during_the_act_is_not_undone_by_the_restore() {
    // `kill` writes, the shim (failing on its own) writes, `kill`'s terminate finds the process
    // already going and reports it did nothing: only `kill`'s own failure is taken back.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &failed(1, RestartDue::After(100))).expect("write");

    noting_failure_around(
        &path,
        &noted(9),
        || {
            // The act holds no lock: the shim's write has to get through during it.
            update(&path, |record| record.failure = Some(failure_from_elsewhere(9))).expect("the shim's write");
            Ok(false)
        },
        |&took_effect| took_effect,
    )
    .expect("the act");

    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r.failure == Some(failure_from_elsewhere(9))));
}

#[skuld::test]
fn a_restore_puts_back_the_failure_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let before = failed(1, RestartDue::After(100));
    write(&path, &before).expect("write");

    noting_failure_around(
        &path,
        &noted(9),
        || {
            update(&path, |record| record.survivor = Some(this_process())).expect("the shim records a survivor");
            Ok(false)
        },
        |&took_effect| took_effect,
    )
    .expect("the act");

    let Loaded::Record(after) = read(&path).expect("read") else {
        panic!("a record");
    };
    assert_eq!(after.failure, before.failure);
    assert_eq!(after.survivor, Some(this_process()));
}

// The restart a failure queues ========================================================================================

#[skuld::test]
fn a_process_that_ends_without_reporting_is_restarted_whatever_the_flag_says() {
    // `TerminateProcess` of the shim is a crash: SCM runs recovery for it with the flag off too.
    assert_eq!(
        restart_due_after_a_crash(&[Duration::from_millis(1500)]),
        RestartDue::After(1500)
    );
    assert_eq!(
        restart_due_after_a_crash(&[Duration::from_secs(1), Duration::from_secs(60)]),
        RestartDue::After(60_000)
    );
    assert_eq!(restart_due_after_a_crash(&[]), RestartDue::Never);
}

#[skuld::test]
fn the_actions_install_configured_give_the_restart_for_a_report_and_for_a_crash() {
    let with_flag = Actions::new(true, &[Duration::from_secs(2)]);
    let without_flag = Actions::new(false, &[Duration::from_secs(2)]);
    let no_restart = Actions::new(true, &[]);

    assert_eq!(with_flag.after(Cause::Reported), RestartDue::After(2000));
    assert_eq!(with_flag.after(Cause::Crashed), RestartDue::After(2000));
    assert_eq!(without_flag.after(Cause::Reported), RestartDue::Never);
    assert_eq!(without_flag.after(Cause::Crashed), RestartDue::After(2000));
    assert_eq!(no_restart.after(Cause::Reported), RestartDue::Never);
    assert_eq!(no_restart.after(Cause::Crashed), RestartDue::Never);
}

#[skuld::test]
fn a_record_keeps_the_actions_install_configured_through_its_text() {
    let mut record = failed(5, RestartDue::Never);
    record.configured = Some(Actions::new(true, &[Duration::from_millis(30)]));

    assert_eq!(StateRecord::decode(&record.encode()).expect("decode"), record);
    // A record written before the actions were kept has none.
    let older = r#"{"v":1,"failure":null,"survivor":null}"#;
    assert_eq!(StateRecord::decode(older).expect("decode").configured, None);
}

#[skuld::test]
fn configuring_keeps_the_rest_of_the_record_and_makes_one_where_there_is_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    let actions = Actions::new(true, &[Duration::from_secs(1)]);

    configure(&path, actions.clone()).expect("configure");
    assert!(
        matches!(read(&path), Ok(Loaded::Record(r)) if r.configured == Some(actions.clone()) && r.failure.is_none())
    );

    let mut busy = failed(1, RestartDue::After(100));
    busy.survivor = Some(this_process());
    write(&path, &busy).expect("write");
    configure(&path, Actions::default()).expect("configure again");

    let Loaded::Record(after) = read(&path).expect("read") else {
        panic!("a record");
    };
    assert_eq!((after.failure, after.survivor), (busy.failure, busy.survivor));
    assert_eq!(
        after.configured,
        Some(Actions::default()),
        "the actions are replaced, and only they"
    );
}

#[skuld::test]
fn a_failure_noted_with_unreadable_actions_takes_the_restart_install_configured() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    configure(&path, Actions::new(true, &[Duration::from_secs(3)])).expect("configure");

    let noted = note_failure_now(
        &path,
        &NewFailure {
            now: at(7),
            cause: Cause::Reported,
            live: None,
        },
    )
    .expect("note");

    assert_eq!(noted, FailureRecord::new(BOOT, 7, RestartDue::After(3000)));
    assert!(matches!(read(&path), Ok(Loaded::Record(r)) if r.failure == Some(noted.clone())));
}

#[skuld::test]
fn a_failure_noted_with_nothing_to_go_by_is_of_unknown_delay() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    write(&path, &StateRecord::empty()).expect("write");

    let noted = note_failure_now(
        &path,
        &NewFailure {
            now: at(7),
            cause: Cause::Reported,
            live: None,
        },
    )
    .expect("note");

    assert_eq!(noted.restart, RestartDue::Unknown);
}

#[skuld::test]
fn a_failure_noted_with_readable_actions_takes_them_over_what_install_configured() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    configure(&path, Actions::new(true, &[Duration::from_secs(3)])).expect("configure");

    let noted = note_failure_now(
        &path,
        &NewFailure {
            now: at(7),
            cause: Cause::Reported,
            live: Some(RestartDue::After(9)),
        },
    )
    .expect("note");

    assert_eq!(noted.restart, RestartDue::After(9));
}

#[skuld::test]
fn a_kill_with_unreadable_actions_takes_the_crash_restart_install_configured() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("svc");
    // Set by hand to off: a report would queue nothing, a crash does.
    configure(&path, Actions::new(false, &[Duration::from_secs(3)])).expect("configure");

    noting_failure_around(
        &path,
        &NewFailure {
            now: at(7),
            cause: Cause::Crashed,
            live: None,
        },
        || Ok(()),
        |_| true,
    )
    .expect("the act");

    assert!(
        matches!(read(&path), Ok(Loaded::Record(r)) if r.failure.as_ref().map(|f| f.restart) == Some(RestartDue::After(3000)))
    );
}

#[skuld::test]
fn a_start_over_an_unknown_failure_of_this_boot_leaves_it_in_the_file() {
    let (outcome, after, _path, _dir) = begin(&failed(1000, RestartDue::Unknown), 5000);

    assert!(matches!(outcome, Begin::Proceed));
    assert!(matches!(after, Loaded::Record(r) if r.failure.is_some()));
}
