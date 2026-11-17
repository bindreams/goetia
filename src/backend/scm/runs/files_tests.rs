//! The file operations on entries of a directory, over a temporary directory.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{FILE_APPEND_DATA, FILE_SHARE_DELETE, FILE_SHARE_WRITE, FILE_WRITE_DATA};

use super::*;
use crate::backend::scm::shim::acl::acl_tests::windows::junction;
use crate::backend::scm::test_support::{delete_pending, directory_with_a_file, make_mount_point};

fn open(dir: &Path) -> Dir {
    Dir::open(dir).unwrap()
}

#[skuld::test]
fn a_pin_with_a_handle_open_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let held_in = open(dir.path());
    let holder = create_pin(&held_in, OsStr::new("pin-a.pin")).unwrap();

    assert!(matches!(delete(&held_in, OsStr::new("pin-a.pin")), DeleteOutcome::Held));
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

    assert!(matches!(
        delete(&held_in, OsStr::new("pin-a.pin")),
        DeleteOutcome::Deleted
    ));
    assert!(!path.exists(), "the stale pin is deleted");
    assert!(
        matches!(delete(&held_in, OsStr::new("pin-a.pin")), DeleteOutcome::Deleted),
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
    let again = delete(&held_in, OsStr::new("pin-a.pin"));

    assert!(matches!(pending, DeleteOutcome::Deleted));
    assert!(
        matches!(again, DeleteOutcome::Deleted),
        "a pin whose delete is pending is not held"
    );
    drop(reader);
    assert!(!path.exists());

    let refused = dir.path().join("pin-b.pin");
    directory_with_a_file(&refused);
    assert!(
        matches!(delete(&held_in, OsStr::new("pin-b.pin")), DeleteOutcome::Failed(_)),
        "a delete that fails for no handle and no pending delete is a failure"
    );
    assert!(matches!(
        delete(&held_in, OsStr::new("pin-b.pin")),
        DeleteOutcome::Failed(_)
    ));
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
    assert!(matches!(
        delete(&held_in, OsStr::new("pin-a.pin")),
        DeleteOutcome::Deleted
    ));
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

/// The access a handle holds on its file.
fn granted(file: &fs::File) -> u32 {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_ACCESS_INFORMATION, FileAccessInformation, NtQueryInformationFile,
    };
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut info = FILE_ACCESS_INFORMATION::default();
    let mut block = IO_STATUS_BLOCK::default();
    // SAFETY: `info` is `size_of::<FILE_ACCESS_INFORMATION>()` writable bytes; the handle is live.
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut block,
            (&raw mut info).cast(),
            size_of::<FILE_ACCESS_INFORMATION>() as u32,
            FileAccessInformation,
        )
    };
    assert!(status >= 0, "{status:#x}");
    info.AccessFlags
}

/// A creating open is granted what it asks for, whatever the directory's entry for the account says, so
/// the files the account's shim makes are as capable as those opens ask. The temp file: write and append,
/// and delete for the rename; the pin: delete, for its delete-on-close. Both `SYNCHRONIZE`, for the
/// synchronous handle. Not the right to set attributes or extended attributes, which a task's account has
/// no entry for.
#[skuld::test]
fn the_temp_file_and_the_pin_are_created_for_what_they_use_and_no_more() {
    let dir = tempfile::tempdir().unwrap();
    let held_in = open(dir.path());

    let temp = create_temp(&held_in, OsStr::new("f.json.tmp")).unwrap();
    let pin = create_pin(&held_in, OsStr::new("pin-a.pin")).unwrap();

    assert_eq!(
        granted(&temp),
        FILE_WRITE_DATA | FILE_APPEND_DATA | DELETE | SYNCHRONIZE,
        "the temp file"
    );
    assert_eq!(granted(&pin), DELETE | SYNCHRONIZE, "the pin");
}

/// A temp file an interrupted write left is deleted and made new, not overwritten: the account has no
/// right to overwrite (KB ddcbd14), and nothing of the leftover is kept.
#[skuld::test]
fn a_leftover_temp_file_is_replaced_by_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.tmp"), "left over").unwrap();

    let _file = create_temp(&open(dir.path()), OsStr::new("f.tmp")).unwrap();

    assert_eq!(
        fs::metadata(dir.path().join("f.tmp")).unwrap().len(),
        0,
        "the file is new"
    );
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

    assert!(matches!(
        remove_tree(&open(dir.path()), OsStr::new("tree")),
        DeleteOutcome::Deleted
    ));

    assert!(!tree.exists());
    assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
}

/// A directory held open, emptied, and made a mount point by its account between a listing and the deletes
/// that follow: `held` is the handle and `stale` what the listing named. Nothing of the target is reachable
/// through the handle. The target is returned, with `victim.txt` in it.
fn held_directory_turned_into_a_link(dir: &Path, stale: &[&str]) -> (Dir, tempfile::TempDir) {
    let held = dir.join("held");
    fs::create_dir(&held).unwrap();
    let handle = open(&held);
    for name in stale {
        fs::write(held.join(name), "").unwrap();
    }
    let target = tempfile::tempdir().unwrap();
    for name in stale {
        fs::write(target.path().join(name), "keep").unwrap();
        fs::remove_file(held.join(name)).unwrap();
    }
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    make_mount_point(&held, target.path());
    (handle, target)
}

/// What the removal does when the directory it holds became a link under it: a delete is no failure and no
/// wait, it says so (`STATUS_REPARSE_POINT_NOT_RESOLVED`), and nothing of the target is touched.
#[skuld::test]
fn a_delete_in_a_directory_that_became_a_link_says_so_and_touches_nothing_of_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let (held, target) = held_directory_turned_into_a_link(dir.path(), &["a.json", "pin-a.pin"]);

    assert!(matches!(delete(&held, OsStr::new("a.json")), DeleteOutcome::Linked));
    assert!(matches!(delete(&held, OsStr::new("pin-a.pin")), DeleteOutcome::Linked));
    assert!(matches!(remove_tree(&held, OsStr::new("sub")), DeleteOutcome::Linked));

    for name in ["a.json", "pin-a.pin", "victim.txt"] {
        assert!(target.path().join(name).exists(), "{name}");
    }
}

/// A directory of the tree that became a link between its listing and the deletes is deleted as a link,
/// and what it points to stays: the rest of that listing is skipped.
#[skuld::test]
fn a_directory_of_a_tree_that_became_a_link_is_deleted_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let (held, target) = held_directory_turned_into_a_link(dir.path(), &["a.json"]);
    let stale = [
        Listed {
            name: "a.json".into(),
            directory: false,
        },
        Listed {
            name: "b.json".into(),
            directory: false,
        },
    ];

    remove_entries(&held, &stale).unwrap();
    held.remove().unwrap();

    assert!(!dir.path().join("held").exists(), "the link is deleted");
    assert!(target.path().join("a.json").exists());
    assert!(target.path().join("victim.txt").exists());
}

/// The one reading of a delete as a result: a held file and a directory that became a link are errors.
#[skuld::test]
fn a_delete_outcome_that_is_no_deletion_is_an_error() {
    assert!(DeleteOutcome::Deleted.into_result().is_ok());
    assert!(is_sharing_violation(&DeleteOutcome::Held.into_result().unwrap_err()));
    assert_eq!(
        DeleteOutcome::Failed(io::Error::from(io::ErrorKind::PermissionDenied))
            .into_result()
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        DeleteOutcome::Linked.into_result().unwrap_err().kind(),
        io::ErrorKind::NotADirectory
    );
}
