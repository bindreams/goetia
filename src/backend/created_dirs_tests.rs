use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use nix::unistd::{Gid, Uid};

use super::*;
use crate::backend::test_elevation::{ELEVATED, elevated};

/// Creates `root/a/b/c`, all of it recorded.
fn create_chain(root: &Path) -> CreatedDirs {
    let mut created = CreatedDirs::default();
    created.create(&root.join("a/b/c"), None).unwrap();
    created
}

/// The groups this process is in.
fn groups() -> Vec<libc::gid_t> {
    // SAFETY: a zero-length query takes no buffer; the second call's buffer holds `n` gids.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    assert!(n >= 0, "getgroups: {}", io::Error::last_os_error());
    let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
    let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    assert!(n >= 0, "getgroups: {}", io::Error::last_os_error());
    groups.truncate(n as usize);
    groups
}

/// Skuld precondition of the tests that hand a directory to another group: root may use any, anyone
/// else needs a group besides the primary one.
fn second_group() -> std::result::Result<(), String> {
    // SAFETY: no arguments, no failure.
    let primary = unsafe { libc::getegid() };
    if Uid::effective().is_root() || groups().iter().any(|group| *group != primary) {
        return Ok(());
    }
    Err("hands a directory to another group; run as root, or as a user in a group besides its primary one".to_string())
}

/// An owner for this process's uid and a group other than the temp directory's, so a `fchown` is
/// visible in the result: one of the process's groups, or, as root, any other.
fn other_owner(tmp: &Path) -> Owner {
    let inherited = fs::metadata(tmp).unwrap().gid();
    let gid = groups().into_iter().find(|g| *g != inherited).unwrap_or(inherited + 1);
    Owner::new(Uid::current(), Some(Gid::from_raw(gid))).unwrap()
}

/// What the error says went wrong, from its source.
fn reason(err: &Error) -> String {
    std::error::Error::source(err)
        .expect("an OS error has a source")
        .to_string()
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
    let mut created = CreatedDirs::default();
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
    let mut created = CreatedDirs::default();
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

    created.settle(|| Standing::Yes);
    drop(created);

    assert!(tmp.path().join("a/b/c").is_dir());
}

#[skuld::test]
fn settle_removes_the_directories_when_the_definition_does_not_stand() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = create_chain(tmp.path());

    created.settle(|| Standing::No);
    drop(created);

    assert!(!tmp.path().join("a").exists());
}

/// A read that did not complete says nothing about the definition; the directories stay.
#[skuld::test]
fn settle_keeps_the_directories_when_the_read_did_not_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = create_chain(tmp.path());

    created.settle(|| Standing::Unknown("could not read unit: permission denied".to_string()));
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

#[skuld::test(requires = [second_group])]
fn create_leaves_an_existing_directory_exactly_as_found() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let dir = tmp.path().join("existing");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(&dir);

    let mut created = CreatedDirs::default();
    created.create(&dir, Some(owner)).unwrap();
    created.hand_over().unwrap();

    assert_eq!(mode_and_gid(&dir), before);
    assert_eq!(before.0, 0o700);
}

/// The mode and owner land on the inode that was created: read back through the guard's own
/// descriptor, which is the inode the path names.
#[skuld::test(requires = [second_group])]
fn create_hands_every_created_directory_to_the_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let mut created = CreatedDirs::default();

    created.create(&tmp.path().join("a/b"), Some(owner)).unwrap();
    created.hand_over().unwrap();

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
    let mut created = CreatedDirs::default();

    created.create(&tmp.path().join("a"), None).unwrap();

    let parent = fs::metadata(tmp.path()).unwrap();
    let made = fs::metadata(tmp.path().join("a")).unwrap();
    assert_eq!((made.uid(), made.gid()), (parent.uid(), parent.gid()));
}

/// The account a daemon runs as owns the parent it was handed and can plant a symlink at the name
/// about to be created. `mkdirat` fails on it with `EEXIST`, so the symlink's target is never
/// chmodded or chowned.
#[skuld::test(requires = [second_group])]
fn create_leaves_a_symlink_planted_at_the_last_name() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let other = tempfile::tempdir().unwrap();
    fs::set_permissions(other.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(other.path());
    let dir = tmp.path().join("a");
    let found = walk(&dir, &[]).unwrap();
    std::os::unix::fs::symlink(other.path(), &dir).unwrap();
    let mut created = CreatedDirs::default();

    created.create_in(found, Some(owner)).unwrap();
    created.hand_over().unwrap();

    assert_eq!(mode_and_gid(other.path()), before);
    assert!(created.0.is_empty());
}

/// The same on a name above the last: the symlink is not descended into.
#[skuld::test]
fn create_does_not_descend_into_a_symlink_planted_above_the_last_name() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let found = walk(&tmp.path().join("a/b"), &[]).unwrap();
    std::os::unix::fs::symlink(other.path(), tmp.path().join("a")).unwrap();
    let mut created = CreatedDirs::default();

    created
        .create_in(found, None)
        .expect_err("a symlink is not a directory");

    assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
}

/// A component another install creates after the walk is not ours to chmod or chown; those below
/// it still are.
#[skuld::test(requires = [second_group])]
fn create_leaves_a_component_that_appeared_after_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let raced = tmp.path().join("a");
    let leaf = raced.join("b");
    let found = walk(&leaf, &[]).unwrap();
    let missing: Vec<_> = found.missing.iter().map(|(path, _)| path.as_path()).collect();
    assert_eq!(missing, [raced.as_path(), leaf.as_path()]);
    fs::create_dir(&raced).unwrap();
    fs::set_permissions(&raced, fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(&raced);

    let mut created = CreatedDirs::default();

    created.create_in(found, Some(owner)).unwrap();
    created.hand_over().unwrap();

    assert_eq!(mode_and_gid(&raced), before);
    assert_eq!(mode_and_gid(&leaf), (0o755, owner.gid.unwrap().as_raw()));
}

/// The last component must be a directory, or a symlink that resolves to one.
#[skuld::test]
fn create_accepts_a_symlink_to_a_directory_as_existing() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("real")).unwrap();
    std::os::unix::fs::symlink("real", tmp.path().join("link")).unwrap();
    let mut created = CreatedDirs::default();

    created.create(&tmp.path().join("link"), None).unwrap();

    assert!(created.0.is_empty());
}

#[skuld::test]
fn create_refuses_a_dangling_symlink_as_the_last_component() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("nowhere");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    CreatedDirs::default()
        .create(&link, None)
        .expect_err("a link to nothing");

    assert!(!target.exists());
}

#[skuld::test]
fn create_refuses_a_regular_file_as_the_last_component() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();

    let err = CreatedDirs::default()
        .create(&file, None)
        .expect_err("a file is not a directory");

    assert!(matches!(err, Error::NotADirectory { .. }), "{err:?}");
    assert_eq!(fs::read(&file).unwrap(), b"x");
}

#[skuld::test]
fn create_under_a_regular_file_fails_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();
    let mut created = CreatedDirs::default();

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
    let mut created = CreatedDirs::default();

    let err = created
        .create(&owned.join("link/new"), None)
        .expect_err("the link leads from the account's directory into root's");

    let reason = reason(&err);
    assert!(reason.contains("owned by uid 0"), "{reason}");
    assert!(reason.contains("comes from uid 21000"), "{reason}");
    assert!(reason.contains("carried from link"), "{reason}");
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
    let mut created = CreatedDirs::default();

    created.create(&tmp.path().join("link/a/b"), None).unwrap();

    assert!(real.join("a/b").is_dir());
    assert_eq!(created.0.len(), 2);
}

/// A directory owned by one account that holds one owned by another is a step between owners, and
/// is refused whatever the entries are: systemd's rule applies to every step.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_refuses_a_plain_directory_owned_by_another_account() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("a");
    let inner = outer.join("b");
    fs::create_dir_all(&inner).unwrap();
    std::os::unix::fs::chown(&outer, Some(21_000), None).unwrap();
    std::os::unix::fs::chown(&inner, Some(21_001), None).unwrap();
    let mut created = CreatedDirs::default();

    let err = created
        .create(&inner.join("c"), None)
        .expect_err("uid 21000 to uid 21001");

    let reason = reason(&err);
    assert!(reason.contains("owned by uid 21001"), "{reason}");
    assert!(!reason.contains("carried"), "{reason}");
    assert!(err.to_string().contains(inner.to_str().unwrap()), "{err}");
    assert!(!inner.join("c").exists());
}

/// The last component is judged too, though it exists and nothing is made: the account's
/// directory holding an entry that is root's is not something to vouch for.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_refuses_an_existing_last_component_owned_by_another_account() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("a");
    let last = outer.join("b");
    fs::create_dir_all(&last).unwrap();
    std::os::unix::fs::chown(&outer, Some(21_000), None).unwrap();

    let err = CreatedDirs::default()
        .create(&last, None)
        .expect_err("uid 21000 to root");

    assert!(reason(&err).contains("owned by uid 0"), "{err}");
}

/// A link the account owns that points at an absolute path is refused outright: that path starts at
/// root's `/`.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_refuses_an_absolute_symlink_the_account_owns() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::os::unix::fs::lchown(&link, Some(21_000), None).unwrap();

    let err = CreatedDirs::default()
        .create(&link.join("new"), None)
        .expect_err("an absolute link of the account's");

    let reason = reason(&err);
    assert!(
        reason.contains("owned by uid 0") && reason.contains("comes from uid 21000"),
        "{reason}"
    );
    assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
}

/// A root-owned absolute link is followed, and what it leads to is judged from root, whoever owns
/// the directories below.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_follows_an_absolute_symlink_root_owns() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::chown(&real, Some(21_000), None).unwrap();
    std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
    let mut created = CreatedDirs::default();

    created.create(&tmp.path().join("link/a"), None).unwrap();

    assert!(real.join("a").is_dir());
}

/// A cycle of links ends in `ELOOP` at the bound, one link past the longest chain that resolves.
#[skuld::test]
fn create_follows_a_chain_of_links_up_to_the_bound_and_no_further() {
    let tmp = tempfile::tempdir().unwrap();
    // Canonical, so the walk spends no link on the way to the chain: on macOS `/var` is one.
    let base = fs::canonicalize(tmp.path()).unwrap();
    fs::create_dir(base.join("end")).unwrap();
    // link{n} -> link{n+1}, the last -> end.
    let chain = |len: usize| {
        for n in 1..=len {
            let target = if n == len {
                "end".to_string()
            } else {
                format!("link{}", n + 1)
            };
            let link = base.join(format!("link{n}"));
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(target, link).unwrap();
        }
    };

    chain(SYMLINK_LIMIT);
    CreatedDirs::default().create(&base.join("link1/ok"), None).unwrap();

    chain(SYMLINK_LIMIT + 1);
    let err = CreatedDirs::default()
        .create(&base.join("link1/no"), None)
        .expect_err("one link too many");
    let source = std::error::Error::source(&err).unwrap();
    let errno = source.downcast_ref::<io::Error>().and_then(io::Error::raw_os_error);
    assert_eq!(errno, Some(libc::ELOOP), "{err}");
}

#[skuld::test]
fn walk_takes_dotdot_at_the_root_to_the_root() {
    let found = walk(Path::new("/.."), &[]).unwrap();

    assert!(found.missing.is_empty());
}

/// Nothing is made through a `..` below a component that does not exist: its meaning is not known.
#[skuld::test]
fn create_refuses_dotdot_below_a_missing_component() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();

    created
        .create(&tmp.path().join("missing/../x"), None)
        .expect_err("`..` below a missing component");

    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// Nothing is made through a link that leads nowhere: that would make its target.
#[skuld::test]
fn create_refuses_to_make_anything_below_a_dangling_link() {
    let tmp = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("nowhere", tmp.path().join("link")).unwrap();

    CreatedDirs::default()
        .create(&tmp.path().join("link/x"), None)
        .expect_err("a dangling link");

    assert!(!tmp.path().join("nowhere").exists());
}

#[skuld::test]
fn create_refuses_a_relative_path() {
    CreatedDirs::default()
        .create(Path::new("goetia-relative"), None)
        .expect_err("a relative path");

    assert!(!Path::new("goetia-relative").exists());
}

/// `(uid_t)-1` and `(gid_t)-1` mean "unchanged" to `fchown`: no account has either.
#[skuld::test]
fn an_owner_refuses_the_ids_that_mean_unchanged() {
    assert!(Owner::new(Uid::from_raw(u32::MAX), None).is_err());
    assert!(Owner::new(Uid::from_raw(0), Some(Gid::from_raw(u32::MAX))).is_err());
    assert!(Owner::new(Uid::from_raw(0), None).is_ok());
}

/// Mode and owner are given at the end, so that nothing is ever made in a directory the account
/// owns: until `hand_over` a created directory is private and the creator's.
#[skuld::test]
fn create_makes_directories_private_and_the_creators_until_they_are_handed_over() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();

    created.create(&tmp.path().join("a/b"), None).unwrap();

    for dir in ["a", "a/b"] {
        let meta = fs::metadata(tmp.path().join(dir)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700, "{dir}");
        assert_eq!(meta.uid(), Uid::effective().as_raw(), "{dir}");
    }
}

/// `hand_over` makes `0755` itself, as `mkdirat`'s `0700` makes the mode its own work: deleting
/// the `fchmod` leaves `0700`, whatever the umask.
#[skuld::test]
fn hand_over_without_an_owner_makes_the_directories_0755() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a/b"), None).unwrap();

    created.hand_over().unwrap();

    for dir in ["a", "a/b"] {
        let mode = fs::metadata(tmp.path().join(dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{dir}");
    }
}

/// A step after a successful `mkdirat` fails (the second name is too long): what was made stays
/// recorded, so the drop removes it.
#[skuld::test]
fn a_failure_after_a_made_directory_leaves_it_to_the_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();

    created
        .create(&tmp.path().join("a").join("x".repeat(300)), None)
        .expect_err("a name too long for the file system");

    assert_eq!(created.0.len(), 1);
    assert!(tmp.path().join("a").is_dir(), "`a` existed before the rollback");
    drop(created);
    assert!(!tmp.path().join("a").exists());
}

/// The last component is made by another install after the walk, as something that is not a
/// directory: it is judged like an existing one.
#[skuld::test]
fn create_refuses_a_last_component_another_install_made_as_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a");
    let found = walk(&dir, &[]).unwrap();
    fs::write(&dir, b"x").unwrap();

    let err = CreatedDirs::default().create_in(found, None).expect_err("a file");

    assert!(matches!(err, Error::NotADirectory { .. }), "{err:?}");
}

/// What the account owns is not entered while root makes directories: every directory is made, then
/// all are handed over, so a later `create` of the same guard walks root-owned ones.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn hand_over_gives_every_directory_of_every_create_to_the_owner_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = Some(Owner::new(Uid::from_raw(21_000), None).unwrap());
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a/b"), owner).unwrap();
    created.create(&tmp.path().join("a/b/c"), owner).unwrap();
    for dir in ["a", "a/b", "a/b/c"] {
        assert_eq!(fs::metadata(tmp.path().join(dir)).unwrap().uid(), 0, "{dir}");
    }

    created.hand_over().unwrap();

    for dir in ["a", "a/b", "a/b/c"] {
        assert_eq!(fs::metadata(tmp.path().join(dir)).unwrap().uid(), 21_000, "{dir}");
    }
}

/// A directory the guard made under another account's directory is not a step between owners to
/// the guard's own later `create`: it is its own.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_walks_through_a_directory_it_made_under_another_accounts() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("x");
    fs::create_dir(&outer).unwrap();
    std::os::unix::fs::chown(&outer, Some(21_000), None).unwrap();
    let mut created = CreatedDirs::default();
    created.create(&outer.join("a"), None).unwrap();

    created.create(&outer.join("a/work"), None).unwrap();

    assert!(outer.join("a/work").is_dir());
}

/// A component another install made after the walk is judged as the walk judges one: a directory of
/// a third uid under the account's is a step between owners.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_refuses_a_component_another_install_made_for_another_account() {
    let tmp = tempfile::tempdir().unwrap();
    let holder = tmp.path().join("p");
    fs::create_dir(&holder).unwrap();
    std::os::unix::fs::chown(&holder, Some(21_000), None).unwrap();
    let found = walk(&holder.join("q/r"), &[]).unwrap();
    fs::create_dir(holder.join("q")).unwrap();
    std::os::unix::fs::chown(holder.join("q"), Some(21_001), None).unwrap();
    let mut created = CreatedDirs::default();

    let err = created.create_in(found, None).expect_err("uid 21000 to uid 21001");

    assert!(reason(&err).contains("owned by uid 21001"), "{err}");
    assert!(!holder.join("q/r").exists());
}

/// A directory the drop removes was taken back from the account for it; one it leaves is as the
/// account had it. Here `a/b` is removed, and `a`, which has content, stays with the owner and mode
/// the account gave it, though it was taken back to remove `b` from it.
#[skuld::test(requires = [second_group])]
fn drop_leaves_a_directory_it_does_not_remove_as_the_account_had_it() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a/b"), Some(owner)).unwrap();
    created.hand_over().unwrap();
    fs::write(tmp.path().join("a/keep"), b"x").unwrap();
    fs::set_permissions(tmp.path().join("a"), fs::Permissions::from_mode(0o777)).unwrap();

    drop(created);

    assert_eq!(
        mode_and_gid(&tmp.path().join("a")),
        (0o777, owner.gid.unwrap().as_raw())
    );
    assert!(!tmp.path().join("a/b").exists());
}

/// A child that stays keeps its parent as it was, and a child that is not the directory created
/// stays untouched: here `a/b` was swapped, so it and `a` are left as the account had them.
#[skuld::test(requires = [second_group])]
fn drop_leaves_a_swapped_directory_and_its_parent_as_the_account_had_them() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a/b"), Some(owner)).unwrap();
    created.hand_over().unwrap();
    fs::rename(tmp.path().join("a/b"), tmp.path().join("a/moved")).unwrap();
    fs::create_dir(tmp.path().join("a/b")).unwrap();
    let gid = owner.gid.unwrap().as_raw();
    let before = (
        mode_and_gid(&tmp.path().join("a")),
        mode_and_gid(&tmp.path().join("a/b")),
    );
    assert_eq!(before.0.1, gid);

    drop(created);

    let after = (
        mode_and_gid(&tmp.path().join("a")),
        mode_and_gid(&tmp.path().join("a/b")),
    );
    assert_eq!(after, before);
    assert!(tmp.path().join("a/moved").is_dir());
}

/// What `reclaim` takes it back to is the creator's and `0755`, and `restore` gives back exactly
/// what it was.
#[skuld::test(requires = [second_group])]
fn reclaim_takes_a_directory_back_and_restore_gives_it_as_it_was() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a"), Some(owner)).unwrap();
    created.hand_over().unwrap();
    let dir = tmp.path().join("a");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let handed = mode_and_gid(&dir);
    assert_eq!(handed.1, owner.gid.unwrap().as_raw());

    let prior = created.reclaim(0).unwrap();
    assert_eq!(mode_and_gid(&dir), (0o755, created.0[0].gid));

    created.restore(0, &prior).unwrap();
    assert_eq!(mode_and_gid(&dir), handed);
}

/// A directory kept stays handed over whatever its install did before: `create` made it private and
/// the creator's, and a failure before `hand_over` must not leave it so, as a rerun would find it
/// and take it for existing.
#[skuld::test(requires = [second_group])]
fn settle_hands_over_a_kept_directory_that_never_was() {
    for standing in [Standing::Yes, Standing::Unknown("unreadable".to_string())] {
        let tmp = tempfile::tempdir().unwrap();
        let owner = other_owner(tmp.path());
        let mut created = CreatedDirs::default();
        created.create(&tmp.path().join("a/b"), Some(owner)).unwrap();

        created.settle(|| standing);
        drop(created);

        for dir in ["a", "a/b"] {
            assert_eq!(
                mode_and_gid(&tmp.path().join(dir)),
                (0o755, owner.gid.unwrap().as_raw()),
                "{dir}"
            );
        }
    }
}

/// With no account to hand to, kept means `0755`.
#[skuld::test]
fn settle_makes_a_kept_directory_0755_when_there_is_no_account() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a"), None).unwrap();

    created.settle(|| Standing::Yes);
    drop(created);

    assert_eq!(mode_and_gid(&tmp.path().join("a")).0, 0o755);
}

/// A panic leaves the definition's standing unknown, as an unreadable one does: kept, and so handed
/// over.
#[skuld::test(requires = [second_group])]
fn drop_hands_the_directories_over_while_a_panic_unwinds() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = other_owner(tmp.path());
    let root = tmp.path().to_path_buf();

    let unwound = std::panic::catch_unwind(move || {
        let mut created = CreatedDirs::default();
        created.create(&root.join("a/b"), Some(owner)).unwrap();
        panic!("a step before `hand_over`");
    });

    assert!(unwound.is_err());
    for dir in ["a", "a/b"] {
        assert_eq!(
            mode_and_gid(&tmp.path().join(dir)),
            (0o755, owner.gid.unwrap().as_raw()),
            "{dir}"
        );
    }
}

/// `CAP_CHOWN` dropped from this thread until the guard goes, so that a `fchown` to a group the
/// process is not in fails, as it does for anyone unprivileged, even when the suite runs as root.
#[cfg(target_os = "linux")]
struct NoChown {
    header: CapHeader,
    saved: [CapData; 2],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[cfg(target_os = "linux")]
impl NoChown {
    const VERSION_3: u32 = 0x2008_0522;
    const CAP_CHOWN: u32 = 1;

    fn new() -> Self {
        let header = CapHeader {
            version: Self::VERSION_3,
            pid: 0,
        };
        let mut saved = [CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        // SAFETY: `capget` fills the two `CapData` of version 3 and reads the header; pid 0 is this thread.
        let got = unsafe { libc::syscall(libc::SYS_capget, &header, saved.as_mut_ptr()) };
        assert_eq!(got, 0, "capget: {}", io::Error::last_os_error());
        let mut lowered = saved;
        lowered[0].effective &= !Self::CAP_CHOWN;
        // SAFETY: `capset` reads the header and the two `CapData` of version 3.
        let set = unsafe { libc::syscall(libc::SYS_capset, &header, lowered.as_ptr()) };
        assert_eq!(set, 0, "capset: {}", io::Error::last_os_error());
        Self { header, saved }
    }
}

#[cfg(target_os = "linux")]
impl Drop for NoChown {
    fn drop(&mut self) {
        // SAFETY: as in `new`.
        let set = unsafe { libc::syscall(libc::SYS_capset, &self.header, self.saved.as_ptr()) };
        assert_eq!(set, 0, "capset: {}", io::Error::last_os_error());
    }
}

/// A directory is handed over only once `fchown` has succeeded: here it fails (a group this process
/// is not in), and the directory is opened but not handed.
#[cfg(target_os = "linux")]
#[skuld::test]
fn a_failed_chown_does_not_read_as_handed_over() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: no arguments, no failure.
    let egid = unsafe { libc::getegid() };
    let foreign = (1..).find(|gid| *gid != egid && !groups().contains(gid)).unwrap();
    let owner = Owner::new(Uid::current(), Some(Gid::from_raw(foreign))).unwrap();
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a"), Some(owner)).unwrap();
    assert_eq!(created.0[0].state, State::Private);
    let _no_chown = NoChown::new();

    created
        .hand_over()
        .expect_err("this process may not give a directory to that group");

    assert_eq!(created.0[0].state, State::Opened);
}

fn parent_stat(uid: libc::uid_t, mode: libc::mode_t) -> FileStat {
    // SAFETY: `stat` is plain integers; all zero is a valid value.
    let mut stat: FileStat = unsafe { std::mem::zeroed() };
    stat.st_uid = uid;
    stat.st_mode = libc::S_IFDIR | mode;
    stat
}

/// A swap between `mkdirat` and the open needs someone besides root who can write the parent. Where
/// nobody can, the entry's owner says nothing: a file system maps it (NFS with `root_squash` gives
/// 65534, a vfat mount gives its `uid=`).
#[skuld::test]
fn a_made_directory_in_a_parent_only_root_can_write_is_accepted_whoever_owns_it() {
    for mode in [0o755, 0o700, 0o1777, 0o3777] {
        assert!(made_here(&parent_stat(0, mode), 65_534, 0), "{mode:o}");
    }
}

#[skuld::test]
fn a_made_directory_in_a_parent_another_can_write_must_be_the_creators() {
    // Owned by an account; group-writable; world-writable without the sticky bit.
    for (owner, mode) in [(1000, 0o755), (0, 0o775), (0, 0o757), (0, 0o777), (1000, 0o1777)] {
        let parent = parent_stat(owner, mode);
        assert!(!made_here(&parent, 65_534, 0), "{owner} {mode:o}");
        assert!(made_here(&parent, 0, 0), "{owner} {mode:o}");
    }
}

#[skuld::test]
fn settle_does_not_ask_when_nothing_was_made() {
    CreatedDirs::default().settle(|| panic!("nothing was made, so there is nothing to ask about"));
}
