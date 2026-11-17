//! The file operations on entries of a directory, over a temporary directory.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_WRITE};

use super::*;
use crate::backend::scm::shim::acl::acl_tests::windows::junction;
use crate::backend::scm::test_support::{delete_pending, directory_with_a_file};

fn open(dir: &Path) -> Dir {
    Dir::open(dir).unwrap()
}

#[skuld::test]
fn a_pin_with_a_handle_open_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let held_in = open(dir.path());
    let holder = create_pin(&held_in, OsStr::new("pin-a.pin")).unwrap();

    assert!(is_held(&held_in, OsStr::new("pin-a.pin")).unwrap());
    drop(holder);
    assert!(!path.exists(), "a pin goes when its holder closes it");
}

/// The holder shares no delete, so another open for `DELETE` (what a delete is) is a sharing violation and
/// the file stays, though it is deleted when the holder closes it.
#[skuld::test]
fn a_held_pin_cannot_be_opened_for_delete_or_removed_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let holder = create_pin(&open(dir.path()), OsStr::new("pin-a.pin")).unwrap();

    let opened = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&path)
        .unwrap_err();
    let removed = fs::remove_file(&path).unwrap_err();

    assert!(is_sharing_violation(&opened), "{opened:?}");
    assert!(is_sharing_violation(&removed), "{removed:?}");
    assert!(path.exists());
    drop(holder);
    assert!(!path.exists());
}

#[skuld::test]
fn a_pin_nothing_holds_is_stale_and_goes_and_one_that_is_gone_is_not_held() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    fs::write(&path, "").unwrap();
    let held_in = open(dir.path());

    assert!(!is_held(&held_in, OsStr::new("pin-a.pin")).unwrap());
    assert!(!path.exists(), "the stale pin is deleted");
    assert!(
        !is_held(&held_in, OsStr::new("pin-a.pin")).unwrap(),
        "a pin that is gone is not held"
    );
}

/// Measured: a file whose delete is pending answers a delete with access denied, as a permission that
/// refuses it does (the Win32 error is 5 for both); only the status under it, `STATUS_DELETE_PENDING`,
/// tells them apart. It is deleted as far as anything can do it: it goes when the last handle closes.
/// A delete that fails for any other reason is a failure.
#[skuld::test]
fn a_file_whose_delete_is_pending_is_deleted_and_any_other_failure_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let reader = delete_pending(&path);
    let held_in = open(dir.path());

    let pending = delete(&held_in, OsStr::new("pin-a.pin"));
    let pin = is_held(&held_in, OsStr::new("pin-a.pin"));

    assert!(matches!(pending, DeleteOutcome::Deleted));
    assert!(!pin.unwrap(), "a pin whose delete is pending is not held");
    drop(reader);
    assert!(!path.exists());

    let refused = dir.path().join("pin-b.pin");
    directory_with_a_file(&refused);
    assert!(
        matches!(delete(&held_in, OsStr::new("pin-b.pin")), DeleteOutcome::Failed(_)),
        "a delete that fails for no handle and no pending delete is a failure"
    );
    assert!(is_held(&held_in, OsStr::new("pin-b.pin")).is_err());
}

/// The path's length is no part of the verdict: a records directory under a relocated `%ProgramData%`, or
/// of a long id, can pass `MAX_PATH`, and goetia has no long-path manifest.
#[skuld::test]
fn a_file_whose_delete_is_pending_is_deleted_at_any_path_length() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir
        .path()
        .join("a".repeat(100))
        .join("b".repeat(100))
        .join("c".repeat(100));
    fs::create_dir_all(&deep).unwrap();
    let path = deep.join("pin-a.pin");
    assert!(path.as_os_str().len() > 300, "the path is past MAX_PATH");
    let reader = delete_pending(&path);

    let outcome = delete(&open(&deep), OsStr::new("pin-a.pin"));

    assert!(matches!(outcome, DeleteOutcome::Deleted));
    drop(reader);
    assert!(!path.exists());
}

/// A directory opened through a device path (`%ProgramData%` given as `\\.\C:\...`) gives the same verdicts
/// as through any path: the file is named by itself, relative to the handle.
#[skuld::test]
fn a_device_path_gives_the_same_verdicts_as_a_drive_path() {
    let dir = tempfile::tempdir().unwrap();
    let device = |path: &Path| PathBuf::from(format!(r"\\.\{}", path.display()));
    let held_in = open(&device(dir.path()));

    let pending = dir.path().join("pin-a.pin");
    let reader = delete_pending(&pending);
    assert!(matches!(delete(&held_in, OsStr::new("pin-a.pin")), DeleteOutcome::Deleted));
    drop(reader);

    let refused = dir.path().join("pin-b.pin");
    directory_with_a_file(&refused);
    assert!(
        matches!(delete(&held_in, OsStr::new("pin-b.pin")), DeleteOutcome::Failed(_)),
        "a refusal is a failure, not a file that is gone"
    );
}

#[skuld::test]
fn a_file_a_handle_holds_without_sharing_delete_is_held_and_one_that_is_gone_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let held_in = open(dir.path());
    let holder = create_temp(&held_in, OsStr::new("f.tmp")).unwrap();

    assert!(matches!(delete(&held_in, OsStr::new("f.tmp")), DeleteOutcome::Held));
    drop(holder);
    assert!(matches!(delete(&held_in, OsStr::new("f.tmp")), DeleteOutcome::Deleted));
    assert!(
        matches!(delete(&held_in, OsStr::new("f.tmp")), DeleteOutcome::Deleted),
        "a file that is not there"
    );
}

/// A temp file an interrupted write left is deleted and made new, not overwritten: the account has no
/// right to overwrite (KB ddcbd14), and nothing of the leftover is kept.
#[skuld::test]
fn a_leftover_temp_file_is_replaced_by_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.tmp"), "left over").unwrap();

    let _file = create_temp(&open(dir.path()), OsStr::new("f.tmp")).unwrap();

    assert_eq!(fs::metadata(dir.path().join("f.tmp")).unwrap().len(), 0, "the file is new");
}

/// A tree is removed through handles opened relative to each directory, and a link in it is deleted and
/// not followed: what it points to stays.
#[skuld::test]
fn a_tree_is_removed_with_the_links_in_it_deleted_and_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("inner")).unwrap();
    fs::write(tree.join("inner").join("file"), "").unwrap();
    junction(&tree.join("inner").join("link"), target.path());

    remove_tree(&open(dir.path()), OsStr::new("tree")).unwrap();

    assert!(!tree.exists());
    assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
}
