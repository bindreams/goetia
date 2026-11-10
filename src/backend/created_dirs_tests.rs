use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use nix::unistd::{Gid, Uid};

use super::*;
use crate::backend::test_elevation::{ELEVATED, elevated};

/// Creates `root/a/b/c`, all of it recorded.
fn create_chain(root: &Path) -> CreatedDirs {
    let mut created = CreatedDirs::default();
    created.create(&root.join("a/b/c")).unwrap();
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
    created.create(&new).unwrap();

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
    created.create(&dir).unwrap();
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
    created.create(&dir).unwrap();
    created.hand_over(Some(owner)).unwrap();

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

    created.create(&tmp.path().join("a/b")).unwrap();
    created.hand_over(Some(owner)).unwrap();

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

    created.create(&tmp.path().join("a")).unwrap();

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

    created.create_in(found).unwrap();
    created.hand_over(Some(owner)).unwrap();

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

    created.create_in(found).expect_err("a symlink is not a directory");

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

    created.create_in(found).unwrap();
    created.hand_over(Some(owner)).unwrap();

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

    created.create(&tmp.path().join("link")).unwrap();

    assert!(created.0.is_empty());
}

#[skuld::test]
fn create_refuses_a_dangling_symlink_as_the_last_component() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("nowhere");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    CreatedDirs::default().create(&link).expect_err("a link to nothing");

    assert!(!target.exists());
}

#[skuld::test]
fn create_refuses_a_regular_file_as_the_last_component() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();

    let err = CreatedDirs::default()
        .create(&file)
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

    created.create(&file.join("a")).expect_err("a file has no children");

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
        .create(&owned.join("link/new"))
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

    created.create(&tmp.path().join("link/a/b")).unwrap();

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

    let err = created.create(&inner.join("c")).expect_err("uid 21000 to uid 21001");

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

    let err = CreatedDirs::default().create(&last).expect_err("uid 21000 to root");

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
        .create(&link.join("new"))
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

    created.create(&tmp.path().join("link/a")).unwrap();

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
    CreatedDirs::default().create(&base.join("link1/ok")).unwrap();

    chain(SYMLINK_LIMIT + 1);
    let err = CreatedDirs::default()
        .create(&base.join("link1/no"))
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
        .create(&tmp.path().join("missing/../x"))
        .expect_err("`..` below a missing component");

    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// Nothing is made through a link that leads nowhere: that would make its target.
#[skuld::test]
fn create_refuses_to_make_anything_below_a_dangling_link() {
    let tmp = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("nowhere", tmp.path().join("link")).unwrap();

    CreatedDirs::default()
        .create(&tmp.path().join("link/x"))
        .expect_err("a dangling link");

    assert!(!tmp.path().join("nowhere").exists());
}

#[skuld::test]
fn create_refuses_a_relative_path() {
    CreatedDirs::default()
        .create(Path::new("goetia-relative"))
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

    created.create(&tmp.path().join("a/b")).unwrap();

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
    created.create(&tmp.path().join("a/b")).unwrap();

    created.hand_over(None).unwrap();

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
        .create(&tmp.path().join("a").join("x".repeat(300)))
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

    let err = CreatedDirs::default().create_in(found).expect_err("a file");

    assert!(matches!(err, Error::NotADirectory { .. }), "{err:?}");
}

/// What the account owns is not entered while root makes directories: every directory is made, then
/// all are handed over, so a later `create` of the same guard walks root-owned ones.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn hand_over_gives_every_directory_of_every_create_to_the_owner_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a/b")).unwrap();
    created.create(&tmp.path().join("a/b/c")).unwrap();
    for dir in ["a", "a/b", "a/b/c"] {
        assert_eq!(fs::metadata(tmp.path().join(dir)).unwrap().uid(), 0, "{dir}");
    }

    created
        .hand_over(Some(Owner::new(Uid::from_raw(21_000), None).unwrap()))
        .unwrap();

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
    created.create(&outer.join("a")).unwrap();

    created.create(&outer.join("a/work")).unwrap();

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

    let err = created.create_in(found).expect_err("uid 21000 to uid 21001");

    assert!(reason(&err).contains("owned by uid 21001"), "{err}");
    assert!(!holder.join("q/r").exists());
}

/// A drop takes a directory back from the account before it removes anything: here `a` stays (it
/// has content), and is root's and `0755` again, whatever the account did to it.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn drop_takes_a_handed_over_directory_back() {
    let tmp = tempfile::tempdir().unwrap();
    let mut created = CreatedDirs::default();
    created.create(&tmp.path().join("a/b")).unwrap();
    created
        .hand_over(Some(
            Owner::new(Uid::from_raw(21_000), Some(Gid::from_raw(21_000))).unwrap(),
        ))
        .unwrap();
    fs::write(tmp.path().join("a/keep"), b"x").unwrap();
    fs::set_permissions(tmp.path().join("a"), fs::Permissions::from_mode(0o777)).unwrap();

    drop(created);

    let meta = fs::metadata(tmp.path().join("a")).unwrap();
    assert_eq!((meta.uid(), meta.permissions().mode() & 0o777), (0, 0o755));
    assert!(!tmp.path().join("a/b").exists());
}

#[skuld::test]
fn settle_does_not_ask_when_nothing_was_made() {
    CreatedDirs::default().settle(|| panic!("nothing was made, so there is nothing to ask about"));
}
