//! `Runs` over a temporary directory. Every test makes its own, removed when the test ends.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use tempfile::TempDir;
use windows_sys::Wdk::Storage::FileSystem::{FILE_CREATE, FILE_DELETE_ON_CLOSE, FILE_NON_DIRECTORY_FILE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    READ_CONTROL, WRITE_DAC,
};

use super::*;
use crate::backend::scm::shim::acl::acl_tests::windows::junction;
use crate::backend::scm::shim::acl::{Ace, CheckPermissions, ObjectSecurity, is_as_wanted, read_object};
use crate::backend::scm::sid::{administrators, sid_from_parts, system};
use crate::backend::scm::test_support::{
    Undeletable, delete_pending, directory_held_open, directory_with_a_file, make_mount_point,
};
use crate::elevation::test_support::process_user_sid;
use crate::testing::groups::{Group, scm_group};

const FULL: u32 = 0x1F_01FF;
const MODIFY: u32 = 0x13_01BF;
const INHERITED_BY_FILES_AND_FOLDERS: u8 = 0x03;
const OBJECT_INHERIT: u8 = 0x01;
const INHERIT_ONLY: u8 = 0x08;
const ADD_SUBDIRECTORY: u32 = 0x4;
const WRITE_EA: u32 = 0x10;
const DELETE_CHILD: u32 = 0x40;
const WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x1_0000;
/// What the account has on the files it makes (`Descriptor::for_runs`): [`MODIFY`] without the rights to
/// set attributes or extended attributes.
const FILES: u32 = MODIFY & !WRITE_ATTRIBUTES & !WRITE_EA;

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
    runs.ensure(std::slice::from_ref(principal)).unwrap();
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

/// The temp file `path`, in `runs`' directory, open as a shim writing it holds it.
fn temp_held(runs: &Runs, path: &Path) -> fs::File {
    files::create_temp(&Dir::open(&runs.dir).unwrap(), path.file_name().unwrap()).unwrap()
}

/// The pin `path`, in `runs`' directory, held as a `run` holds one, and open for `WRITE_DAC` so that the
/// test can give it the permissions of a pin another account made.
fn pin_held_with_write_dac(runs: &Runs, path: &Path) -> fs::File {
    Dir::open(&runs.dir)
        .unwrap()
        .open_in(
            path.file_name().unwrap(),
            files::PIN_ACCESS | WRITE_DAC,
            0,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE | FILE_DELETE_ON_CLOSE,
        )
        .unwrap()
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

        runs.write(&written, None).unwrap();

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
        runs.write(&written, None).unwrap();
        assert_eq!(runs.read(&written.instance).unwrap(), Some(written));
    }
    assert_eq!(
        runs.read(&instance(99)).unwrap(),
        None,
        "another instance has no record"
    );
}

/// Nothing is renamed onto a name that exists: a second record for an instance is refused, whoever holds the
/// first, and the first is as it was.
#[skuld::test]
fn a_second_write_for_an_instance_is_refused_and_leaves_its_record() {
    let (_dir, runs) = fixture();
    let first = RunRecord {
        outcome: RunOutcome::Stopped,
        ..record(1, 5)
    };
    let second = RunRecord {
        outcome: RunOutcome::IncompleteStop,
        ..first.clone()
    };
    runs.write(&first, None).unwrap();

    let refused = runs.write(&second, None).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists, "{refused:?}");
    assert_eq!(runs.read(&first.instance).unwrap(), Some(first.clone()));
    assert_eq!(names(&runs), [file_of(&first.instance)], "no temp file is left");
}

#[skuld::test]
fn a_write_leaves_no_partial_record_and_removes_a_leftover_temp() {
    let (_dir, runs) = fixture();
    let own = record(1, 1);
    // What a write interrupted before its rename leaves: for this instance and for another.
    fs::write(runs.dir.join(format!("{}.json.tmp", own.instance.hex())), "{\"sta").unwrap();
    fs::write(runs.dir.join(format!("{}.json.tmp", instance(2).hex())), "{\"sta").unwrap();

    runs.write(&own, None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();
    let calls = std::cell::Cell::new(0);
    let pin = std::cell::RefCell::new(None);

    let dir = Dir::open(&runs.dir).unwrap();
    runs.prune_listing(&dir, &instance(1), || {
        let listing = entries(&dir);
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            *pin.borrow_mut() = Some(runs.pin().unwrap());
            runs.write(&record(2, 2), None).unwrap();
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
    let held = temp_held(&runs, &in_flight);

    let problems = runs.write(&record(1, 1), None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();

    let dir = Dir::open(&runs.dir).unwrap();
    let problems = runs.prune_listing(&dir, &instance(1), || Err(io::Error::other("listing denied")));

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("listing denied"), "{problems:?}");
}

/// A removal that fails (not a handle, not a pending delete) is told, and the prune goes on to the rest.
#[skuld::test]
fn a_removal_that_fails_is_a_problem_and_the_rest_are_removed() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let stuck = runs.dir.join(file_of(&instance(2)));
    directory_with_a_file(&stuck);
    let doomed = runs.dir.join(format!("{}.json.tmp", instance(3).hex()));
    fs::write(&doomed, "").unwrap();

    let problems = runs.write(&record(4, 4), None).unwrap();

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains(&file_of(&instance(2))), "{problems:?}");
    assert!(!doomed.exists() && runs.read(&instance(1)).unwrap().is_none());
}

#[skuld::test]
fn without_a_pin_a_write_keeps_only_its_own_record() {
    let (_dir, runs) = fixture();

    runs.write(&record(1, 1), None).unwrap();
    runs.write(&record(2, 2), None).unwrap();
    runs.write(&record(3, 3), None).unwrap();

    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [false, false, true]
    );
}

#[skuld::test]
fn a_live_pin_stops_all_pruning() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let pin = runs.pin().unwrap();
    let before = names(&runs).len();

    runs.write(&record(2, 2), None).unwrap();
    runs.write(&record(3, 3), None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();
    // A pin whose holder died with the machine: no handle holds the file.
    let stale = runs.dir.join("pin-stale.pin");
    fs::write(&stale, "").unwrap();

    runs.write(&record(2, 2), None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();
    let stale = runs.dir.join("pin-old.pin");
    fs::write(&stale, "").unwrap();
    let system_only = Descriptor::from_sddl("D:P(A;;FA;;;SY)").unwrap();
    crate::backend::scm::test_support::set_dacl(&system_only, &stale).unwrap();

    let problems = runs.write(&record(2, 2), None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();
    let live = runs.dir.join("pin-other.pin");
    let holder = pin_held_with_write_dac(&runs, &live);
    Descriptor::from_sddl("D:P(A;;FA;;;SY)")
        .unwrap()
        .set_dacl_of(&holder)
        .unwrap();

    let problems = runs.write(&record(2, 2), None).unwrap();

    assert!(live.exists(), "a live pin is not deleted");
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [true, true]);
    assert_eq!(problems, Vec::<String>::new());
    drop(holder);
    assert!(!live.exists(), "the pin goes when its holder does");
}

/// A pin that cannot be deleted for another reason than a sharing violation (here it is a directory that is
/// not empty) is told, and is not taken as held: reading it as held would stop every later prune.
#[skuld::test]
fn a_pin_that_cannot_be_deleted_is_a_problem_and_is_taken_as_not_held() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let odd = runs.dir.join("pin-odd.pin");
    directory_with_a_file(&odd);

    let problems = runs.write(&record(2, 2), None).unwrap();

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("pin-odd.pin"), "{problems:?}");
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [false, true]);
}

/// A pin whose delete is pending is going away by itself: a prune takes it as deleted, tells nothing of
/// it, and goes on.
#[skuld::test]
fn a_pin_whose_delete_is_pending_is_no_problem_and_no_hold_to_a_prune() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let reader = delete_pending(&runs.dir.join("pin-odd.pin"));

    let problems = runs.write(&record(2, 2), None).unwrap();

    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [false, true]);
    drop(reader);
}

/// A stop's hold ([`Runs::hold`], no listing) keeps the records it will read as a `run`'s pin does.
#[skuld::test]
fn a_hold_keeps_the_records_from_every_write_as_a_pin_does() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let hold = runs.hold().unwrap();

    runs.write(&record(2, 2), None).unwrap();
    runs.write(&record(3, 3), None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();
    let pin = runs.pin().unwrap();
    let stale = runs.dir.join("pin-stale.pin");
    fs::write(&stale, "").unwrap();

    let problems = runs.write(&record(2, 2), None).unwrap();

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
    runs.write(&record(1, 1), None).unwrap();

    drop(first);
    runs.write(&record(2, 2), None).unwrap();
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [true, true]);

    drop(second);
    runs.write(&record(3, 3), None).unwrap();
    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [false, false, true]
    );
}

#[skuld::test]
fn dropping_a_pin_releases_its_records_to_the_next_write() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();
    runs.write(&record(1, 1), None).unwrap();
    runs.write(&record(2, 2), None).unwrap();

    drop(pin);
    assert_eq!(
        names(&runs).len(),
        2,
        "dropping the pin deletes its file and nothing else"
    );
    runs.write(&record(3, 3), None).unwrap();

    assert_eq!(
        instances_kept(&runs, &[instance(1), instance(2), instance(3)]),
        [false, false, true]
    );
}

// the pin's listing ===================================================================================================

#[skuld::test]
fn appeared_lists_only_records_not_in_the_pins_listing_ascending() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 100), None).unwrap();
    let pin = runs.pin().unwrap();
    runs.write(&record(2, 30), None).unwrap();
    runs.write(&record(3, 10), None).unwrap();
    runs.write(&record(4, 20), None).unwrap();

    let appeared = pin.appeared().unwrap();

    assert_eq!(appeared, [record(3, 10), record(4, 20), record(2, 30)]);
}

#[skuld::test]
fn appeared_is_empty_when_nothing_was_written_after_the_pin() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();

    let pin = runs.pin().unwrap();

    assert_eq!(pin.appeared().unwrap(), []);
}

// uninstall ===========================================================================================================

/// The note of a `remove_all` that has nothing to wait for: it is never made.
fn no_note(_: &Path, _: PauseKind) {
    panic!("nothing holds the directory, so there is nothing to wait for");
}

/// `remove_all` while `holder` (what a `run` or a stop holds) is alive. A thread drops `holder` when the
/// note is made, so the wait ends on the real condition and nothing is slept for. Returns the result and,
/// when a note was made, what the directory held at the first one: a pin that closes can bring a second
/// note, by which time the holder is gone. A `remove_all` that returns with no note ends the wait too (the
/// sender is dropped): a removal that failed must fail the test and not hang it.
fn remove_all_while_held(runs: &Runs, holder: impl Send) -> (Result<()>, Option<Vec<String>>) {
    let (release, released) = mpsc::channel::<()>();
    let mut seen = None;
    let result = thread::scope(|scope| {
        scope.spawn(move || {
            let _ = released.recv();
            drop(holder);
        });
        let result = runs.remove_all(|_, _| {
            seen.get_or_insert_with(|| names(runs));
            let _ = release.send(());
        });
        drop(release);
        result
    });
    (result, seen)
}

#[skuld::test]
fn remove_all_clears_the_directory_and_its_pins_that_nothing_holds() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
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
    runs.write(&record(1, 1), None).unwrap();
    let stale = runs.dir.join("pin-old.pin");
    fs::write(&stale, "").unwrap();
    let system_only = Descriptor::from_sddl("D:P(A;;FA;;;SY)").unwrap();
    crate::backend::scm::test_support::set_dacl(&system_only, &stale).unwrap();

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
}

/// Pins whose state cannot be told (their delete fails, and not for a handle) may be held, and none can be
/// taken now: the removal returns the failure, naming every one of them, before it deletes anything, and
/// does not wait for a handle that may never go. Once they can be told about, the same call finishes.
#[skuld::test]
fn remove_all_with_pins_it_cannot_tell_about_deletes_nothing_and_names_each() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    fs::write(runs.dir.join("other.txt"), "").unwrap();
    fs::write(runs.dir.join("pin-odd.pin"), "").unwrap();
    fs::write(runs.dir.join("pin-odd2.pin"), "").unwrap();
    let before = names(&runs);
    let refused = Undeletable::new(&runs.dir, &["pin-odd.pin", "pin-odd2.pin"]);

    let failed = runs.remove_all(no_note).unwrap_err();

    let said = failed.report().to_string();
    assert!(said.contains("pin-odd.pin"), "{said}");
    assert!(said.contains("pin-odd2.pin"), "{said}");
    assert_eq!(names(&runs), before, "nothing was deleted");
    drop(refused);
    runs.remove_all(no_note).unwrap();
    assert!(!runs.dir.exists());
}

/// A file whose delete is pending is deleted as far as anything can do it: it goes when the handle open
/// on it closes. Its delete was no failure, so it is no reason to refuse the removal, whether it is a pin
/// or a record; the directory is not empty until it goes, and the removal looks again. The only note is
/// for what is held, here a temp file the shim is writing, and the three handles are dropped when it is made.
#[skuld::test]
fn remove_all_takes_a_file_whose_delete_is_pending_as_deleted() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let pending_pin = delete_pending(&runs.dir.join("pin-odd.pin"));
    let pending_record = delete_pending(&runs.dir.join(file_of(&instance(2))));
    let temp = runs.dir.join(format!("{}.json.tmp", instance(3).hex()));
    let writing = temp_held(&runs, &temp);
    let mut handles = Some((pending_pin, pending_record, writing));
    let mut noted = Vec::new();

    runs.remove_all(|path, _| {
        noted.push(path.to_path_buf());
        drop(handles.take());
    })
    .unwrap();

    assert_eq!(noted, [temp], "a pending delete is no pause");
    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
}

/// The shim's write is under way when the task is uninstalled by another route (its task deleted from
/// outside): its temp file is open without sharing delete, for as long as the write lasts. The removal
/// waits for it, naming the file, as it waits for a pin, and then removes everything.
#[skuld::test]
fn remove_all_waits_for_a_temp_file_the_shim_is_writing_and_names_it() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let temp = runs.dir.join(format!("{}.json.tmp", instance(2).hex()));
    let writing = temp_held(&runs, &temp);
    let mut noted = Vec::new();
    let mut writing = Some(writing);

    runs.remove_all(|held, _| {
        noted.push(held.to_path_buf());
        if held == temp {
            drop(writing.take());
        }
    })
    .unwrap();

    assert_eq!(noted, [temp], "one pause, named once");
    assert!(!runs.dir.exists());
}

/// A pin whose name is not Unicode (the task's account can make one) is a pin all the same: a stale one
/// is deleted with the rest, and the removal finishes. It once was skipped by the listing and kept by the
/// delete, so the removal never ended.
#[skuld::test]
fn remove_all_deletes_a_pin_whose_name_is_not_unicode() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;

    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let mut units: Vec<u16> = "pin-".encode_utf16().collect();
    units.push(0xD800);
    units.extend(".pin".encode_utf16());
    fs::write(runs.dir.join(OsString::from_wide(&units)), "").unwrap();

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
    runs.write(&record(1, 1), None).unwrap();
    let pin = runs.pin().unwrap();
    runs.write(&record(2, 2), None).unwrap();
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
    runs.write(&record(1, 1), None).unwrap();
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
    runs.write(&record(1, 1), None).unwrap();
    let live = runs.dir.join("pin-other.pin");
    let holder = pin_held_with_write_dac(&runs, &live);
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
    runs.write(&record(1, 1), None).unwrap();
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
    runs.write(&record(1, 1), None).unwrap();
    leave_marker(&runs);

    let pin = runs.pin().err().expect("a pin is refused");
    let hold = runs.hold().err().expect("a hold is refused");

    for refused in [pin, hold] {
        assert_eq!(refused.kind(), io::ErrorKind::ResourceBusy);
        let text = refused.to_string();
        assert!(
            text.contains("the run records of `frpc`")
                && text.contains("`frpc` is being, or did not finish being, installed or uninstalled"),
            "{text}"
        );
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
    runs.write(&record(1, 1), None).unwrap();
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
    runs.write(&record(1, 1), None).unwrap();
    let pin = runs.pin().unwrap();
    let mut probes = Vec::new();
    let mut pin = Some(pin);

    runs.remove_all(|_, _| {
        probes.push((runs.marker.exists(), runs.hold().err().map(|e| e.kind())));
        drop(pin.take());
    })
    .unwrap();

    assert!(!probes.is_empty(), "the removal waited");
    for (seen, refused) in probes {
        assert!(seen, "the marker was there while the removal waited");
        assert_eq!(refused, Some(io::ErrorKind::ResourceBusy));
    }
    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists(), "the marker goes after the directory");
}

/// The marker is placed beside the directory, and placing it again changes nothing.
#[skuld::test]
fn close_places_the_marker_and_is_idempotent() {
    let (_dir, runs) = fixture();

    assert_eq!(runs.close().unwrap(), Closed::Placed);
    assert!(runs.marker.exists());
    assert_eq!(runs.close().unwrap(), Closed::Placed);
    assert!(runs.marker.exists());
}

/// With `runs` gone nothing can be placed in it, and nothing needs the marker: there are no records.
#[skuld::test]
fn close_with_no_directory_places_none_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());

    assert_eq!(runs.close().unwrap(), Closed::NoRecords);
    assert!(!runs.marker.exists());
}

#[skuld::test]
fn reopen_takes_the_marker_away_and_a_missing_one_is_ok() {
    let (_dir, runs) = fixture();
    leave_marker(&runs);

    runs.reopen().unwrap();
    assert!(!runs.marker.exists());
    runs.reopen().unwrap();
}

#[skuld::test]
fn is_closed_sees_the_marker() {
    let (_dir, runs) = fixture();
    assert!(!runs.is_closed().unwrap());

    leave_marker(&runs);

    assert!(runs.is_closed().unwrap());
}

/// A pin that ignores the marker is the removal's own: it is made while the marker stands, and a pin that
/// does not ignore it is refused and leaves no file.
#[skuld::test]
fn hold_with_the_marker_is_refused_busy_and_hold_ignoring_it_is_not() {
    let (_dir, runs) = fixture();
    leave_marker(&runs);

    let refused = runs
        .hold_in(Dir::open(&runs.dir).unwrap(), MarkerCheck::Refuse)
        .err()
        .expect("a pin that checks the marker is refused");
    assert_eq!(refused.kind(), io::ErrorKind::ResourceBusy);
    assert_eq!(names(&runs), Vec::<String>::new());

    let own = runs
        .hold_in(Dir::open(&runs.dir).unwrap(), MarkerCheck::Ignore)
        .expect("a pin that ignores the marker is made");
    assert_eq!(names(&runs).len(), 1, "the pin is a file in the directory");
    drop(own);
}

/// A held pin is named at once, whoever holds it (a shim's, a `run`'s, a stop's).
#[skuld::test]
fn held_pin_reports_the_path_of_a_held_pin_at_once() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let pin = runs.hold().unwrap();
    let before = names(&runs);

    let held = runs.held_pin().unwrap().expect("the pin is held");

    assert_eq!(held.parent(), Some(runs.dir.as_path()));
    assert!(held.extension().is_some_and(|e| e == "pin"), "{}", held.display());
    assert_eq!(names(&runs), before, "nothing was deleted");
    drop(pin);
}

/// A pin nothing holds (a power cut's) is no hold, and a directory that is gone holds none.
#[skuld::test]
fn held_pin_with_no_pin_is_none() {
    let (dir, runs) = fixture();
    assert_eq!(runs.held_pin().unwrap(), None);
    fs::write(runs.dir.join("pin-stale.pin"), "").unwrap();
    assert_eq!(runs.held_pin().unwrap(), None);

    let gone = Runs::of(dir.path(), &Id::try_from("absent").unwrap());
    assert_eq!(gone.held_pin().unwrap(), None);
}

/// A pin that cannot be judged may be held: the change's check fails and names it, and nothing is deleted.
#[skuld::test]
fn an_unjudgeable_pin_is_an_error() {
    let (_dir, runs) = fixture();
    fs::write(runs.dir.join("pin-odd.pin"), "").unwrap();
    let before = names(&runs);
    let refused = Undeletable::new(&runs.dir, &["pin-odd.pin"]);

    let failed = runs.held_pin().unwrap_err();

    let said = failed.report().to_string();
    assert!(said.contains("pin-odd.pin"), "{said}");
    let Error::Os { what, .. } = &failed else {
        panic!("one pin that cannot be judged is one error: {failed:?}")
    };
    assert_eq!(
        what, "tell whether a run holds",
        "a question about a run does not say goetia failed to remove"
    );
    assert_eq!(names(&runs), before);
    drop(refused);
}

/// The writer's pin never blocks its own prune.
#[skuld::test]
fn write_with_its_own_pin_prunes_and_is_not_blocked_by_it() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let own = runs.hold().unwrap();

    let problems = runs.write(&record(2, 2), Some(own)).unwrap();

    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(instances_kept(&runs, &[instance(1), instance(2)]), [false, true]);
    assert_eq!(names(&runs), [file_of(&instance(2))], "the pin is gone");
}

/// The provisional file of the run `instance`'s first `Stopped`.
fn stopping_file_of(instance: &InstanceId) -> String {
    format!("{}.stopping", instance.hex())
}

/// The run `n`'s record as a first `Stopped` writes it.
fn stopped(n: u8, started: u64) -> RunRecord {
    RunRecord {
        outcome: RunOutcome::Stopped,
        ..record(n, started)
    }
}

/// A first `Stopped` goes to a file of its own beside where the record will be, keeps the writer's pin and
/// prunes nothing: the run is still alive, so "no pin held" keeps meaning "no run alive".
#[skuld::test]
fn write_stopping_leaves_a_provisional_file_keeps_the_pin_and_prunes_nothing() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let pin = runs.hold().unwrap();

    pin.write_stopping(&instance(2), 2).unwrap();

    assert!(runs.dir.join(stopping_file_of(&instance(2))).exists());
    assert!(!runs.dir.join(file_of(&instance(2))).exists(), "no record is written");
    assert_eq!(instances_kept(&runs, &[instance(1)]), [true], "a stopping write pruned");
    assert!(
        names(&runs).iter().all(|name| !name.ends_with(".tmp")),
        "no temp file is left: {:?}",
        names(&runs)
    );
    assert!(runs.held_pin().unwrap().is_some(), "a stopping write released the pin");
    drop(pin);
    assert_eq!(runs.held_pin().unwrap(), None);
}

/// The file lands where the pin's handle is, not at the path: the directory that became a link after the pin's
/// handle opened it refuses the write, and nothing is made in the link's target. (The pin's file is held elsewhere:
/// a directory with a file in it cannot become a link.)
#[skuld::test]
fn write_stopping_writes_through_the_pins_handle_and_not_the_path() {
    let (_dir, runs) = fixture();
    let (held, target) = held_and_then_linked(&runs);
    let pin = Pin {
        _file: tempfile::tempfile().unwrap(),
        dir: held,
        listing: None,
    };

    let refused = pin.write_stopping(&instance(1), 1).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::NotADirectory, "{refused}");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1, "only victim.txt");
}

/// The provisional file is written to a temp file of its own name, which no write of the record uses: another handle
/// holds it (a scanner, say), so the write fails and cannot remove it, and the run's record lands all the same, to its
/// own temp name.
#[skuld::test]
fn a_provisional_write_that_fails_at_its_temp_file_does_not_stop_the_record() {
    let (_dir, runs) = fixture();
    let pin = runs.hold().unwrap();
    let temp = runs.dir.join(format!("{}.stopping.tmp", instance(1).hex()));
    let holder = temp_held(&runs, &temp);

    let failed = pin.write_stopping(&instance(1), 1).unwrap_err();
    let exited = record(1, 1);
    let problems = runs.write(&exited, Some(pin)).unwrap();

    assert!(files::is_sharing_violation(&failed), "{failed}");
    assert_eq!(
        problems,
        Vec::<String>::new(),
        "a file that is held is for the next prune"
    );
    assert_eq!(runs.read(&instance(1)).unwrap(), Some(exited));
    assert!(temp.exists(), "the held temp file is left");
    drop(holder);
    runs.write(&record(2, 2), None).unwrap();
    assert_eq!(names(&runs), [file_of(&instance(2))], "the next prune removes it");
}

/// A failed write whose temp file cannot be removed either says so in its error, which the caller logs, and keeps
/// the kind of the write's own failure.
#[skuld::test]
fn a_failed_write_whose_cleanup_failed_says_so_and_keeps_its_kind() {
    let failed = io::Error::new(io::ErrorKind::AlreadyExists, "the name exists");
    let temp = OsStr::new("x.stopping.tmp");

    let told = after_cleanup(failed, temp, DeleteOutcome::Failed(io::Error::other("access denied")));

    assert_eq!(told.kind(), io::ErrorKind::AlreadyExists);
    let text = told.to_string();
    assert!(
        text.contains("the name exists") && text.contains("x.stopping.tmp") && text.contains("access denied"),
        "{text}"
    );
    for removed in [DeleteOutcome::Deleted, DeleteOutcome::Held] {
        let failed = io::Error::new(io::ErrorKind::AlreadyExists, "the name exists");
        assert_eq!(after_cleanup(failed, temp, removed).to_string(), "the name exists");
    }
}

/// A provisional file with no record beside it is a run that was told to stop and never recorded how it ended.
#[skuld::test]
fn read_of_a_provisional_file_alone_is_stopped_with_its_started() {
    let (_dir, runs) = fixture();
    let pin = runs.hold().unwrap();
    pin.write_stopping(&instance(1), 7).unwrap();

    assert_eq!(runs.read(&instance(1)).unwrap(), Some(stopped(1, 7)));
}

/// The record wins over a provisional file beside it.
#[skuld::test]
fn read_gives_the_record_when_there_is_one_beside_the_provisional_file() {
    let (_dir, runs) = fixture();
    let pin = runs.hold().unwrap();
    pin.write_stopping(&instance(1), 7).unwrap();
    let exited = record(1, 7);
    // Another pin is held, so the write that lands the record prunes nothing and leaves the provisional file.
    let other = runs.hold().unwrap();
    runs.write(&exited, Some(pin)).unwrap();

    assert!(runs.dir.join(stopping_file_of(&instance(1))).exists());
    assert_eq!(runs.read(&instance(1)).unwrap(), Some(exited));
    drop(other);
}

#[skuld::test]
fn read_of_an_instance_with_neither_file_is_none() {
    let (_dir, runs) = fixture();
    runs.write(&record(2, 2), None).unwrap();

    assert_eq!(runs.read(&instance(1)).unwrap(), None);
}

/// A directory where the provisional file of `instance` is, so that it cannot be read. (A handle held open without
/// sharing would do too, but the files of this directory do not refuse a read to one on this machine.)
fn provisional_unreadable(runs: &Runs, instance: &InstanceId) {
    directory_with_a_file(&runs.dir.join(stopping_file_of(instance)));
}

/// A provisional file that cannot be read does not hide the record beside it: the record is the answer.
#[skuld::test]
fn read_gives_the_record_when_the_provisional_file_cannot_be_read() {
    let (_dir, runs) = fixture();
    let exited = record(1, 7);
    runs.write(&exited, None).unwrap();
    provisional_unreadable(&runs, &instance(1));

    assert_eq!(runs.read(&instance(1)).unwrap(), Some(exited));
}

/// With no record, what the provisional file's read gave is the answer, its error included.
#[skuld::test]
fn read_is_the_error_of_a_provisional_file_that_cannot_be_read_when_there_is_no_record() {
    let (_dir, runs) = fixture();
    provisional_unreadable(&runs, &instance(1));

    let read = runs.read(&instance(1)).unwrap_err();

    assert_eq!(read.kind(), io::ErrorKind::PermissionDenied, "{read}");
}

/// A provisional file holds `Stopped`, which is what its writer stores: another outcome in it is a record that does
/// not make sense, and is reported, not overwritten with `Stopped`.
#[skuld::test]
fn a_provisional_file_that_holds_another_outcome_than_stopped_is_a_corrupt_record() {
    let (_dir, runs) = fixture();
    let stored = Stored {
        started: 1,
        outcome: RunOutcome::Exited(3),
    };
    fs::write(
        runs.dir.join(stopping_file_of(&instance(1))),
        serde_json::to_vec(&stored).unwrap(),
    )
    .unwrap();

    let read = runs.read(&instance(1)).unwrap_err();

    assert_eq!(read.kind(), io::ErrorKind::Other);
    assert!(read.to_string().contains("provisional"), "{read}");
}

/// Another program holds the provisional file open without sharing delete (a scanner, say). The record is written
/// to a name nothing holds, so it lands at once, and the held provisional file is left for a later prune.
#[skuld::test]
fn a_final_write_lands_while_another_handle_holds_the_provisional_file_without_sharing_delete() {
    let (_dir, runs) = fixture();
    let pin = runs.hold().unwrap();
    pin.write_stopping(&instance(1), 1).unwrap();
    let provisional = runs.dir.join(stopping_file_of(&instance(1)));
    let holder = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&provisional)
        .unwrap();
    let exited = record(1, 1);

    let problems = runs.write(&exited, Some(pin)).unwrap();

    assert_eq!(
        problems,
        Vec::<String>::new(),
        "a file that is held is for the next prune"
    );
    assert_eq!(runs.read(&instance(1)).unwrap(), Some(exited));
    assert!(provisional.exists(), "the held provisional file is left");
    drop(holder);
    runs.write(&record(2, 2), None).unwrap();
    assert_eq!(names(&runs), [file_of(&instance(2))], "the next prune removes it");
}

/// With no pin held a write removes every record but its own, every temp file and every provisional file, its
/// own run's too: its record has landed, and the record wins.
#[skuld::test]
fn without_a_pin_a_write_prunes_every_provisional_file_the_writers_own_too() {
    let (_dir, runs) = fixture();
    let pin = runs.hold().unwrap();
    pin.write_stopping(&instance(1), 1).unwrap();
    pin.write_stopping(&instance(2), 2).unwrap();
    runs.write(&record(3, 3), None).unwrap();
    fs::write(runs.dir.join(format!("{}.json.tmp", instance(4).hex())), "").unwrap();
    drop(pin);

    runs.write(&record(1, 1), None).unwrap();

    assert_eq!(names(&runs), [file_of(&instance(1))]);
    assert_eq!(runs.read(&instance(1)).unwrap(), Some(record(1, 1)));
}

/// With a pin held no operation deletes a provisional file, a leftover one included: the record wins.
#[skuld::test]
fn with_a_pin_held_a_write_prunes_no_provisional_file() {
    let (_dir, runs) = fixture();
    let own = runs.hold().unwrap();
    own.write_stopping(&instance(1), 1).unwrap();
    own.write_stopping(&instance(2), 2).unwrap();
    let other = runs.hold().unwrap();

    runs.write(&record(1, 1), Some(own)).unwrap();
    runs.write(&record(3, 3), None).unwrap();

    assert!(
        runs.dir.join(stopping_file_of(&instance(1))).exists(),
        "the writer's own"
    );
    assert!(runs.dir.join(stopping_file_of(&instance(2))).exists(), "another run's");
    assert_eq!(runs.read(&instance(1)).unwrap(), Some(record(1, 1)), "the record wins");
    drop(other);
}

/// A provisional file the pin's listing already held is not new to its holder; one that came after it is.
#[skuld::test]
fn appeared_does_not_list_a_provisional_file_the_pin_listed_and_lists_one_that_came_after() {
    let (_dir, runs) = fixture();
    let first = runs.hold().unwrap();
    first.write_stopping(&instance(1), 1).unwrap();
    let pin = runs.pin().unwrap();
    assert_eq!(pin.appeared().unwrap(), Vec::<RunRecord>::new());

    first.write_stopping(&instance(2), 2).unwrap();

    assert_eq!(pin.appeared().unwrap(), [stopped(2, 2)]);
}

/// A record that lands after a provisional file the pin listed is new to it: the run it ended is the one the
/// holder waits for.
#[skuld::test]
fn appeared_lists_the_record_that_lands_after_a_listed_provisional_file() {
    let (_dir, runs) = fixture();
    let own = runs.hold().unwrap();
    own.write_stopping(&instance(1), 1).unwrap();
    let pin = runs.pin().unwrap();

    runs.write(&record(1, 1), Some(own)).unwrap();

    assert_eq!(pin.appeared().unwrap(), [record(1, 1)]);
}

/// One entry per instance: the record when there is one, else the provisional file.
#[skuld::test]
fn appeared_lists_one_entry_per_instance_the_record_over_the_provisional_file() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();
    let own = runs.hold().unwrap();
    own.write_stopping(&instance(1), 1).unwrap();
    own.write_stopping(&instance(2), 2).unwrap();
    runs.write(&record(1, 1), Some(own)).unwrap();

    assert_eq!(pin.appeared().unwrap(), [record(1, 1), stopped(2, 2)]);
}

/// A provisional file that cannot be read does not keep `appeared` from listing the record beside it.
#[skuld::test]
fn appeared_lists_the_record_when_the_provisional_file_beside_it_cannot_be_read() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();
    runs.write(&record(1, 1), None).unwrap();
    provisional_unreadable(&runs, &instance(1));

    assert_eq!(pin.appeared().unwrap(), [record(1, 1)]);
}

/// A provisional file is a file goetia made: the removal takes it with the rest.
#[skuld::test]
fn remove_all_removes_a_provisional_file() {
    let (_dir, runs) = fixture();
    let pin = runs.hold().unwrap();
    pin.write_stopping(&instance(1), 1).unwrap();
    drop(pin);

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
}

/// A provisional file has its own kind in a listing, and a name that is not an instance's is no provisional file.
#[skuld::test]
fn entries_tell_a_provisional_file_from_a_record_and_from_a_name_that_is_neither() {
    let (_dir, runs) = fixture();
    fs::write(runs.dir.join(stopping_file_of(&instance(1))), "").unwrap();
    fs::write(runs.dir.join(file_of(&instance(2))), "").unwrap();
    fs::write(runs.dir.join("short.stopping"), "").unwrap();

    let mut kinds: Vec<String> = entries(&Dir::open(&runs.dir).unwrap())
        .unwrap()
        .iter()
        .map(|entry| match entry {
            Entry::Stopping { instance, .. } => format!("stopping {}", instance.hex()),
            Entry::Record { instance, .. } => format!("record {}", instance.hex()),
            Entry::Other { name, .. } => format!("other {}", name.to_string_lossy()),
            Entry::Temp { .. } | Entry::Pin { .. } => "temp or pin".to_owned(),
        })
        .collect();
    kinds.sort();

    assert_eq!(
        kinds,
        [
            "other short.stopping".to_owned(),
            format!("record {}", instance(2).hex()),
            format!("stopping {}", instance(1).hex()),
        ]
    );
}

/// A write with a pin lands where the pin's handle is, not at the path.
#[skuld::test]
fn write_with_its_own_pin_writes_through_the_pins_directory() {
    let (_pinned_dir, pinned) = fixture();
    let (_other_dir, other) = fixture();
    let own = pinned.hold().unwrap();

    other.write(&record(1, 1), Some(own)).unwrap();

    assert_eq!(
        names(&pinned),
        [file_of(&instance(1))],
        "the record is where the pin was"
    );
    assert_eq!(names(&other), Vec::<String>::new(), "nothing went by the path");
}

/// A marker an uninstall that died left does not block the removal that finishes it.
#[skuld::test]
fn remove_all_is_not_blocked_by_a_marker_a_cut_short_removal_left() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    leave_marker(&runs);

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
}

/// What is not goetia's is removed too: a file of another name, and a directory with files in it.
#[skuld::test]
fn remove_all_removes_files_and_directories_of_names_that_are_not_goetias() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    fs::write(runs.dir.join("notes.txt"), "").unwrap();
    // A directory in it: made as an account that may add one.
    crate::backend::scm::test_support::directory_with_a_file(&runs.dir.join("sub"));

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
}

/// What a name says is a file: a directory or a directory link of that name is removed with its tree all the
/// same, and never followed. Each once failed every pass.
#[skuld::test]
fn remove_all_removes_a_directory_or_a_link_whose_name_is_a_pins_a_records_or_a_temps() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    let temp = format!("{}.json.tmp", instance(2).hex());
    for name in ["a.pin", temp.as_str(), &file_of(&instance(3))] {
        directory_with_a_file(&runs.dir.join(name));
    }
    junction(&runs.dir.join("b.pin"), target.path());
    junction(&runs.dir.join(file_of(&instance(4))), target.path());

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
    assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
}

/// A directory named as a record is no record: a pin's listing and what appeared since do not read it.
#[skuld::test]
fn a_directory_named_as_a_record_is_not_a_record_to_a_pin() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();
    directory_with_a_file(&runs.dir.join(file_of(&instance(1))));
    runs.write(&record(2, 2), None).unwrap();

    assert_eq!(pin.appeared().unwrap(), [record(2, 2)]);
}

/// A file another program holds is waited for, as a pin is, whoever the holder is: the removal says what
/// it waits for, naming the file, and finishes when the handle goes.
#[skuld::test]
fn remove_all_waits_for_a_file_another_program_holds_and_names_it() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    runs.write(&record(2, 2), None).unwrap();
    let held_name = file_of(&instance(2));
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(runs.dir.join(&held_name))
        .unwrap();
    let held_path = runs.dir.join(&held_name);
    let mut noted = Vec::new();
    let mut held = Some(held);

    runs.remove_all(|path, _| {
        noted.push(path.to_path_buf());
        if path == held_path {
            drop(held.take());
        }
    })
    .unwrap();

    assert_eq!(noted, [held_path], "one pause, named once");
    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
}

/// A file whose delete is pending keeps the directory from going, and a held temp file does too; the
/// removal names each as the one it waits for, when that changes: the temp file first, then the record
/// whose delete is pending, which goes when its handle does. A pause with nothing to say for it was silent.
#[skuld::test]
fn remove_all_names_each_thing_it_waits_for_as_it_changes() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let pending = runs.dir.join(file_of(&instance(2)));
    let temp = runs.dir.join(format!("{}.json.tmp", instance(3).hex()));
    let mut reader = Some(delete_pending(&pending));
    let mut writing = Some(temp_held(&runs, &temp));
    let mut noted = Vec::new();

    // Each handle goes when the path it holds is named: a note for anything else (another program's
    // hold on a file of ours) lets neither go early.
    runs.remove_all(|path, _| {
        noted.push(path.to_path_buf());
        if path == temp {
            drop(writing.take());
        } else if path == pending {
            drop(reader.take());
        }
    })
    .unwrap();

    assert_eq!(noted, [temp, pending], "each path is named once, when it changes");
    assert!(!runs.dir.exists());
}

/// A directory of a name that is not goetia's is removed with its tree. One that cannot be (a handle on a
/// file in it that shares no delete) ends the removal at once, naming the directory and not the file deep
/// in it: it is no pause, since what is wrong is not goetia's to wait out. The marker stays; once the handle
/// goes, the removal runs again and finishes.
#[skuld::test]
fn remove_all_fails_at_once_naming_a_directory_of_a_foreign_name_it_cannot_remove() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let sub = runs.dir.join("sub");
    directory_with_a_file(&sub);
    let holder = directory_held_open(&sub.join("inner"));

    let failed = runs.remove_all(no_note).unwrap_err();

    let text = failed.to_string();
    assert!(text.contains(&sub.display().to_string()), "{text}");
    assert!(!text.contains("inner"), "the top directory is named: {text}");
    assert!(runs.marker.exists());
    drop(holder);
    runs.remove_all(no_note).unwrap();
    assert!(!runs.dir.exists());
}

/// The directory itself held by a handle that shares no delete: a sharing violation, which is waited for
/// and named as a file is. Measured on windows-2022 and windows-2025.
#[skuld::test]
fn remove_all_waits_for_a_handle_on_the_directory_that_shares_no_delete_and_names_it() {
    let (_dir, runs) = fixture();
    let holder = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&runs.dir)
        .unwrap();
    let mut holder = Some(holder);
    let mut noted = Vec::new();

    runs.remove_all(|path, _| {
        noted.push(path.to_path_buf());
        if path == runs.dir {
            drop(holder.take());
        }
    })
    .unwrap();

    assert_eq!(noted, std::slice::from_ref(&runs.dir), "one pause, named once");
    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
}

/// A handle that shares delete is no obstacle: the name goes at once, as for a file, so the removal pauses
/// for nothing and has no delete-pending directory to tell.
#[skuld::test]
fn remove_all_removes_a_directory_at_once_while_a_handle_that_shares_delete_is_open() {
    let (_dir, runs) = fixture();
    let holder = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&runs.dir)
        .unwrap();

    runs.remove_all(no_note).unwrap();

    assert!(!runs.dir.exists());
    assert!(!runs.marker.exists());
    drop(holder);
}

/// The pause of a directory that is not empty names the smallest path left. A listing that comes back
/// empty (what was left went in between) pauses at the pace all the same but names nothing: no handle is
/// known to hold anything, and a note naming the directory would be false.
#[skuld::test]
fn a_directory_that_is_not_empty_pauses_on_what_is_left_and_names_nothing_when_nothing_is() {
    let dir = Path::new(r"C:\runs\frpc");
    let left = [
        Entry::Temp {
            name: "b.json.tmp".into(),
            directory: false,
        },
        Entry::Pin {
            name: "a.pin".into(),
            directory: false,
        },
        Entry::Other {
            name: "c".into(),
            directory: true,
        },
    ];

    assert!(matches!(
        pass_after_not_empty(dir, &left),
        PassOutcome::Waiting(path) if path == dir.join("a.pin")
    ));
    assert!(matches!(pass_after_not_empty(dir, &[]), PassOutcome::Retry));
}

/// What the removal says over the passes it makes ([`Said::note`]): a pause is noted when it is not what
/// the last note said, so a hold that comes back after the directory emptied is a new thing to say, and so
/// is a second phase of emptying. Driven on the sequence of passes, which a real removal makes only when
/// a file goes between a delete and a listing.
#[skuld::test]
fn a_pause_is_noted_when_it_differs_from_the_last_note_and_a_new_emptying_phase_is_noted_again() {
    let dir = Path::new(r"C:\runs\frpc");
    let (pin, record) = (dir.join("a.pin"), dir.join("b.json"));
    let passes = [
        PassOutcome::Waiting(pin.clone()),
        PassOutcome::Waiting(pin.clone()),
        PassOutcome::Retry,
        PassOutcome::Retry,
        PassOutcome::Waiting(pin.clone()),
        PassOutcome::Retry,
        PassOutcome::Waiting(record.clone()),
        PassOutcome::Waiting(record.clone()),
        PassOutcome::Done,
    ];
    let mut said = Said::Nothing;
    let mut notes = Vec::new();

    for pass in &passes {
        said.note(dir, pass, &mut |path, kind| notes.push((path.to_path_buf(), kind)));
    }

    assert_eq!(
        notes,
        [
            (pin.clone(), PauseKind::Held),
            (dir.to_path_buf(), PauseKind::Emptying),
            (pin, PauseKind::Held),
            (dir.to_path_buf(), PauseKind::Emptying),
            (record, PauseKind::Held),
        ]
    );
}

/// A removal that fails leaves its marker, so that a `run` still backs off: its records are partly gone.
/// An install, which makes them again, takes it away ([`Runs::ensure`]).
#[skuld::test]
fn a_failed_removal_leaves_its_marker_beside_the_directory() {
    let (_dir, runs) = fixture();
    let _held = directory_held_open(&runs.dir.join(file_of(&instance(1))));

    let failed = runs.remove_all(no_note).unwrap_err();

    assert!(failed.to_string().contains(&file_of(&instance(1))), "{failed}");
    assert!(runs.marker.exists());
    assert_eq!(runs.hold().err().map(|e| e.kind()), Some(io::ErrorKind::ResourceBusy));
}

/// The note is for a pause and for nothing else: records, a temp file, a stale pin and a marker a dead
/// removal left are removed without one, as no handle holds any of them.
#[skuld::test]
fn remove_all_makes_no_note_when_nothing_is_held_or_pending() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
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
    other.write(&record(1, 1), None).unwrap();

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
    assert_eq!(pin.appeared().unwrap(), []);
}

// a link where the directory should be ================================================================================

/// What the task's account can make of its empty directory (see the note of `runs::dir`): a junction to a
/// folder it cannot reach. The target is another task's
/// records directory, with a record in it and a file that is no record. The task's own empty directory is
/// replaced by the junction, as the account does it. Returns the target's `Runs`, and its temporary
/// directory, which outlasts the junction.
fn linked(runs: &Runs) -> (TempDir, Runs) {
    let (target_dir, target) = fixture();
    target.write(&record(9, 9), None).unwrap();
    fs::write(target.dir.join("victim.txt"), "keep").unwrap();
    fs::remove_dir(&runs.dir).unwrap();
    junction(&runs.dir, &target.dir);
    (target_dir, target)
}

/// Everything the target holds is still there, and nothing else.
fn target_is_untouched(target: &Runs) -> bool {
    names(target) == [file_of(&instance(9)), "victim.txt".to_owned()]
}

/// Measured: an elevated listing and delete through the mount point deleted a file in a folder only
/// Administrators can write. The removal deletes the link and reads nothing through it.
#[skuld::test]
fn remove_all_of_a_link_removes_the_link_and_leaves_what_it_points_to() {
    let (_dir, runs) = fixture();
    let (_target_dir, target) = linked(&runs);

    runs.remove_all(no_note).unwrap();

    assert!(fs::symlink_metadata(&runs.dir).is_err(), "the link is gone");
    assert!(!runs.marker.exists());
    assert!(target_is_untouched(&target), "{:?}", names(&target));
}

/// The directory the removal holds becomes a mount point after the listing and before the deletes (its
/// account does it to an empty directory), whatever the listing held, a pin first or a tree last. Every
/// delete that says so ends the pass, with nothing held and nothing failed, and the directory's own delete
/// that follows deletes the link; the target is as it was. Driven step by step, so no timing decides it.
#[skuld::test]
fn a_directory_that_becomes_a_link_after_the_listing_is_deleted_as_a_link() {
    let listings = [
        vec![Entry::Pin {
            name: "pin-a.pin".into(),
            directory: false,
        }],
        vec![Entry::Record {
            instance: instance(1),
            name: file_of(&instance(1)).into(),
            directory: false,
        }],
        vec![Entry::Temp {
            name: format!("{}.json.tmp", instance(2).hex()).into(),
            directory: false,
        }],
        vec![Entry::Other {
            name: "foreign".into(),
            directory: false,
        }],
        vec![Entry::Other {
            name: "tree".into(),
            directory: true,
        }],
        vec![Entry::Pin {
            name: "pin-b.pin".into(),
            directory: true,
        }],
    ];
    for listed in listings {
        let (_dir, runs) = fixture();
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join("victim.txt"), "keep").unwrap();
        let held = Dir::open(&runs.dir).unwrap();
        make_mount_point(&runs.dir, target.path());

        let waiting = runs.delete_entries(&held, &listed).unwrap();
        held.remove().unwrap();

        assert_eq!(waiting, None);
        assert!(fs::symlink_metadata(&runs.dir).is_err(), "the link is gone");
        assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
    }
}

#[skuld::test]
fn a_link_whose_target_is_gone_is_records_to_remove_and_is_removed() {
    let (_dir, runs) = fixture();
    let (target_dir, _target) = linked(&runs);
    drop(target_dir);

    assert!(runs.is_present());
    runs.remove_all(no_note).unwrap();

    assert!(fs::symlink_metadata(&runs.dir).is_err(), "the dangling link is gone");
}

/// An install finding a link where the directory should be refuses it and rewrites nothing it points to;
/// the uninstall that follows removes it.
#[skuld::test]
fn ensure_refuses_a_link_and_leaves_what_it_points_to_alone() {
    let (_dir, runs) = fixture();
    let (_target_dir, target) = linked(&runs);
    let before = entries_of(&target.dir);

    let refused = runs.ensure(&[process_user_sid()]).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::NotADirectory);
    assert!(
        refused.to_string().contains(&runs.dir.display().to_string()),
        "{refused}"
    );
    assert_eq!(
        entries_of(&target.dir),
        before,
        "the target's permissions are not rewritten"
    );
    assert!(target_is_untouched(&target));
}

/// `check_directory` passes a directory that is not there, and one `ensure` would only set the permissions of,
/// and changes nothing; it refuses a link as `ensure` does.
#[skuld::test]
fn check_directory_passes_what_ensure_would_not_refuse_and_refuses_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let absent = Runs::of(dir.path(), &id());
    absent.check_directory().unwrap();
    assert!(!absent.dir.exists(), "nothing is made");

    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
    let before = entries_of(&runs.dir);
    runs.check_directory().unwrap();
    assert_eq!(entries_of(&runs.dir), before, "the permissions are not set");
    assert_eq!(names(&runs), [file_of(&instance(1))]);

    let (_link_dir, linking) = fixture();
    let (_target_dir, target) = linked(&linking);
    let refused = linking.check_directory().unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::NotADirectory);
    assert!(target_is_untouched(&target));
}

/// Every other operation on the records refuses the link and touches nothing: a read does not follow it
/// to a record, and a pin or a write makes no file in its target.
#[skuld::test]
fn nothing_reads_pins_or_writes_through_a_link() {
    let (_dir, runs) = fixture();
    let (_target_dir, target) = linked(&runs);
    let before = names(&target);

    let read = runs.read(&instance(9)).unwrap();
    let pin = runs.pin().err().map(|e| e.kind());
    let hold = runs.hold().err().map(|e| e.kind());
    let write = runs.write(&record(1, 1), None).unwrap_err();

    assert_eq!(read, None, "a link holds no records");
    assert_eq!(pin, Some(io::ErrorKind::NotADirectory));
    assert_eq!(hold, Some(io::ErrorKind::NotADirectory));
    assert_eq!(write.kind(), io::ErrorKind::NotADirectory, "{write}");
    assert_eq!(names(&target), before, "nothing was made or removed in the target");
}

/// The task's directory, held open, which its account then makes a mount point to a folder with `victim.txt`
/// in it, as it can while nothing is in it. Returns the handle and the target.
fn held_and_then_linked(runs: &Runs) -> (Dir, TempDir) {
    let held = Dir::open(&runs.dir).unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    make_mount_point(&runs.dir, target.path());
    (held, target)
}

/// The account makes the directory a link after the handle opened it and before the pin is created or a
/// record is read: both answer as a link does, which a stop takes as no records, and nothing is made in the
/// target.
#[skuld::test]
fn a_directory_that_becomes_a_link_after_it_was_opened_is_a_link_to_a_pin_and_a_read() {
    let (_dir, runs) = fixture();
    let (held, target) = held_and_then_linked(&runs);

    let read = read_in(&held, &instance(9)).unwrap_err();
    let pin = runs.hold_in(held, MarkerCheck::Refuse).err().expect("a pin is refused");

    assert_eq!(read.kind(), io::ErrorKind::NotADirectory, "{read}");
    assert_eq!(pin.kind(), io::ErrorKind::NotADirectory, "{pin}");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1, "only victim.txt");
}

/// The same while the removal's marker is there: the pin is refused as one taken during the removal, which
/// a stop does not take as no records.
#[skuld::test]
fn a_pin_that_cannot_be_made_while_the_records_are_being_removed_says_so() {
    let (_dir, runs) = fixture();
    let (held, _target) = held_and_then_linked(&runs);
    leave_marker(&runs);

    let refused = runs.hold_in(held, MarkerCheck::Refuse).err().expect("a pin is refused");

    assert_eq!(refused.kind(), io::ErrorKind::ResourceBusy, "{refused}");
    assert!(refused.to_string().contains("did not finish being"), "{refused}");
}

/// The directory becomes a mount point during the removal, as its account can make it of an empty one: here
/// when the pin that held the pass up is released. The next pass finds the link and deletes it, and the target
/// is as it was.
#[skuld::test]
fn a_directory_that_becomes_a_link_between_two_passes_is_deleted_as_one() {
    let (_dir, runs) = fixture();
    let pin = runs.pin().unwrap();
    leave_marker(&runs);
    assert!(matches!(runs.remove_pass().unwrap(), PassOutcome::Waiting(_)));
    drop(pin);
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    make_mount_point(&runs.dir, target.path());

    assert!(matches!(runs.remove_pass().unwrap(), PassOutcome::Done));

    assert!(fs::symlink_metadata(&runs.dir).is_err(), "the link is gone");
    assert!(!runs.marker.exists());
    assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
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
/// and has, on the directory itself, what that is without adding a subdirectory or deleting the directory
/// and with deleting entries, so that a principal that replaces another can prune what the other wrote.
fn dacl_for(principal: crate::backend::scm::sid::Sid) -> Vec<Ace> {
    sorted(vec![
        inherited_ace(system(), FULL),
        inherited_ace(administrators(), FULL),
        ace(principal.clone(), FILES, OBJECT_INHERIT | INHERIT_ONLY),
        ace(principal, (FILES & !ADD_SUBDIRECTORY & !DELETE) | DELETE_CHILD, 0),
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

    runs.ensure(&[process_user_sid()]).unwrap();

    assert_eq!(security_of(&runs.dir).owner, administrators());
    assert_eq!(entries_of(&runs.dir), dacl_for(process_user_sid()));
}

#[skuld::test]
fn a_directory_that_is_there_gets_a_protected_dacl_naming_the_principal() {
    let (_dir, runs) = fixture();

    assert_eq!(entries_of(&runs.dir), dacl_for(process_user_sid()));
    // What lets the account that replaces another prune what the other wrote, and keeps it from
    // making a subdirectory or deleting the directory.
    let own = entries_of(&runs.dir)
        .into_iter()
        .find(|ace| ace.trustee == process_user_sid() && ace.flags == 0)
        .expect("the principal's entry for the directory itself");
    assert_ne!(own.mask & DELETE_CHILD, 0);
    assert_eq!(own.mask & ADD_SUBDIRECTORY, 0);
    assert_eq!(
        own.mask & (WRITE_ATTRIBUTES | WRITE_EA),
        0,
        "the account may not set READONLY on its directory, which would keep the removal from deleting it"
    );
    assert_eq!(own.mask, 0x1200eb, "the directory's mask, as measured");
    assert_eq!(
        own.mask & DELETE,
        0,
        "the account may not delete or delete-pend its directory"
    );
}

#[skuld::test]
fn ensure_for_another_principal_rewrites_the_dacl() {
    let (_dir, runs) = fixture();
    let first = record(1, 1);
    runs.write(&first, None).unwrap();
    // `Users` has this test's account among its members, so it still reaches what it made.
    let users = sid_from_parts(5, &[32, 545]);

    runs.ensure(std::slice::from_ref(&users)).unwrap();

    assert_eq!(entries_of(&runs.dir), dacl_for(users));
    assert_eq!(runs.read(&first.instance).unwrap(), Some(first));
    runs.write(&record(2, 2), None).unwrap();
}

/// The DACL call changes the directory and nothing it holds: the files made in it keep their trustees
/// and masks, and a hard link in it, whose target is one file that lives elsewhere, leaves that file's
/// permissions as they were. A call that applies the new DACL to what inherits it (`SetNamedSecurityInfo`,
/// `SetSecurityInfo`) rewrites both.
#[skuld::test]
fn a_change_of_principal_leaves_what_is_in_the_directory_and_a_hard_links_target_alone() {
    let (dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();
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

    runs.ensure(&[sid_from_parts(5, &[32, 545])]).unwrap();

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

/// A marker a removal left, after its directory went, is something to remove: the marker would make a
/// `run` of the id back off for good.
#[skuld::test]
fn a_marker_with_no_directory_is_something_to_remove() {
    let (_dir, runs) = fixture();
    leave_marker(&runs);
    fs::remove_dir(&runs.dir).unwrap();

    assert!(runs.is_present());
    runs.remove_all(no_note).unwrap();

    assert!(!runs.is_present());
    assert!(!runs.marker.exists());
}

/// An install makes the directory again, and with it a task that may run: a marker a removal left
/// must not make its `run` back off.
#[skuld::test]
fn ensure_clears_a_marker_a_cut_short_removal_left() {
    let (_dir, runs) = fixture();
    leave_marker(&runs);
    assert!(runs.pin().is_err());

    runs.ensure(&[process_user_sid()]).unwrap();

    assert!(runs.pin().is_ok());
    assert!(!runs.marker.exists());
}

#[skuld::test]
fn ensure_is_repeatable() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1), None).unwrap();

    runs.ensure(&[process_user_sid()]).unwrap();

    assert!(runs.read(&instance(1)).unwrap().is_some());
}

/// An account named twice is one trustee, as in the default log.
#[skuld::test]
fn ensure_names_an_account_given_twice_once() {
    let (_dir, runs) = fixture();
    let users = sid_from_parts(5, &[32, 545]);

    runs.ensure(&[users.clone(), users.clone()]).unwrap();

    assert_eq!(entries_of(&runs.dir), dacl_for(users));
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
/// owner, protection and entries) is refused, naming it, and left as it is. `Goetia` and `logs` are made
/// as an elevated install makes them, which names Administrators as their owner.
#[skuld::test]
fn a_runs_folder_that_is_there_and_is_not_as_wanted_is_refused_and_left_alone(#[fixture(scm_group)] _group: &Group) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    let root = dir.path().join("Goetia").join("runs");
    crate::backend::scm::folder::make_goetia_folders(dir.path(), |_| Ok(())).unwrap();
    fs::create_dir(&root).unwrap();

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
