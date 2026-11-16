//! `Runs` over a temporary directory. Every test makes its own, removed when the test ends.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::Path;

use tempfile::TempDir;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, READ_CONTROL};

use super::*;
use crate::backend::scm::shim::acl::{Ace, ObjectSecurity, read_object};
use crate::backend::scm::sid::{administrators, sid_from_parts, system};
use crate::elevation::test_support::process_user_sid;

const FULL: u32 = 0x1F_01FF;
const MODIFY: u32 = 0x13_01BF;
const INHERITED_BY_FILES_AND_FOLDERS: u8 = 0x03;

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

/// A task's directory, made for the account this test runs as, in a directory that is removed with
/// `dir` (after `runs` and anything declared later).
fn fixture() -> (TempDir, Runs) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    runs.ensure(&process_user_sid()).unwrap();
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
        runs.ensure(&process_user_sid()).unwrap();
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
fn a_write_in_flight_keeps_its_temp_file_from_a_prune() {
    let (_dir, runs) = fixture();
    // Another run's write, between creating its temp file and renaming it.
    let in_flight = runs.dir.join(format!("{}.json.tmp", instance(2).hex()));
    let held = files::create_temp(&in_flight).unwrap();

    runs.write(&record(1, 1)).unwrap();

    assert!(in_flight.exists(), "a prune cannot delete what a write holds");
    drop(held);
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

#[skuld::test]
fn remove_all_clears_the_directory_and_its_pins() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    fs::write(runs.dir.join("pin-stale.pin"), "").unwrap();
    fs::write(runs.dir.join(format!("{}.json.tmp", instance(2).hex())), "").unwrap();

    runs.remove_all().unwrap();

    assert!(!runs.dir.exists());
    runs.remove_all().unwrap();
}

#[skuld::test]
fn remove_all_while_a_run_holds_a_pin_removes_the_directory_and_the_pins_reader_is_told_why() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();
    let pin = runs.pin().unwrap();

    runs.remove_all().unwrap();

    assert!(!runs.dir.exists());
    let error = runs.appeared(&pin).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let text = error.to_string();
    assert!(text.contains("uninstalled"), "{text}");
    assert!(text.contains(&runs.dir.display().to_string()), "{text}");
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

fn inherited_ace(trustee: crate::backend::scm::sid::Sid, mask: u32) -> Ace {
    Ace {
        trustee,
        mask,
        flags: INHERITED_BY_FILES_AND_FOLDERS,
        allow: true,
        conditional: false,
    }
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

#[skuld::test]
fn the_directory_is_created_with_a_protected_dacl_naming_the_principal() {
    let (_dir, runs) = fixture();
    let me = process_user_sid();

    assert_eq!(
        entries_of(&runs.dir),
        sorted(vec![
            inherited_ace(system(), FULL),
            inherited_ace(administrators(), FULL),
            inherited_ace(me, MODIFY),
        ])
    );
}

#[skuld::test]
fn ensure_for_another_principal_rewrites_the_dacl() {
    let (_dir, runs) = fixture();
    let first = record(1, 1);
    runs.write(&first).unwrap();
    // `Users` has this test's account among its members, so it still reaches what it made.
    let users = sid_from_parts(5, &[32, 545]);

    runs.ensure(&users).unwrap();

    assert_eq!(
        entries_of(&runs.dir),
        sorted(vec![
            inherited_ace(system(), FULL),
            inherited_ace(administrators(), FULL),
            inherited_ace(users, MODIFY),
        ])
    );
    assert_eq!(runs.read(&first.instance).unwrap(), Some(first));
    runs.write(&record(2, 2)).unwrap();
}

#[skuld::test]
fn ensure_is_repeatable() {
    let (_dir, runs) = fixture();
    runs.write(&record(1, 1)).unwrap();

    runs.ensure(&process_user_sid()).unwrap();

    assert!(runs.read(&instance(1)).unwrap().is_some());
}
