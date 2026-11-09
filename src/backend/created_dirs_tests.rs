use std::fs;

use super::*;

/// Creates and records `root/a/b/c`.
fn create_chain(root: &Path) -> CreatedDirs {
    let mut created = CreatedDirs::new();
    let mut dir = root.to_path_buf();
    for name in ["a", "b", "c"] {
        dir.push(name);
        fs::create_dir(&dir).unwrap();
        created.record(&dir);
    }
    created
}

#[skuld::test]
fn drop_removes_every_recorded_directory_deepest_first() {
    let tmp = tempfile::tempdir().unwrap();

    drop(create_chain(tmp.path()));

    assert!(!tmp.path().join("a").exists());
}

#[skuld::test]
fn drop_leaves_a_directory_that_existed_before() {
    let tmp = tempfile::tempdir().unwrap();
    let before = tmp.path().join("a");
    fs::create_dir(&before).unwrap();
    let mut created = CreatedDirs::new();
    let new = before.join("b");
    fs::create_dir(&new).unwrap();
    created.record(&new);

    drop(created);

    assert!(before.is_dir());
    assert!(!new.exists());
}

#[skuld::test]
fn drop_leaves_a_directory_that_is_no_longer_empty_and_its_ancestors() {
    let tmp = tempfile::tempdir().unwrap();
    let created = create_chain(tmp.path());
    let file = tmp.path().join("a/b/c/data");
    fs::write(&file, b"x").unwrap();

    drop(created);

    assert_eq!(fs::read(&file).unwrap(), b"x");
}

/// A path that is not a directory cannot be tracked: reported, never a panic, and left.
#[skuld::test]
fn record_leaves_a_path_that_is_not_a_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("not-a-dir");
    fs::write(&file, b"x").unwrap();
    let mut created = CreatedDirs::new();
    created.record(&file);

    drop(created);

    assert!(file.is_file());
}

/// The account a daemon runs as owns what the install created for it and can swap a recorded
/// directory for a symlink before the drop; the drop, which runs as root, must not follow it.
#[skuld::test]
fn drop_does_not_follow_a_symlink_swapped_in_for_a_recorded_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::create_dir(other.path().join("c")).unwrap();
    let created = create_chain(tmp.path());
    fs::rename(tmp.path().join("a/b"), tmp.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(other.path(), tmp.path().join("a/b")).unwrap();

    drop(created);

    assert!(other.path().join("c").is_dir(), "the symlink's target was emptied");
}

/// What the name refers to now is not what the install created, so it is left.
#[skuld::test]
fn drop_leaves_a_directory_that_is_no_longer_the_one_created() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::new();
    let dir = tmp.path().join("a");
    fs::create_dir(&dir).unwrap();
    created.record(&dir);
    fs::rename(&dir, tmp.path().join("moved")).unwrap();
    fs::create_dir(&dir).unwrap();

    drop(created);

    assert!(dir.is_dir());
}

#[skuld::test]
fn settle_keeps_every_recorded_directory_when_the_definition_stands() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = create_chain(tmp.path());

    created.settle(Path::new("unit"), Standing::Yes);
    drop(created);

    assert!(tmp.path().join("a/b/c").is_dir());
}

#[skuld::test]
fn settle_removes_the_directories_when_the_definition_does_not_stand() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = create_chain(tmp.path());

    created.settle(Path::new("unit"), Standing::No);
    drop(created);

    assert!(!tmp.path().join("a").exists());
}

/// A read that did not complete says nothing about the definition; the directories stay.
#[skuld::test]
fn settle_keeps_the_directories_when_the_read_did_not_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = create_chain(tmp.path());

    created.settle(Path::new("unit"), Standing::Unknown("permission denied".to_string()));
    drop(created);

    assert!(tmp.path().join("a/b/c").is_dir());
}

/// A panic may come after the definition was written: the directories of an installed service stay.
#[skuld::test]
fn drop_keeps_the_directories_while_a_panic_unwinds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    let unwound = std::panic::catch_unwind(move || {
        let _created = create_chain(&root);
        panic!("a step after the write");
    });

    assert!(unwound.is_err());
    assert!(tmp.path().join("a/b/c").is_dir());
}
