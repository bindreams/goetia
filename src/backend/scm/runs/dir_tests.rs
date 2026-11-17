//! The handle on a records directory, over temporary directories and junctions.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Win32::Foundation::STATUS_REPARSE_POINT_NOT_RESOLVED;

use super::*;
use crate::backend::scm::shim::acl::acl_tests::windows::junction;

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

/// What the whole design stands on (measured, KB 78c82b1): a handle on a mount point is on the link itself,
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
