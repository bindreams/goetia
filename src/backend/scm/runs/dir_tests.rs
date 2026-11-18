//! The handle on a records directory, over temporary directories and junctions.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Win32::Foundation::{STATUS_OBJECT_NAME_INVALID, STATUS_REPARSE_POINT_NOT_RESOLVED};

use super::*;
use crate::backend::scm::shim::acl::acl_tests::windows::junction;
use crate::backend::scm::test_support::{make_mount_point, set_reparse_point};

/// A junction `link` in `dir`, to a folder holding the file `victim.txt`. Returns that folder, which
/// is removed with the tempdir.
fn linked(dir: &Path) -> tempfile::TempDir {
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    junction(&dir.join("link"), target.path());
    target
}

#[skuld::test]
fn a_link_is_refused_by_open_and_opened_itself_by_open_even_if_link() {
    let dir = tempfile::tempdir().unwrap();
    let _target = linked(dir.path());
    let link = dir.path().join("link");

    let refused = Dir::open(&link).err().unwrap();
    let opened = Dir::open_even_if_link(&link).unwrap();

    assert_eq!(refused.kind(), io::ErrorKind::NotADirectory);
    assert!(refused.to_string().contains(&link.display().to_string()), "{refused}");
    assert!(opened.is_link());
    assert!(!Dir::open(dir.path()).unwrap().is_link());
}

/// What the whole design stands on (measured): a handle on a mount point is on the link itself,
/// so a listing through it is empty and an open relative to it reaches nothing of the target.
#[skuld::test]
fn nothing_of_a_links_target_is_listed_or_opened_through_a_handle_on_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let _target = linked(dir.path());
    let link = Dir::open_even_if_link(&dir.path().join("link")).unwrap();

    let listed = link.list().unwrap();
    let opened = link.open_in(OsStr::new("victim.txt"), FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0);

    assert!(listed.is_empty(), "the target's files are not listed");
    assert_eq!(opened.err(), Some(STATUS_REPARSE_POINT_NOT_RESOLVED));
}

/// The directory the handle is held on becomes a mount point while it is held, as its account can make it:
/// the handle is on the link, so a listing is empty and a relative open fails with the status that says so,
/// which the removal reads (`STATUS_REPARSE_POINT_NOT_RESOLVED`).
#[skuld::test]
fn a_directory_that_becomes_a_mount_point_while_held_lists_nothing_and_opens_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let held = dir.path().join("held");
    fs::create_dir(&held).unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("victim.txt"), "keep").unwrap();
    let handle = Dir::open(&held).unwrap();

    make_mount_point(&held, target.path());

    assert!(handle.list().unwrap().is_empty());
    assert_eq!(
        handle
            .open_in(OsStr::new("victim.txt"), FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0)
            .err(),
        Some(STATUS_REPARSE_POINT_NOT_RESOLVED)
    );
    assert!(Dir::open_even_if_link(&held).unwrap().is_link(), "a new open sees it");
    handle.remove().unwrap();
    assert!(!held.exists());
    assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
}

/// Only a tag that names a surrogate of another name (a mount point, a symlink) is a link: a directory
/// with any other tag is a directory of goetia's, listed and emptied through its handle. The tag values are
/// Windows's (`ntifs.h`).
#[skuld::test]
fn only_a_name_surrogate_tag_is_a_link() {
    for (tag, link) in [
        (0xA000_0003u32, true), // IO_REPARSE_TAG_MOUNT_POINT
        (0xA000_000C, true),    // IO_REPARSE_TAG_SYMLINK
        (0x8000_0007, false),   // IO_REPARSE_TAG_HSM2
        (0x9000_001A, false),   // IO_REPARSE_TAG_CLOUD_6
        (0x8000_0023, false),   // IO_REPARSE_TAG_AF_UNIX
        (0x0000_0000, false),
    ] {
        assert_eq!(is_name_surrogate(tag), link, "{tag:#x}");
    }
}

/// The same on a real directory, where this host lets an unelevated account set a tag that is no
/// surrogate: the handle does not call it a link.
#[skuld::test]
fn a_directory_with_a_tag_that_is_no_name_surrogate_is_listed_and_removed_like_any() {
    let dir = tempfile::tempdir().unwrap();
    let tagged = dir.path().join("tagged");
    fs::create_dir(&tagged).unwrap();
    let handle = Dir::open(&tagged).unwrap();
    set_reparse_point(&tagged, 0x8000_0007, b"x");

    let reopened = Dir::open(&tagged).expect("a directory with a tag that is no surrogate is not refused");
    assert!(!reopened.is_link());
    assert!(!Dir::open_even_if_link(&tagged).unwrap().is_link());
    assert!(reopened.list().unwrap().is_empty());
    drop(handle);
    reopened.remove().unwrap();
    assert!(!tagged.exists());
}

/// A name with a NUL in it is refused, not cut at it: the entry `a` would be opened for `a\0b`.
#[skuld::test]
fn a_name_with_a_nul_is_refused_and_not_cut() {
    use std::os::windows::ffi::OsStringExt as _;

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a"), "").unwrap();
    let name = OsString::from_wide(&[u16::from(b'a'), 0, u16::from(b'b')]);

    let opened = Dir::open(dir.path())
        .unwrap()
        .open_in(&name, FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0);

    assert_eq!(opened.err(), Some(STATUS_OBJECT_NAME_INVALID));
}

/// A directory that is empty is deleted through the second handle; a link is deleted itself and what it
/// points to stays.
#[skuld::test]
fn remove_deletes_the_directory_or_the_link_and_not_what_a_link_points_to() {
    let dir = tempfile::tempdir().unwrap();
    let target = linked(dir.path());
    fs::create_dir(dir.path().join("plain")).unwrap();

    Dir::open_even_if_link(&dir.path().join("link"))
        .unwrap()
        .remove()
        .unwrap();
    Dir::open(&dir.path().join("plain")).unwrap().remove().unwrap();

    assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
    assert!(!dir.path().join("plain").exists());
    assert_eq!(fs::read(target.path().join("victim.txt")).unwrap(), b"keep");
}

/// The name the handle was opened by is now another directory: the delete is refused, and the replacement
/// is not touched.
#[skuld::test]
fn remove_refuses_a_directory_whose_name_was_replaced_while_it_was_held() {
    let dir = tempfile::tempdir().unwrap();
    let held = dir.path().join("held");
    fs::create_dir(&held).unwrap();
    let handle = Dir::open(&held).unwrap();
    fs::rename(&held, dir.path().join("moved")).unwrap();
    fs::create_dir(&held).unwrap();

    let refused = handle.remove().unwrap_err();

    assert!(refused.to_string().contains("was replaced"), "{refused}");
    assert!(
        held.is_dir(),
        "the replacement, which is empty and would have gone, is there"
    );
    assert!(dir.path().join("moved").is_dir());
}

#[skuld::test]
fn a_directory_that_is_not_empty_is_not_removed() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("plain")).unwrap();
    fs::write(dir.path().join("plain").join("file"), "").unwrap();

    let refused = Dir::open(&dir.path().join("plain")).unwrap().remove().unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::DirectoryNotEmpty);
    assert!(dir.path().join("plain").join("file").exists());
}

#[skuld::test]
fn the_listing_names_entries_and_tells_directories() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("file.json"), "").unwrap();

    let mut listed: Vec<(String, bool)> = Dir::open(dir.path())
        .unwrap()
        .list()
        .unwrap()
        .into_iter()
        .map(|entry| (entry.name.to_string_lossy().into_owned(), entry.directory))
        .collect();
    listed.sort();

    assert_eq!(listed, [("file.json".to_owned(), false), ("sub".to_owned(), true)]);
}
