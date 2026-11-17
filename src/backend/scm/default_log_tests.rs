//! The default log's file: what an install makes of it and what the shim may do to it. The files are in
//! temporary directories; a file that is there is the test account's own, so what an install sets on it
//! needs no elevation, and only making one (it names Administrators as its owner) is a test of the `scm`
//! group.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use tempfile::TempDir;

use super::*;
use crate::backend::scm::folder::logs_folder;
use crate::backend::scm::shim::acl::{DEFAULT_LOG_ACCESS, Descriptor, is_as_wanted, read_path};
use crate::backend::scm::sid::sid_from_parts;
use crate::backend::scm::test_support::set_reparse_point;
use crate::elevation::test_support::process_user_sid;
use crate::testing::groups::{Group, scm_group};

/// `IO_REPARSE_TAG_HSM2`: a tag that names no other file, which an unelevated account can set on its own
/// file. A log with any tag is not one goetia made.
const A_TAG: u32 = 0x8000_0007;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

/// A `%ProgramData%` with the logs folder made plainly, and the path of the log in it.
fn program_data_with_logs() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let logs = logs_folder(dir.path());
    fs::create_dir_all(&logs).unwrap();
    let log = path_in(&logs, "nightly");
    (dir, log)
}

fn entries_of(path: &Path) -> Vec<crate::backend::scm::shim::acl::Ace> {
    let mut entries = read_path(path).unwrap().dacl.expect("a DACL");
    entries.sort();
    entries
}

fn entries_wanted_for(account: &Sid) -> Vec<crate::backend::scm::shim::acl::Ace> {
    let mut entries = Descriptor::for_default_log(account).unwrap().wanted().unwrap().dacl.unwrap();
    entries.sort();
    entries
}

#[skuld::test]
fn the_default_log_is_named_for_its_daemon_in_the_logs_folder() {
    assert_eq!(
        path_in(Path::new(r"C:\ProgramData\Goetia\logs"), "svc"),
        PathBuf::from(r"C:\ProgramData\Goetia\logs\svc.log")
    );
}

/// The permissions are set on a log that is there, and it is not otherwise changed.
#[skuld::test]
fn a_log_that_is_there_has_its_permissions_set_to_the_account_s_append_and_nothing_else() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let me = process_user_sid();

    ensure(dir.path(), &id(), &me).unwrap();

    let security = read_path(&log).unwrap();
    assert!(security.protected);
    assert_eq!(entries_of(&log), entries_wanted_for(&me));
}

/// A reinstall as another account takes the old account's entry away and gives the new one its own.
#[skuld::test]
fn an_account_change_rewrites_the_entry_and_keeps_what_is_in_the_log() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let (first, second) = (process_user_sid(), sid_from_parts(5, &[32, 545]));
    ensure(dir.path(), &id(), &first).unwrap();

    ensure(dir.path(), &id(), &second).unwrap();

    assert_eq!(entries_of(&log), entries_wanted_for(&second));
    assert_eq!(fs::metadata(&log).unwrap().len(), 5, "what the account wrote is kept");
}

/// A log that is a reparse point is not goetia's: nothing is set on it, and it is refused naming it.
#[skuld::test]
fn a_log_that_is_a_reparse_point_is_refused_and_its_permissions_are_left_alone() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    set_reparse_point(&log, A_TAG, b"x");

    let refused = ensure(dir.path(), &id(), &process_user_sid()).unwrap_err();

    assert!(refused.report().to_string().contains(&log.display().to_string()), "{}", refused.report());
    assert!(!read_path(&log).unwrap().protected, "its permissions are as they were");
}

/// The folder is an install's, made before the file: where it is not there nothing is made.
#[skuld::test]
fn a_missing_logs_folder_is_an_error_and_nothing_is_made() {
    let dir = tempfile::tempdir().unwrap();

    let refused = ensure(dir.path(), &id(), &process_user_sid()).unwrap_err();

    assert!(refused.report().to_string().contains("nightly.log"), "{}", refused.report());
    assert!(!dir.path().join("Goetia").exists());
}

/// Making the file names Administrators as its owner, which only an elevated token may.
#[skuld::test]
fn a_log_that_is_missing_is_made_empty_with_the_whole_descriptor(#[fixture(scm_group)] _group: &Group) {
    let (dir, log) = program_data_with_logs();
    let me = process_user_sid();

    ensure(dir.path(), &id(), &me).unwrap();

    assert_eq!(fs::metadata(&log).unwrap().len(), 0);
    let wanted = Descriptor::for_default_log(&me).unwrap().wanted().unwrap();
    assert!(is_as_wanted(&read_path(&log).unwrap(), &wanted));
    ensure(dir.path(), &id(), &me).unwrap();
}

/// The shim never makes the file: one that is missing is an open that fails, and nothing is made.
#[skuld::test]
fn the_shim_does_not_make_a_missing_log() {
    let (_dir, log) = program_data_with_logs();

    let refused = open_for_append(&log).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::NotFound);
    assert!(!log.exists());
}

/// The open is for exactly what the account is granted: it appends, after what is there, and the handle
/// can do nothing else.
#[skuld::test]
fn the_shim_appends_to_a_log_through_a_handle_that_can_do_nothing_else() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "first\n").unwrap();

    let mut file = open_for_append(&log).unwrap();
    file.write_all(b"second\n").unwrap();

    assert_eq!(fs::read_to_string(&log).unwrap(), "first\nsecond\n");
    let mut read = [0u8; 1];
    let denied = file.read(&mut read).unwrap_err();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied, "the handle cannot read");
    assert_eq!(DEFAULT_LOG_ACCESS, 0x10_0084);
}

/// A log that is a reparse point is refused on the shim's own handle, which a link set after the install
/// checked it would otherwise lead to anywhere.
#[skuld::test]
fn the_shim_refuses_a_log_that_is_a_reparse_point() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    set_reparse_point(&log, A_TAG, b"x");

    let refused = open_for_append(&log).unwrap_err();

    assert!(refused.to_string().contains(&log.display().to_string()), "{refused}");
}
