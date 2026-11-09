use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use nix::unistd::{Gid, Uid};

use super::*;
use crate::backend::test_elevation::{ELEVATED, elevated};

/// Creates `root/a/b/c`, all of it recorded.
fn create_chain(root: &Path) -> CreatedDirs {
    let mut created = CreatedDirs::new();
    created.create(&root.join("a/b/c"), None).unwrap();
    created
}

/// An owner for this process's uid and a group other than the temp directory's, so a `fchown` is
/// visible in the result. Elevated tests only: root may hand a directory to any group.
fn other_owner(tmp: &Path) -> Owner {
    let inherited = fs::metadata(tmp).unwrap().gid();
    Owner {
        uid: Uid::current(),
        gid: Some(Gid::from_raw(inherited + 1)),
    }
}

fn mode_and_gid(path: &Path) -> (u32, u32) {
    let meta = fs::metadata(path).unwrap();
    (meta.permissions().mode() & 0o777, meta.gid())
}

#[skuld::test]
fn drop_removes_every_created_directory_deepest_first() {
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
    created.create(&new, None).unwrap();

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

/// The account a daemon runs as owns what the install created for it and can swap a created
/// directory for a symlink before the drop; the drop, which runs as root, must not follow it.
#[skuld::test]
fn drop_does_not_follow_a_symlink_swapped_in_for_a_created_directory() {
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
    created.create(&dir, None).unwrap();
    fs::rename(&dir, tmp.path().join("moved")).unwrap();
    fs::create_dir(&dir).unwrap();

    drop(created);

    assert!(dir.is_dir());
}

#[skuld::test]
fn settle_keeps_every_created_directory_when_the_definition_stands() {
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

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_leaves_an_existing_directory_exactly_as_found() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let dir = tmp.path().join("existing");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(&dir);

    CreatedDirs::new().create(&dir, Some(owner)).unwrap();

    assert_eq!(mode_and_gid(&dir), before);
    assert_eq!(before.0, 0o700);
}

/// The mode and owner land on the inode that was created: read back through the guard's own
/// descriptor, which is the inode the path names.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_hands_every_created_directory_to_the_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let mut created = CreatedDirs::new();

    created.create(&tmp.path().join("a/b"), Some(owner)).unwrap();

    assert_eq!(created.0.len(), 2);
    for entry in &created.0 {
        let held = fstat(&entry.dir).unwrap();
        assert_eq!(held.st_mode as u32 & 0o777, 0o755, "{}", entry.path.display());
        assert_eq!(held.st_gid, owner.gid.unwrap().as_raw(), "{}", entry.path.display());
        let named = fs::metadata(&entry.path).unwrap();
        assert_eq!((named.dev(), named.ino()), (held.st_dev as u64, held.st_ino as u64));
    }
}

#[skuld::test]
fn create_without_an_owner_changes_no_ownership() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::new();

    created.create(&tmp.path().join("a"), None).unwrap();

    let parent = fs::metadata(tmp.path()).unwrap();
    let made = fs::metadata(tmp.path().join("a")).unwrap();
    assert_eq!((made.uid(), made.gid()), (parent.uid(), parent.gid()));
}

/// The account a daemon runs as owns the parent it was handed and can plant a symlink at the name
/// about to be created. `mkdirat` fails on it with `EEXIST`, so the symlink's target is never
/// chmodded or chowned.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_leaves_a_symlink_planted_at_the_last_name() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let other = tempfile::tempdir().unwrap();
    fs::set_permissions(other.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(other.path());
    let dir = tmp.path().join("a");
    let found = walk(&dir).unwrap();
    std::os::unix::fs::symlink(other.path(), &dir).unwrap();
    let mut created = CreatedDirs::new();

    created.create_in(found, Some(owner)).unwrap();

    assert_eq!(mode_and_gid(other.path()), before);
    assert!(created.0.is_empty());
}

/// The same on a name above the last: the symlink is not descended into.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_does_not_descend_into_a_symlink_planted_above_the_last_name() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let other = tempfile::tempdir().unwrap();
    let found = walk(&tmp.path().join("a/b")).unwrap();
    std::os::unix::fs::symlink(other.path(), tmp.path().join("a")).unwrap();
    let mut created = CreatedDirs::new();

    created
        .create_in(found, Some(owner))
        .expect_err("a symlink is not a directory");

    assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
}

/// A component another install creates after the walk is not ours to chmod or chown; those below
/// it still are.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_leaves_a_component_that_appeared_after_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let raced = tmp.path().join("a");
    let leaf = raced.join("b");
    let found = walk(&leaf).unwrap();
    let missing: Vec<_> = found.missing.iter().map(|(path, _)| path.as_path()).collect();
    assert_eq!(missing, [raced.as_path(), leaf.as_path()]);
    fs::create_dir(&raced).unwrap();
    fs::set_permissions(&raced, fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(&raced);

    let mut created = CreatedDirs::new();

    created.create_in(found, Some(owner)).unwrap();

    assert_eq!(mode_and_gid(&raced), before);
    assert_eq!(mode_and_gid(&leaf), (0o755, owner.gid.unwrap().as_raw()));
}

/// A path that exists, of any kind and a dangling symlink included, is left untouched.
#[skuld::test]
fn create_treats_a_dangling_symlink_as_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("nowhere");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut created = CreatedDirs::new();

    created.create(&link, None).unwrap();

    assert!(!target.exists());
    assert!(created.0.is_empty());
}

#[skuld::test]
fn create_accepts_a_regular_file_as_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();

    CreatedDirs::new().create(&file, None).unwrap();

    assert_eq!(fs::read(&file).unwrap(), b"x");
}

#[skuld::test]
fn create_under_a_regular_file_fails_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();
    let mut created = CreatedDirs::new();

    created
        .create(&file.join("a"), None)
        .expect_err("a file has no children");

    assert!(created.0.is_empty());
}

/// The account a daemon runs as owns the directories an earlier install made for it and can swap
/// one for a symlink into a root-owned directory; a directory owned by one uid is never followed
/// into one owned by another, so nothing is created there.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_refuses_to_follow_an_account_owned_symlink_into_a_root_owned_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let account = Some(21_000);
    let owned = tmp.path().join("owned");
    let target = tmp.path().join("target");
    fs::create_dir(&owned).unwrap();
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::chown(&owned, account, None).unwrap();
    std::os::unix::fs::symlink("../target", owned.join("link")).unwrap();
    std::os::unix::fs::lchown(owned.join("link"), account, None).unwrap();
    let mut created = CreatedDirs::new();

    let err = created
        .create(&owned.join("link/new"), None)
        .expect_err("the link leads from the account's directory into root's");

    assert!(err.to_string().contains("owned"), "{err}");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    assert!(created.0.is_empty());
}

/// The same shape owned by root is a legitimate ancestor (`/var/run` is one): it is followed.
#[skuld::test]
fn create_follows_a_symlinked_ancestor_the_caller_owns() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink("real", tmp.path().join("link")).unwrap();
    let mut created = CreatedDirs::new();

    created.create(&tmp.path().join("link/a/b"), None).unwrap();

    assert!(real.join("a/b").is_dir());
    assert_eq!(created.0.len(), 2);
}
