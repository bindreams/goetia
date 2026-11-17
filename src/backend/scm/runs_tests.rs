//! `Runs` over a temporary directory. Every test makes its own, removed when the test ends.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use tempfile::TempDir;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, READ_CONTROL, WRITE_DAC,
};

use super::*;
use crate::backend::scm::shim::acl::{Ace, CheckPermissions, ObjectSecurity, read_object};
use crate::backend::scm::sid::{administrators, sid_from_parts, system};
use crate::backend::scm::test_support::delete_pending;
use crate::elevation::test_support::process_user_sid;
use crate::testing::groups::{Group, scm_group};

const FULL: u32 = 0x1F_01FF;
const MODIFY: u32 = 0x13_01BF;
const INHERITED_BY_FILES_AND_FOLDERS: u8 = 0x03;
const OBJECT_INHERIT: u8 = 0x01;
const INHERIT_ONLY: u8 = 0x08;
const ADD_SUBDIRECTORY: u32 = 0x4;
const DELETE_CHILD: u32 = 0x40;

fn id() -> Id {
    Id::try_from("frpc").unwrap()
}

fn instance(n: u8) -> InstanceId {
    InstanceId::parse(&format!("{n:032x}")).unwrap()
}

fn record(n: u8, started: u64) -> RunRecord {
    RunRecord {
        instance: instance(n),
        started,
        outcome: RunOutcome::Exited(u32::from(n)),
    }
}

/// The task's directory made plainly in `dir`, then given the permissions an install gives it for
/// `principal`. A directory that exists has only its permissions set, which needs no elevation; making
/// it with Administrators as its owner does, and is the tests of the `scm` group.
fn made_for(runs: &Runs, principal: &crate::backend::scm::sid::Sid) {
    fs::create_dir_all(&runs.dir).unwrap();
    runs.ensure(principal).unwrap();
}

/// A task's directory, made for the account this test runs as, in a directory that is removed with
/// `dir` (after `runs` and anything declared later).
fn fixture() -> (TempDir, Runs) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    made_for(&runs, &process_user_sid());
    (dir, runs)
}

/// What is in the task's directory, by file name, in order.
fn names(runs: &Runs) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&runs.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn file_of(instance: &InstanceId) -> String {
    format!("{}.json", instance.hex())
}

fn instances_kept(runs: &Runs, wanted: &[InstanceId]) -> Vec<bool> {
    wanted.iter().map(|id| runs.read(id).unwrap().is_some()).collect()
}

// write and read ======================================================================================================

/// The rename names the record whole for every length of its path: it once left garbage after the name
/// (or was refused) for one length in eight, and the tests' own directory fixes the length of the rest.
/// Eight ids in a row reach every length modulo eight, in any directory.
#[skuld::test]
fn a_record_is_whole_whatever_the_length_of_its_path() {
    for length in 1..=8 {
        let dir = tempfile::tempdir().unwrap();
        let id = Id::try_from("x".repeat(length).as_str()).unwrap();
        let runs = Runs::of(dir.path(), &id);
        made_for(&runs, &process_user_sid());
        let written = record(1, 5);

        runs.write(&written).unwrap();

        assert_eq!(
            runs.read(&written.instance).unwrap(),
            Some(written),
            "an id of {length}"
        );
    }
}

#[skuld::test]
fn a_record_round_trips_by_its_instance() {
    let (_dir, runs) = fixture();
    let outcomes = [
        RunOutcome::Exited(0),
        RunOutcome::Exited(u32::MAX),
        RunOutcome::Stopped,
        RunOutcome::IncompleteStop,
    ];

    for (n, outcome) in (1u8..).zip(outcomes) {
        let written = RunRecord {
            outcome,
            ..record(n, 0x01DB_0000_0000_0000 + u64::from(n))
        };
        runs.write(&written).unwrap();
        assert_eq!(runs.read(&written.instance).unwrap(), Some(written));
    }
    assert_eq!(
        runs.read(&instance(99)).unwrap(),
        None,
        "another instance has no record"
    );
}

#[skuld::test]
fn a_second_write_for_an_instance_replaces_its_record() {
    let (_dir, runs) = fixture();
    let first = RunRecord {
        outcome: RunOutcome::Stopped,
        ..record(1, 5)
    };
    let second = RunRecord {
        outcome: RunOutcome::IncompleteStop,
        ..first.clone()
    };

    runs.write(&first).unwrap();
    runs.write(&second).unwrap();

    assert_eq!(runs.read(&first.instance).unwrap(), Some(second));
    assert_eq!(names(&runs), [file_of(&first.instance)]);
}

#[skuld::test]
fn a_write_leaves_no_partial_record_and_removes_a_leftover_temp() {
    let (_dir, runs) = fixture();
    let own = record(1, 1);
    // What a write interrupted before its rename leaves: for this instance and for another.
    fs::write(runs.dir.join(format!("{}.json.tmp", own.instance.hex())), "{\"sta").unwrap();
    fs::write(runs.dir.join(format!("{}.json.tmp", instance(2).hex())), "{\"sta").unwrap();

    runs.write(&own).unwrap();

    assert_eq!(names(&runs), [file_of(&own.instance)]);
    assert_eq!(runs.read(&own.instance).unwrap(), Some(own));
}

// pruning =============================================================================================================

/// A prune whose first listing is taken, then a pin and a record that is newer than it appear, as
/// they can between any two steps of a prune. Listing records before pins is what makes the pin
/// visible whenever a record newer than it is: a prune that read the pins first would not see this
/// pin, and would delete the record the pin is waiting for.
#[skuld::test]
fn a_prune_reads_the_records_before_the_pins() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let calls = std::cell::Cell::new(0);
    let pin = std::cell::RefCell::new(None);

    runs.prune_listing(&instance(1), || {
        let listing = runs.entries();
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            *pin.borrow_mut() = Some(runs.pin().unwrap());
            runs.write(&record(2, 2)).unwrap();
        }
        listing
    });

    assert_eq!(calls.get(), 2, "the records, then the pins");
    assert!(runs.read(&instance(2)).unwrap().is_some());
}

#[skuld::test]
fn a_write_in_flight_keeps_its_temp_file_from_a_prune_and_is_no_problem() {
    let (_dir, runs) = fixture();
    // Another run's write, between creating its temp file and renaming it.
    let in_flight = runs.dir.join(format!("{}.json.tmp", instance(2).hex()));
    let held = files::create_temp(&in_flight).unwrap();

    let problems = runs.write(&record(1, 1)).unwrap();

    assert!(in_flight.exists(), "a prune cannot delete what a write holds");
    assert_eq!(
        problems,
        Vec::<String>::new(),
        "a file that is held is for the next write"
    );
    drop(held);
}

/// A prune that cannot list the directory says so, and does not fail the write.
#[skuld::test]
fn a_listing_that_fails_is_a_problem_of_the_prune() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();

    let problems = runs.prune_listing(&instance(1), || Err(io::Error::other("listing denied")));

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("listing denied"), "{problems:?}");
}

/// A file whose delete is pending (another handle has it open) cannot be opened, deleted or listed as
/// anything but denied: a removal that fails so is told, and the prune goes on to the rest.
#[skuld::test]
fn a_removal_that_fails_is_a_problem_and_the_rest_are_removed() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let stuck = runs.dir.join(file_of(&instance(2)));
    let reader = delete_pending(&stuck);
    let doomed = runs.dir.join(format!("{}.json.tmp", instance(3).hex()));
    fs::write(&doomed, "").unwrap();

    let problems = runs.write(&record(4, 4)).unwrap();

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains(&file_of(&instance(2))), "{problems:?}");
    assert!(!doomed.exists() && runs.read(&instance(1)).unwrap().is_none());
    drop(reader);
}

#[skuld::test]
fn without_a_pin_a_write_keeps_only_its_own_record() {
    let (_dir, runs) = fixture();

    runs.write(&record(1, 1)).unwrap();
    runs.write(&record(2, 2)).unwrap();
    runs.write(&record(3, 3)).unwrap();

    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [false, false, true]
    );
}

#[skuld::test]
fn a_live_pin_stops_all_pruning() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pin = runs.pin().unwrap();
    let before = names(&runs).len();

    runs.write(&record(2, 2)).unwrap();
    runs.write(&record(3, 3)).unwrap();

    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [true, true, true]
    );
    assert_eq!(names(&runs).len(), before + 2, "the pin file is still there");
    drop(pin);
}

#[skuld::test]
fn a_stale_pin_is_deleted_and_does_not_stop_pruning() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    // A pin whose holder died with the machine: no handle holds the file.
    let stale = runs.dir.join("pin-stale.pin");
    fs::write(&stale, "").unwrap();

    runs.write(&record(2, 2)).unwrap();

    assert!(!stale.exists(), "the stale pin is deleted");
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [false, true]);
}

/// A pin that a previous principal's `run` left (a power cut) has entries for that principal alone, since
/// a directory's new permissions are never given to what is in it: the new principal, whose write finds
/// it, cannot open it at all (not even for a delete) and has only what the directory gives, deleting
/// entries. It is no live pin: the write deletes it and prunes, and has nothing to report.
#[skuld::test]
fn a_stale_pin_the_writing_account_has_no_entry_on_is_deleted_and_does_not_stop_pruning() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let stale = runs.dir.join("pin-old.pin");
    fs::write(&stale, "").unwrap();
    Descriptor::from_sddl("D:P(A;;FA;;;SY)(A;;FA;;;BA)")
        .unwrap()
        .set_dacl(&stale)
        .unwrap();

    let problems = runs.write(&record(2, 2)).unwrap();

    assert!(!stale.exists(), "the stale pin is deleted");
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [false, true]);
    assert_eq!(problems, Vec::<String>::new());
}

/// A pin a `run` of another account holds, live, with no entry for this account: after a change of
/// account the new account's shim writes while that `run` waits. The write must not delete the live pin,
/// which the directory's right to delete entries would let it, and must keep the record the `run` waits
/// for.
#[skuld::test]
fn a_live_pin_the_writing_account_has_no_entry_on_is_kept_and_stops_pruning() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let live = runs.dir.join("pin-other.pin");
    let holder = files::pin_options()
        .access_mode(files::PIN_ACCESS | WRITE_DAC)
        .create_new(true)
        .open(&live)
        .unwrap();
    Descriptor::from_sddl("D:P(A;;FA;;;SY)")
        .unwrap()
        .set_dacl_of(&holder)
        .unwrap();

    let problems = runs.write(&record(2, 2)).unwrap();

    assert!(live.exists(), "a live pin is not deleted");
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [true, true]);
    assert_eq!(problems, Vec::<String>::new());
    drop(holder);
    assert!(!live.exists(), "the pin goes when its holder does");
}

/// A pin that cannot be deleted for another reason than a sharing violation (here its delete is pending:
/// Windows reports that as access denied) is told, and is not taken as held: reading it as held would
/// stop every later prune.
#[skuld::test]
fn a_pin_that_cannot_be_deleted_is_a_problem_and_is_taken_as_not_held() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pending = runs.dir.join("pin-odd.pin");
    let reader = delete_pending(&pending);

    let problems = runs.write(&record(2, 2)).unwrap();

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("pin-odd.pin"), "{problems:?}");
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [false, true]);
    drop(reader);
}

/// A stop's hold ([`Runs::hold`], no listing) keeps the records it will read as a `run`'s pin does.
#[skuld::test]
fn a_hold_keeps_the_records_from_every_write_as_a_pin_does() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let hold = runs.hold().unwrap();

    runs.write(&record(2, 2)).unwrap();
    runs.write(&record(3, 3)).unwrap();

    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [true, true, true]
    );
    drop(hold);
}

/// A stale pin beside a live one is told from it by its own sharing: the stale one is deleted, the
/// live one and every record stay.
#[skuld::test]
fn a_stale_pin_beside_a_live_one_is_deleted_and_the_live_one_still_keeps_every_record() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pin = runs.pin().unwrap();
    let stale = runs.dir.join("pin-stale.pin");
    fs::write(&stale, "").unwrap();

    let problems = runs.write(&record(2, 2)).unwrap();

    assert!(!stale.exists());
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [true, true]);
    assert_eq!(problems, Vec::<String>::new());
    drop(pin);
}

#[skuld::test]
fn the_records_are_kept_until_the_last_of_two_pins_is_dropped() {
    let (_dir, runs) = fixture();
    let first = runs.pin().unwrap();
    let second = runs.hold().unwrap();
    runs.write(&record(1, 1)).unwrap();

    drop(first);
    runs.write(&record(2, 2)).unwrap();
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [true, true]);

    drop(second);
    runs.write(&record(3, 3)).unwrap();
    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [false, false, true]
    );
}

#[skuld::test]
fn dropping_a_pin_releases_its_records_to_the_next_write() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();
    runs.write(&record(1, 1)).unwrap();
    runs.write(&record(2, 2)).unwrap();

    drop(pin);
    assert_eq!(
        names(&runs).len(),
        2,
        "dropping the pin deletes its file and nothing else"
    );
    runs.write(&record(3, 3)).unwrap();

    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [false, false, true]
    );
}

// the pin's listing ===================================================================================================

#[skuld::test]
fn appeared_lists_only_records_not_in_the_pins_listing_ascending() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 100)).unwrap();
    let pin = runs.pin().unwrap();
    runs.write(&record(2, 30)).unwrap();
    runs.write(&record(3, 10)).unwrap();
    runs.write(&record(4, 20)).unwrap();

    let appeared = runs.appeared(&pin).unwrap();

    assert_eq!(appeared, [record(3, 10), record(4, 20), record(2, 30)]);
}

#[skuld::test]
fn appeared_is_empty_when_nothing_was_written_after_the_pin() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();

    let pin = runs.pin().unwrap();

    assert_eq!(runs.appeared(&pin).unwrap(), []);
}

// uninstall ===========================================================================================================

/// The note of a `remove_all` that has nothing to wait for: it is never made.
fn no_note() {
    panic!("nothing holds the directory, so there is nothing to wait for");
}

/// `remove_all` while `holder` (what a `run` or a stop holds) is alive. A thread drops `holder` when the
/// note is made, so the wait ends on the real condition and nothing is slept for. Returns the result and,
/// when a note was made, what the directory held at that moment.
fn remove_all_while_held(runs: &Runs, holder: impl Send) -> (io::Result<()>, Option<Vec<String>>) {
    let (release, released) = mpsc::channel::<()>();
    let mut seen = None;
    let result = thread::scope(|scope| {
        scope.spawn(move || {
            let _ = released.recv();
            drop(holder);
        });
        runs.remove_all(|| {
            seen = Some(names(runs));
            let _ = release.send(());
        })
    });
    (result, seen)
}

#[skuld::test]
fn remove_all_clears_the_directory_and_its_pins_that_nothing_holds() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    fs::write(runs.dir.join("pin-stale.pin"), "").unwrap();
    fs::write(runs.dir.join(format!("{}.json.tmp", instance(2).hex())), "").unwrap();

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
    runs.remove_all(no_note).unwrap();
}

/// A pin left by a power cut, of an account the writer has no entry for, is told from a live one by its
/// sharing alone: it is deleted with the directory and is not waited for.
#[skuld::test]
fn remove_all_does_not_wait_for_a_stale_pin_it_has_no_entry_on() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let stale = runs.dir.join("pin-old.pin");
    fs::write(&stale, "").unwrap();
    Descriptor::from_sddl("D:P(A;;FA;;;SY)(A;;FA;;;BA)")
        .unwrap()
        .set_dacl(&stale)
        .unwrap();

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
}

/// A pin whose state cannot be told (its delete is pending, which Windows answers with access denied)
/// may be held, and none can be taken now: the removal returns the failure, naming the pin, before it
/// deletes anything, and does not wait for a handle that may never go. Once the pin can be told about,
/// the same call finishes.
#[skuld::test]
fn remove_all_with_a_pin_it_cannot_tell_about_deletes_nothing_and_names_it() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    fs::write(runs.dir.join("other.txt"), "").unwrap();
    let pending = runs.dir.join("pin-odd.pin");
    let reader = delete_pending(&pending);
    let before = names(&runs);

    let failed = runs.remove_all(no_note).unwrap_err();

    assert!(failed.to_string().contains("pin-odd.pin"), "{failed}");
    assert_eq!(names(&runs), before, "nothing was deleted");
    drop(reader);
    runs.remove_all(no_note).unwrap();
    assert!(!runs.dir.exists());
}

#[skuld::test]
fn remove_all_of_a_directory_that_is_gone_is_done_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());

    runs.remove_all(no_note).unwrap();
}

/// While a `run` waits, its pin is held and nothing in the directory is deleted: the removal says it
/// waits, then removes everything when the `run` returns.
#[skuld::test]
fn remove_all_deletes_nothing_while_a_run_holds_a_pin_and_removes_everything_when_it_returns() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pin = runs.pin().unwrap();
    runs.write(&record(2, 2)).unwrap();
    let before = names(&runs);

    let (result, seen) = remove_all_while_held(&runs, pin);

    result.unwrap();
    assert_eq!(
        seen,
        Some(before),
        "a pin was held at the note, and every file was still there"
    );
    assert!(!runs.dir.exists());
}

/// A stop holds a pin too ([`Runs::hold`]), which keeps the records it reads as much as a `run`'s does.
#[skuld::test]
fn remove_all_waits_for_a_stops_hold_as_for_a_runs_pin() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let hold = runs.hold().unwrap();
    let before = names(&runs);

    let (result, seen) = remove_all_while_held(&runs, hold);

    result.unwrap();
    assert_eq!(seen, Some(before));
    assert!(!runs.dir.exists());
}

/// A pin held by a `run` of another account, which this account has no entry on, is waited for as any
/// is: the directory's right to delete entries does not let a removal take it.
#[skuld::test]
fn remove_all_waits_for_a_live_pin_the_removing_account_has_no_entry_on() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let live = runs.dir.join("pin-other.pin");
    let holder = files::pin_options()
        .access_mode(files::PIN_ACCESS | WRITE_DAC)
        .create_new(true)
        .open(&live)
        .unwrap();
    Descriptor::from_sddl("D:P(A;;FA;;;SY)")
        .unwrap()
        .set_dacl_of(&holder)
        .unwrap();
    let before = names(&runs);

    let (result, seen) = remove_all_while_held(&runs, holder);

    result.unwrap();
    assert_eq!(seen, Some(before), "the live pin and the record were there at the note");
    assert!(!runs.dir.exists());
}

#[skuld::test]
fn remove_all_waits_for_two_held_pins_and_is_not_confused_by_a_stale_one_beside_them() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let first = runs.pin().unwrap();
    let second = runs.hold().unwrap();
    fs::write(runs.dir.join("pin-stale.pin"), "").unwrap();

    let (result, seen) = remove_all_while_held(&runs, (first, second));

    result.unwrap();
    assert!(seen.is_some(), "two pins were held at the note");
    assert!(!runs.dir.exists());
}

/// The removal's marker, placed as an uninstall that began and was cut short leaves it. It is beside the
/// task's directory, in `runs`.
fn leave_marker(runs: &Runs) {
    fs::write(&runs.marker, "").unwrap();
}

/// A pin taken after the removal began finds its marker, is dropped, and is refused with an error of
/// its own, which is not the "no records" of a directory that is gone: the records it would have kept
/// are being deleted, and no pin file is left to hold the removal up.
#[skuld::test]
fn a_pin_or_hold_taken_after_the_removal_began_backs_off_and_leaves_no_file() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    leave_marker(&runs);

    let pin = runs.pin().err().expect("a pin is refused");
    let hold = runs.hold().err().expect("a hold is refused");

    for refused in [pin, hold] {
        assert_eq!(refused.kind(), io::ErrorKind::ResourceBusy);
        assert!(refused.to_string().contains("being removed"), "{refused}");
    }
    assert_eq!(names(&runs), [file_of(&instance(1))]);
}

/// A hold in a directory that is gone is "no records", which a stop takes as nothing to keep: the
/// refusal during a removal is not that.
#[skuld::test]
fn a_hold_in_a_directory_that_is_gone_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());

    assert_eq!(runs.hold().err().map(|e| e.kind()), Some(io::ErrorKind::NotFound));
}

/// The other order: a pin taken before the marker is waited for by a removal that finds the marker
/// already there, and nothing is deleted while it is held.
#[skuld::test]
fn a_pin_taken_before_the_marker_is_waited_for_and_nothing_is_deleted_under_it() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pin = runs.pin().unwrap();
    leave_marker(&runs);
    let before = names(&runs);

    let (result, seen) = remove_all_while_held(&runs, pin);

    result.unwrap();
    assert_eq!(seen, Some(before));
    assert!(!runs.dir.exists());
}

/// The removal places its marker before it looks at the pins, so that a pin taken while it waits
/// backs off instead of keeping records the removal is about to delete.
#[skuld::test]
fn remove_all_refuses_a_new_pin_while_it_waits_on_one() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pin = runs.pin().unwrap();
    let mut probe = None;

    runs.remove_all(|| {
        probe = Some((runs.marker.exists(), runs.hold().err().map(|e| e.kind())));
        drop(pin);
    })
    .unwrap();

    let (seen, refused) = probe.expect("the removal waited");
    assert!(seen, "the marker was there while the removal waited");
    assert_eq!(refused, Some(io::ErrorKind::ResourceBusy));
    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists(), "the marker goes after the directory");
}

/// A marker an uninstall that died left does not block the removal that finishes it.
#[skuld::test]
fn remove_all_is_not_blocked_by_a_marker_a_cut_short_removal_left() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    leave_marker(&runs);

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
}

/// A file another program holds is no pin: the removal does not wait for it. It fails at once, naming
/// the file, so the user can close what holds it and uninstall again; what else it could delete is
/// deleted. The same call finishes once the handle is gone.
#[skuld::test]
fn remove_all_with_a_file_another_program_holds_fails_naming_it_and_does_not_wait() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    runs.write(&record(2, 2)).unwrap();
    let held_name = file_of(&instance(2));
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(runs.dir.join(&held_name))
        .unwrap();

    let failed = runs.remove_all(no_note).unwrap_err();

    assert!(failed.to_string().contains(&held_name), "{failed}");
    assert!(runs.dir.exists());
    drop(held);
    runs.remove_all(no_note).unwrap();
    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
}

/// A failed removal leaves its marker, so that a `run` still backs off: its records are partly gone.
/// An install, which makes them again, takes it away ([`Runs::ensure`]).
#[skuld::test]
fn a_failed_removal_leaves_its_marker_beside_the_directory() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(runs.dir.join(file_of(&instance(1))))
        .unwrap();

    runs.remove_all(no_note).unwrap_err();

    assert!(runs.marker.exists());
    assert_eq!(runs.hold().err().map(|e| e.kind()), Some(io::ErrorKind::ResourceBusy));
    drop(held);
}

/// The note is for a held pin and for nothing else: records, a temp file, a stale pin and a marker a
/// dead removal left are removed without it.
#[skuld::test]
fn remove_all_makes_no_note_for_anything_but_a_held_pin() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    fs::write(runs.dir.join("pin-stale.pin"), "").unwrap();
    fs::write(runs.dir.join(format!("{}.json.tmp", instance(2).hex())), "").unwrap();
    leave_marker(&runs);

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
}

/// The marker is named so that no task's id can be it: an id has no `~`, so `x`'s marker is not the
/// directory of a task `x.removing`, and removing one leaves the other alone.
#[skuld::test]
fn a_marker_is_never_the_directory_of_another_task() {
    let dir = tempfile::tempdir().unwrap();
    let removed = Runs::of(dir.path(), &Id::try_from("x").unwrap());
    let other = Runs::of(dir.path(), &Id::try_from("x.removing").unwrap());
    made_for(&removed, &process_user_sid());
    made_for(&other, &process_user_sid());
    other.write(&record(1, 1)).unwrap();

    removed.remove_all(no_note).unwrap();

    assert!(!removed.dir.exists());
    assert_eq!(instances_kept(&other, &[instance(1)]), [true]);
}

/// What makes the directory's removal impossible while a pin is held: not even a rename moves it.
#[skuld::test]
fn a_directory_with_a_held_pin_cannot_be_renamed_away() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();

    let moved = fs::rename(&runs.dir, runs.dir.with_extension("moved"));

    assert!(moved.is_err());
    assert_eq!(runs.appeared(&pin).unwrap(), []);
}

// the directory's permissions =========================================================================================

fn security_of(dir: &Path) -> ObjectSecurity {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .unwrap();
    read_object(&file, dir.to_path_buf()).unwrap()
}

fn ace(trustee: crate::backend::scm::sid::Sid, mask: u32, flags: u8) -> Ace {
    Ace {
        trustee,
        mask,
        flags,
        allow: true,
        conditional: false,
    }
}

fn inherited_ace(trustee: crate::backend::scm::sid::Sid, mask: u32) -> Ace {
    ace(trustee, mask, INHERITED_BY_FILES_AND_FOLDERS)
}

/// What the directory's DACL is for `principal`: SYSTEM and Administrators have full control of it and
/// of what is in it; the principal may modify the files made in it (an entry that only files inherit),
/// and has, on the directory itself, what that is without adding a subdirectory and with deleting
/// entries, so that a principal that replaces another can prune what the other wrote.
fn dacl_for(principal: crate::backend::scm::sid::Sid) -> Vec<Ace> {
    sorted(vec![
        inherited_ace(system(), FULL),
        inherited_ace(administrators(), FULL),
        ace(principal.clone(), MODIFY, OBJECT_INHERIT | INHERIT_ONLY),
        ace(principal, (MODIFY & !ADD_SUBDIRECTORY) | DELETE_CHILD, 0),
    ])
}

fn entries_of(dir: &Path) -> Vec<Ace> {
    let security = security_of(dir);
    assert!(security.protected, "the DACL ignores what its parent would inherit");
    let mut entries = security.dacl.expect("the directory has a DACL, not a NULL one");
    entries.sort();
    entries.dedup();
    entries
}

fn sorted(mut entries: Vec<Ace>) -> Vec<Ace> {
    entries.sort();
    entries.dedup();
    entries
}

/// Making the directory names Administrators as its owner, which only an elevated token may.
#[skuld::test]
fn the_directory_is_made_owned_by_administrators_with_a_protected_dacl_naming_the_principal(
    #[fixture(scm_group)] _group: &Group,
) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    fs::create_dir_all(runs.dir.parent().unwrap()).unwrap();

    runs.ensure(&process_user_sid()).unwrap();

    assert_eq!(security_of(&runs.dir).owner, administrators());
    assert_eq!(entries_of(&runs.dir), dacl_for(process_user_sid()));
}

#[skuld::test]
fn a_directory_that_is_there_gets_a_protected_dacl_naming_the_principal() {
    let (_dir, runs) = fixture();

    assert_eq!(entries_of(&runs.dir), dacl_for(process_user_sid()));
    // What lets the account that replaces another prune what the other wrote, and keeps it from
    // making a subdirectory.
    let own = entries_of(&runs.dir)
        .into_iter()
        .find(|ace| ace.trustee == process_user_sid() && ace.flags == 0)
        .expect("the principal's entry for the directory itself");
    assert_ne!(own.mask & DELETE_CHILD, 0);
    assert_eq!(own.mask & ADD_SUBDIRECTORY, 0);
}

#[skuld::test]
fn ensure_for_another_principal_rewrites_the_dacl() {
    let (_dir, runs) = fixture();
    let first = record(1, 1);
    runs.write(&first).unwrap();
    // `Users` has this test's account among its members, so it still reaches what it made.
    let users = sid_from_parts(5, &[32, 545]);

    runs.ensure(&users).unwrap();

    assert_eq!(entries_of(&runs.dir), dacl_for(users));
    assert_eq!(runs.read(&first.instance).unwrap(), Some(first));
    runs.write(&record(2, 2)).unwrap();
}

/// The DACL call changes the directory and nothing it holds: the files made in it keep their trustees
/// and masks, and a hard link in it, whose target is one file that lives elsewhere, leaves that file's
/// permissions as they were. A call that applies the new DACL to what inherits it (`SetNamedSecurityInfo`,
/// `SetSecurityInfo`) rewrites both.
#[skuld::test]
fn a_change_of_principal_leaves_what_is_in_the_directory_and_a_hard_links_target_alone() {
    let (dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let kept = runs.dir.join(file_of(&instance(1)));
    let elsewhere = tempfile::tempdir_in(dir.path()).unwrap();
    let target = elsewhere.path().join("target.txt");
    fs::write(&target, b"x").unwrap();
    fs::hard_link(&target, runs.dir.join("link.json")).unwrap();
    let entries_of_file = |path: &Path| {
        security_of(path)
            .dacl
            .expect("a DACL")
            .into_iter()
            .map(|ace| (ace.trustee, ace.mask, ace.allow))
            .collect::<Vec<_>>()
    };
    let (record_before, target_before) = (entries_of_file(&kept), security_of(&target));

    runs.ensure(&sid_from_parts(5, &[32, 545])).unwrap();

    assert_eq!(entries_of_file(&kept), record_before, "a record in the directory");
    assert_eq!(security_of(&target), target_before, "the target of a hard link in it");
}

#[skuld::test]
fn a_task_has_records_to_remove_while_its_directory_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    assert!(!runs.is_present());

    made_for(&runs, &process_user_sid());
    assert!(runs.is_present());

    runs.remove_all(no_note).unwrap();
    assert!(!runs.is_present());
}

/// An install makes the directory again, and with it a task that may run: a marker a removal left
/// must not make its `run` back off.
#[skuld::test]
fn ensure_clears_a_marker_a_cut_short_removal_left() {
    let (_dir, runs) = fixture();
    leave_marker(&runs);
    assert!(runs.pin().is_err());

    runs.ensure(&process_user_sid()).unwrap();

    assert!(runs.pin().is_ok());
    assert!(!runs.marker.exists());
}

#[skuld::test]
fn ensure_is_repeatable() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();

    runs.ensure(&process_user_sid()).unwrap();

    assert!(runs.read(&instance(1)).unwrap().is_some());
}

// the folders above the directory =====================================================================================

fn enabled() -> FolderCheck {
    FolderCheck::new(CheckPermissions::Enable)
}

fn disabled() -> FolderCheck {
    FolderCheck::new(CheckPermissions::Disable)
}

/// A `Goetia` folder that was there before goetia made one is refused when an account that is not
/// trusted owns it (the folder here is the test account's own), and nothing is made in it: a
/// standard user can create it in `%ProgramData%`, and put a link in it.
#[skuld::test]
fn a_goetia_folder_that_is_there_and_is_not_safe_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    let goetia = dir.path().join("Goetia");
    fs::create_dir(&goetia).unwrap();

    let refused = runs.ensure_roots(&enabled()).unwrap_err();

    let Error::FolderUnsafe { path, .. } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &goetia);
    assert_eq!(
        fs::read_dir(&goetia).unwrap().count(),
        0,
        "nothing was made in the folder that was refused"
    );
}

/// `Goetia\runs` is goetia's alone to make, so one that is there and is not as goetia makes it (its
/// owner, protection and entries) is refused, naming it, and left as it is.
#[skuld::test]
fn a_runs_folder_that_is_there_and_is_not_as_wanted_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    let root = dir.path().join("Goetia").join("runs");
    fs::create_dir_all(&root).unwrap();
    // `Goetia\logs` is there too: making it names Administrators as its owner, which a test that needs no
    // elevation leaves to the `scm` group's.
    fs::create_dir_all(crate::backend::scm::folder::logs_folder(dir.path())).unwrap();

    let refused = runs.ensure_roots(&disabled()).unwrap_err();

    let Error::FolderNotAsMade { path } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &root);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0, "nothing was made in it");
}

/// Making the folders names Administrators as their owner, which only an elevated token may.
#[skuld::test]
fn the_folders_are_made_goetia_inheriting_and_runs_as_wanted_and_a_second_call_adopts_them(
    #[fixture(scm_group)] _group: &Group,
) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    let goetia = dir.path().join("Goetia");

    runs.ensure_roots(&disabled()).unwrap();

    let made = security_of(&goetia);
    assert_eq!(
        made.owner,
        administrators(),
        "named, whatever the creator's own default"
    );
    assert!(
        !made.protected,
        "it inherits what %ProgramData% gives, as the services' logs folder does"
    );
    let wanted = Descriptor::for_folder().unwrap().wanted().unwrap();
    assert!(is_as_wanted(&security_of(&goetia.join("runs")), &wanted));
    runs.ensure_roots(&disabled()).unwrap();
}
