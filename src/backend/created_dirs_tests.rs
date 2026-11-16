use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

use nix::unistd::{Gid, Uid, chown};

use super::*;
use crate::error::Error;
use crate::testing::elevation::{ELEVATED, require_elevated};

/// A uid no account has, to own what a test makes foreign.
pub(super) const STRANGER: u32 = 4242;

/// These tests make root-owned directories, which is what the code under test accepts, so they run
/// as root: CI's elevated test step does.
pub(super) fn as_root() {
    require_elevated();
    assert!(Uid::effective().is_root(), "an elevated process is root on Unix");
}

/// A directory under `/tmp`, which is root's on every host (a macOS temp directory is the
/// logged-in user's), holding `system`, the one root of the tests, and `outside`, which is not.
pub(crate) struct Layout {
    tmp: tempfile::TempDir,
}

impl Layout {
    pub(crate) fn new() -> Self {
        as_root();
        let tmp = tempfile::Builder::new()
            .prefix("goetia-dirs-")
            .tempdir_in("/tmp")
            .expect("make a directory under /tmp");
        // A macOS `/tmp` is a link: the walk resolves it, and the tests name the resolved path.
        let tmp_path = tmp.path().canonicalize().expect("resolve the temp directory");
        fs::create_dir(tmp_path.join("system")).unwrap();
        fs::create_dir(tmp_path.join("outside")).unwrap();
        Self { tmp }
    }

    pub(super) fn base(&self) -> PathBuf {
        self.tmp.path().canonicalize().unwrap()
    }

    pub(crate) fn system(&self) -> PathBuf {
        self.base().join("system")
    }

    pub(super) fn outside(&self) -> PathBuf {
        self.base().join("outside")
    }

    /// A guard that treats `system` as its only root.
    fn dirs(&self) -> CreatedDirs {
        CreatedDirs::under(vec![self.system()])
    }
}

pub(super) fn give_to_a_stranger(path: &Path) {
    chown(path, Some(Uid::from_raw(STRANGER)), Some(Gid::from_raw(STRANGER))).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn owner_of(path: &Path) -> (u32, u32) {
    let meta = fs::metadata(path).unwrap();
    (meta.uid(), meta.gid())
}

/// The text of a refusal, which the tests read for what it names.
fn refusal(err: &Error) -> String {
    match err {
        Error::DirectoryRefused { why, .. } => why.clone(),
        other => panic!("expected a refused directory, got {other:?}"),
    }
}

pub(super) fn stranger_owner() -> Owner {
    Owner::new(Uid::from_raw(STRANGER), Some(Gid::from_raw(STRANGER))).unwrap()
}

// Creating ------------------------------------------------------------------------------------------------------------

#[skuld::test(labels = [ELEVATED])]
fn create_makes_every_missing_component_under_a_root_as_roots_0755() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b/c"), None).unwrap();

    for dir in ["a", "a/b", "a/b/c"] {
        let path = layout.system().join(dir);
        assert!(path.is_dir(), "{}", path.display());
        assert_eq!(mode(&path), 0o755, "{}", path.display());
        assert_eq!(owner_of(&path).0, 0, "{}", path.display());
    }
}

#[skuld::test(labels = [ELEVATED])]
fn create_leaves_a_directory_that_exists_as_found_wherever_it_is_and_whoever_owns_it() {
    let layout = Layout::new();
    let dir = layout.outside().join("existing");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    give_to_a_stranger(&dir);
    let before = (mode(&dir), owner_of(&dir));
    let mut dirs = layout.dirs();

    dirs.create(&dir, Some(stranger_owner())).unwrap();
    dirs.hand_over().unwrap();

    assert_eq!((mode(&dir), owner_of(&dir)), before);
}

#[skuld::test(labels = [ELEVATED])]
fn create_uses_a_path_that_exists_whole_through_directories_and_links_an_account_owns() {
    let layout = Layout::new();
    let owned = layout.outside().join("owned");
    fs::create_dir_all(owned.join("deep")).unwrap();
    give_to_a_stranger(&owned);
    let link = layout.outside().join("link");
    symlink(&owned, &link).unwrap();
    std::os::unix::fs::lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&link.join("deep"), None).unwrap();
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_missing_directory_outside_the_roots_naming_the_path_and_the_roots() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let wanted = layout.outside().join("a/b");

    let err = dirs.create(&wanted, None).expect_err("outside every root");

    let why = refusal(&err);
    assert!(why.contains("create it first"), "{why}");
    assert!(why.contains(layout.system().to_str().unwrap()), "{why}");
    assert!(
        matches!(&err, Error::DirectoryRefused { path, .. } if *path == wanted),
        "{err:?}"
    );
    assert!(
        !layout.outside().join("a").exists(),
        "nothing is created outside a root"
    );
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_missing_directory_when_no_root_exists() {
    let layout = Layout::new();
    let mut dirs = CreatedDirs::under(vec![layout.base().join("no-such-root")]);

    let err = dirs
        .create(&layout.base().join("no-such-root/a"), None)
        .expect_err("a root that is missing holds nothing");

    assert!(refusal(&err).contains("create it first"));
    assert!(!layout.base().join("no-such-root").exists());
}

/// "Under a root" is read on the path the walk resolved: a `..` that leaves the root is outside it
/// however the text starts, and one that stays inside is not.
#[skuld::test(labels = [ELEVATED])]
fn create_decides_under_a_root_on_the_resolved_path_not_the_text() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    let leaving = layout.system().join("../outside/new");
    let err = dirs.create(&leaving, None).expect_err("`..` leaves the root");
    assert!(refusal(&err).contains("create it first"));
    assert!(!layout.outside().join("new").exists());

    let staying = layout.outside().join("../system/new");
    dirs.create(&staying, None).expect("`..` arrives in the root");
    assert!(layout.system().join("new").is_dir());
}

#[skuld::test(labels = [ELEVATED])]
fn create_follows_a_root_owned_link_into_a_root_and_judges_the_target_not_the_name() {
    let layout = Layout::new();
    symlink(layout.system(), layout.outside().join("in")).unwrap();
    symlink(layout.outside(), layout.system().join("out")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.outside().join("in/made"), None)
        .expect("the link leads into the root");
    assert!(layout.system().join("made").is_dir());

    let err = dirs
        .create(&layout.system().join("out/not-made"), None)
        .expect_err("the link leads out of the root");
    assert!(refusal(&err).contains("create it first"));
    assert!(!layout.outside().join("not-made").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_missing_component_below_a_directory_an_account_owns() {
    let layout = Layout::new();
    let owned = layout.system().join("owned");
    fs::create_dir(&owned).unwrap();
    give_to_a_stranger(&owned);
    let mut dirs = layout.dirs();

    let err = dirs
        .create(&owned.join("new"), None)
        .expect_err("root does not create inside a directory an account owns");

    let why = refusal(&err);
    assert!(why.contains(&format!("uid {STRANGER}")), "{why}");
    assert!(why.contains(owned.to_str().unwrap()), "{why}");
    assert!(!owned.join("new").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_missing_component_below_a_symlink_an_account_owns() {
    let layout = Layout::new();
    let link = layout.system().join("link");
    symlink(layout.system().join("real"), &link).unwrap();
    fs::create_dir(layout.system().join("real")).unwrap();
    std::os::unix::fs::lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();
    let mut dirs = layout.dirs();

    let err = dirs
        .create(&link.join("new"), None)
        .expect_err("the account can retarget its link");

    assert!(refusal(&err).contains(&format!("uid {STRANGER}")));
    assert!(!layout.system().join("real/new").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_to_make_anything_below_a_dangling_link() {
    let layout = Layout::new();
    symlink(layout.system().join("nowhere"), layout.system().join("link")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("link/a"), None)
        .expect_err("that would make the link's target");

    assert!(!layout.system().join("nowhere").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_dangling_link_or_a_file_at_the_last_name() {
    let layout = Layout::new();
    symlink(layout.system().join("nowhere"), layout.system().join("link")).unwrap();
    fs::write(layout.system().join("file"), b"x").unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("link"), None)
        .expect_err("a link that leads nowhere is not a directory");
    let err = dirs
        .create(&layout.system().join("file"), None)
        .expect_err("a file is not a directory");

    assert!(matches!(err, Error::NotADirectory { .. }), "{err:?}");
    assert_eq!(fs::read(layout.system().join("file")).unwrap(), b"x");
}

#[skuld::test(labels = [ELEVATED])]
fn create_under_a_regular_file_fails_and_makes_nothing() {
    let layout = Layout::new();
    fs::write(layout.system().join("file"), b"x").unwrap();
    let mut dirs = layout.dirs();

    let err = dirs
        .create(&layout.system().join("file/a/b"), None)
        .expect_err("a file has no entries");

    assert!(
        std::error::Error::source(&err).is_some_and(|source| source.to_string().contains("ENOTDIR")),
        "{err:?}"
    );
    assert!(!layout.system().join("file/a").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_accepts_a_symlink_to_a_directory_as_existing() {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("real")).unwrap();
    symlink("real", layout.system().join("link")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("link"), None).unwrap();

    assert_eq!(fs::read_dir(layout.system().join("real")).unwrap().count(), 0);
}

#[skuld::test(labels = [ELEVATED])]
fn create_stops_at_a_symlink_loop() {
    let layout = Layout::new();
    symlink("b", layout.system().join("a")).unwrap();
    symlink("a", layout.system().join("b")).unwrap();
    let mut dirs = layout.dirs();

    let err = dirs
        .create(&layout.system().join("a/c"), None)
        .expect_err("a loop leads nowhere");

    assert!(
        std::error::Error::source(&err)
            .is_some_and(|source| source.to_string().contains("Too many levels of symbolic links")),
        "{err:?}"
    );
}

/// Only a root that is not there is not a root: one that cannot be opened for another reason says so.
#[skuld::test(labels = [ELEVATED])]
fn create_fails_when_a_root_cannot_be_opened_for_a_reason_other_than_not_being_there() {
    let layout = Layout::new();
    let file = layout.outside().join("file");
    fs::write(&file, b"x").unwrap();
    let mut dirs = CreatedDirs::under(vec![file.clone(), layout.system()]);

    let err = dirs
        .create(&layout.system().join("a"), None)
        .expect_err("a root that is a file cannot be searched");

    assert!(
        std::error::Error::source(&err).is_some_and(|source| source.to_string().contains("Not a directory"))
            && err.to_string().contains(file.to_str().unwrap()),
        "{err:?}"
    );
}

/// A path that exists whole is used as it is, wherever it is: the roots are opened only when a
/// component is missing and the walk asks whether it is under one.
#[skuld::test(labels = [ELEVATED])]
fn create_uses_a_path_that_exists_whatever_state_the_roots_are_in() {
    let layout = Layout::new();
    let file = layout.outside().join("file");
    fs::write(&file, b"x").unwrap();
    let looped = layout.outside().join("loop");
    symlink(&looped, &looped).unwrap();
    let mut dirs = CreatedDirs::under(vec![file, looped, layout.system()]);

    dirs.create(&layout.outside(), None)
        .expect("an existing directory outside every root");
    dirs.create(&layout.system(), None).expect("an existing root");

    assert!(dirs.made.is_empty());
}

#[skuld::test(labels = [ELEVATED])]
fn create_ignores_a_root_that_does_not_exist() {
    let layout = Layout::new();
    let mut dirs = CreatedDirs::under(vec![layout.base().join("nowhere"), layout.system()]);

    dirs.create(&layout.system().join("a"), None).unwrap();

    assert!(layout.system().join("a").is_dir());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_relative_path() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(Path::new("a/b"), None)
        .expect_err("a spec's directory is absolute");
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_dotdot_below_a_component_that_does_not_exist() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/../b"), None)
        .expect_err("`a` would have to exist");

    assert!(!layout.system().join("a").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn walking_dotdot_at_the_root_stays_at_the_root() {
    as_root();
    let mut dirs = CreatedDirs::under(vec![PathBuf::from("/")]);

    dirs.create(Path::new("/../.."), None).expect("the root is a directory");
}

/// A path with nothing left to walk is a directory that exists, wherever the roots are: `/` has no
/// last component, and a path that ends in `..` has none the walk takes.
#[skuld::test(labels = [ELEVATED])]
fn create_uses_the_root_directory_whatever_the_roots_are() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(Path::new("/"), None).expect("`/` exists");
    dirs.create(Path::new("/."), None).expect("`/.` is `/`");
}

#[skuld::test(labels = [ELEVATED])]
fn create_uses_a_path_that_ends_in_dotdot_as_the_directory_it_names() {
    let layout = Layout::new();
    fs::create_dir(layout.outside().join("deep")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.outside().join("deep/.."), None)
        .expect("it names the directory above `deep`, which exists");

    assert!(!layout.outside().join("deep/new").exists());
}

/// A component another install creates after the walk is not ours: it is used as it is, and what is
/// made below it is.
#[skuld::test(labels = [ELEVATED])]
fn create_uses_a_component_that_appeared_after_the_walk_and_makes_what_is_below_it() {
    let layout = Layout::new();
    let raced = layout.system().join("a");
    let leaf = raced.join("b");
    let found = walk(&leaf).unwrap();
    fs::create_dir(&raced).unwrap();
    fs::set_permissions(&raced, fs::Permissions::from_mode(0o700)).unwrap();
    let mut dirs = layout.dirs();

    dirs.make(found, || Ok(None)).unwrap();

    assert_eq!(mode(&raced), 0o700, "the other install's directory is untouched");
    assert_eq!(mode(&leaf), 0o755);
    drop(dirs);
    assert!(raced.is_dir(), "the undo does not remove what it did not make");
    assert!(!leaf.exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_component_that_appeared_after_the_walk_for_an_account() {
    let layout = Layout::new();
    let raced = layout.system().join("a");
    let found = walk(&raced.join("b")).unwrap();
    fs::create_dir(&raced).unwrap();
    give_to_a_stranger(&raced);
    let mut dirs = layout.dirs();

    let err = dirs.make(found, || Ok(None)).expect_err("an account owns it now");

    assert!(refusal(&err).contains(&format!("uid {STRANGER}")));
    assert!(!raced.join("b").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_does_not_descend_into_a_symlink_planted_after_the_walk() {
    let layout = Layout::new();
    let found = walk(&layout.system().join("a/b")).unwrap();
    symlink(layout.outside(), layout.system().join("a")).unwrap();
    let mut dirs = layout.dirs();

    dirs.make(found, || Ok(None)).expect_err("a symlink is not a directory");

    assert_eq!(fs::read_dir(layout.outside()).unwrap().count(), 0);
}

// What a made directory is checked for --------------------------------------------------------------------------------

fn stat_of(uid: u32, mode: u32, nlink: u64) -> FileStat {
    // SAFETY: `stat` is plain data, and all-zero is a valid value of it.
    let mut stat: FileStat = unsafe { std::mem::zeroed() };
    stat.st_uid = uid;
    stat.st_mode = libc::S_IFDIR | mode as libc::mode_t;
    stat.st_nlink = nlink as _;
    stat
}

#[skuld::test]
fn a_made_directory_is_roots_empty_and_a_directory() {
    assert!(judge(&stat_of(0, 0o700, 2), true).is_ok());
}

/// btrfs counts no `.` and `..`, a setgid parent hands its bit on, and the umask narrows a mode: none
/// of these makes a directory another's.
#[skuld::test]
fn a_made_directory_is_not_judged_by_link_count_or_mode() {
    assert!(judge(&stat_of(0, 0o700, 1), true).is_ok());
    assert!(judge(&stat_of(0, 0o2700, 2), true).is_ok());
    assert!(judge(&stat_of(0, 0o500, 2), true).is_ok());
}

#[skuld::test]
fn a_made_directory_is_refused_when_not_roots_or_not_empty_or_not_a_directory() {
    assert!(judge(&stat_of(1000, 0o700, 2), true).unwrap_err().contains("uid 1000"));
    assert!(judge(&stat_of(0, 0o700, 2), false).unwrap_err().contains("not empty"));
    let mut file = stat_of(0, 0o700, 1);
    file.st_mode = (libc::S_IFREG | 0o700) as _;
    assert!(judge(&file, true).unwrap_err().contains("not a directory"));
}

#[skuld::test(labels = [ELEVATED])]
fn a_made_directory_in_a_setgid_parent_is_accepted_and_given_0755() {
    let layout = Layout::new();
    let parent = layout.system().join("setgid");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o2755)).unwrap();
    chown(&parent, None, Some(Gid::from_raw(STRANGER))).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&parent.join("new"), None).unwrap();

    assert_eq!(mode(&parent.join("new")), 0o755);
}

/// A directory made as another uid, which is what a root-squashing file system hands back, is
/// refused; it was recorded when it was made, so it is removed with the rest.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_made_directory_that_is_not_roots_is_refused_and_removed_with_the_rest() {
    /// The file-system uid of this thread, put back on drop.
    struct Fsuid(libc::uid_t);
    impl Fsuid {
        fn set(uid: libc::uid_t) -> Self {
            // SAFETY: `setfsuid` takes and returns an id and touches only this thread's credentials.
            Self(unsafe { libc::setfsuid(uid) } as libc::uid_t)
        }
    }
    impl Drop for Fsuid {
        fn drop(&mut self) {
            // SAFETY: as in `set`.
            unsafe { libc::setfsuid(self.0) };
        }
    }

    let layout = Layout::new();
    let parent = layout.system().join("open");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    let mut dirs = layout.dirs();

    let result = {
        let _fs = Fsuid::set(STRANGER);
        dirs.create(&parent.join("new"), None)
    };

    let err = result.expect_err("the directory belongs to another uid");
    assert!(refusal(&err).contains(&format!("uid {STRANGER}")), "{err:?}");
    assert!(parent.join("new").is_dir(), "it was made");
    drop(dirs);
    assert!(!parent.join("new").exists(), "the undo removes a refused directory");
}

// Handing over --------------------------------------------------------------------------------------------------------

#[skuld::test(labels = [ELEVATED])]
fn hand_over_gives_the_deepest_directory_to_the_owner_and_no_other() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b"), Some(stranger_owner()))
        .unwrap();
    assert_eq!(
        owner_of(&layout.system().join("a/b")).0,
        0,
        "nothing is handed before hand_over"
    );
    dirs.hand_over().unwrap();

    assert_eq!(owner_of(&layout.system().join("a/b")), (STRANGER, STRANGER));
    assert_eq!(owner_of(&layout.system().join("a")), (0, owner_of(&layout.system()).1));
    assert_eq!(mode(&layout.system().join("a")), 0o755);
    assert_eq!(mode(&layout.system().join("a/b")), 0o755);
}

/// The owner is looked up when a directory is to be handed over, and not otherwise: never for a path
/// that exists whole, once for one made now or made earlier and marked now.
#[skuld::test(labels = [ELEVATED])]
fn create_for_asks_for_the_owner_only_when_a_directory_is_handed_over() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let asked = std::cell::Cell::new(0);
    let owner = || {
        asked.set(asked.get() + 1);
        Ok(Some(stranger_owner()))
    };
    fs::create_dir(layout.system().join("existing")).unwrap();

    dirs.create_for(&layout.system().join("existing"), owner).unwrap();
    assert_eq!(asked.get(), 0, "an existing path is handed nothing");

    dirs.create(&layout.system().join("a/b"), None).unwrap();
    dirs.create_for(&layout.system().join("a"), owner).unwrap();
    assert_eq!(asked.get(), 1, "a directory made earlier is marked now");
    dirs.create_for(&layout.system().join("c"), owner).unwrap();
    assert_eq!(asked.get(), 2);
    dirs.hand_over().unwrap();

    assert_eq!(owner_of(&layout.system().join("a")).0, STRANGER);
    assert_eq!(owner_of(&layout.system().join("c")).0, STRANGER);
    assert_eq!(owner_of(&layout.system().join("existing")).0, 0);
}

#[skuld::test(labels = [ELEVATED])]
fn hand_over_without_an_owner_leaves_everything_roots() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b"), None).unwrap();
    dirs.hand_over().unwrap();

    assert_eq!(owner_of(&layout.system().join("a/b")).0, 0);
}

/// `cwd` may be an ancestor of the log directory, or the other way about: a directory made as an
/// intermediate is the one handed over when a later path names it.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_made_as_an_intermediate_is_handed_over_when_a_later_path_names_it() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b"), None).unwrap();
    dirs.create(&layout.system().join("a"), Some(stranger_owner())).unwrap();
    dirs.hand_over().unwrap();

    assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
    assert_eq!(owner_of(&layout.system().join("a/b")).0, 0);
}

#[skuld::test(labels = [ELEVATED])]
fn an_owner_refuses_the_ids_that_mean_unchanged() {
    assert!(Owner::new(Uid::from_raw(u32::MAX), None).is_err());
    assert!(Owner::new(Uid::from_raw(1), Some(Gid::from_raw(u32::MAX))).is_err());
    assert!(Owner::new(Uid::from_raw(1), None).is_ok());
}

// Undoing -------------------------------------------------------------------------------------------------------------

fn make_chain(layout: &Layout) -> CreatedDirs {
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a/b/c"), None).unwrap();
    dirs
}

#[skuld::test(labels = [ELEVATED])]
fn drop_removes_every_directory_made_deepest_first() {
    let layout = Layout::new();

    drop(make_chain(&layout));

    assert!(!layout.system().join("a").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_that_existed_before() {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("before")).unwrap();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("before/new"), None).unwrap();

    drop(dirs);

    assert!(layout.system().join("before").is_dir());
    assert!(!layout.system().join("before/new").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_that_is_no_longer_empty_and_its_ancestors() {
    let layout = Layout::new();
    let dirs = make_chain(&layout);
    let file = layout.system().join("a/b/c/data");
    fs::write(&file, b"x").unwrap();

    drop(dirs);

    assert_eq!(fs::read(&file).unwrap(), b"x");
}

/// The account a daemon runs as owns what is handed to it and can swap a directory for a symlink
/// before the drop; the drop, which runs as root, must not follow it.
#[skuld::test(labels = [ELEVATED])]
fn drop_does_not_follow_a_symlink_swapped_in_for_a_made_directory() {
    let layout = Layout::new();
    fs::create_dir(layout.outside().join("c")).unwrap();
    let dirs = make_chain(&layout);
    fs::rename(layout.system().join("a/b"), layout.system().join("moved")).unwrap();
    symlink(layout.outside(), layout.system().join("a/b")).unwrap();

    drop(dirs);

    assert!(layout.outside().join("c").is_dir(), "the link's target was emptied");
}

#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_that_is_no_longer_the_one_made() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let dir = layout.system().join("a");
    dirs.create(&dir, None).unwrap();
    fs::rename(&dir, layout.system().join("moved")).unwrap();
    fs::create_dir(&dir).unwrap();

    drop(dirs);

    assert!(dir.is_dir());
}

/// A directory whose open failed after `mkdirat` cannot be told from whatever is at its name now.
#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_it_could_not_identify() {
    let layout = Layout::new();
    let mut dirs = make_chain(&layout);
    let victim = layout.system().join("a/b/c");
    dirs.made[2].held = None;
    fs::remove_dir(&victim).unwrap();
    fs::create_dir(&victim).unwrap();

    drop(dirs);

    assert!(victim.is_dir());
}

#[skuld::test(labels = [ELEVATED])]
fn drop_removes_a_handed_directory_that_is_still_empty_and_leaves_one_the_account_wrote_to() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("empty"), Some(stranger_owner()))
        .unwrap();
    dirs.create(&layout.system().join("used"), Some(stranger_owner()))
        .unwrap();
    dirs.hand_over().unwrap();
    fs::write(layout.system().join("used/data"), b"x").unwrap();

    drop(dirs);

    assert!(!layout.system().join("empty").exists());
    assert!(layout.system().join("used/data").is_file());
}

// Settling ------------------------------------------------------------------------------------------------------------

#[skuld::test(labels = [ELEVATED])]
fn settle_keeps_every_directory_when_the_definition_stands() {
    let layout = Layout::new();
    let mut dirs = make_chain(&layout);

    dirs.settle(|| Standing::Yes);
    drop(dirs);

    assert!(layout.system().join("a/b/c").is_dir());
}

/// A failure between making a directory and handing it over must not leave it root's for good when
/// the definition that names it stands.
#[skuld::test(labels = [ELEVATED])]
fn settle_hands_over_a_directory_that_never_was_when_the_definition_stands() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), Some(stranger_owner())).unwrap();

    dirs.settle(|| Standing::Yes);
    drop(dirs);

    assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
}

#[skuld::test(labels = [ELEVATED])]
fn settle_takes_the_directories_back_when_the_definition_does_not_stand() {
    let layout = Layout::new();
    let mut dirs = make_chain(&layout);

    dirs.settle(|| Standing::No);
    drop(dirs);

    assert!(!layout.system().join("a").exists());
}

/// An unreadable definition may stand: removing what it names could break it, so the directories
/// stay, handed over, as an install that stands leaves them.
#[skuld::test(labels = [ELEVATED])]
fn settle_keeps_and_hands_over_the_directories_when_the_read_did_not_complete() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), Some(stranger_owner())).unwrap();

    dirs.settle(|| Standing::Unknown("could not read the unit".to_string()));
    drop(dirs);

    assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
}

#[skuld::test(labels = [ELEVATED])]
fn settle_does_not_ask_when_nothing_was_made() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.settle(|| unreachable!("nothing was made"));
}

#[skuld::test(labels = [ELEVATED])]
fn drop_keeps_and_hands_over_the_directories_while_a_panic_unwinds() {
    let layout = Layout::new();
    let path = layout.system().join("a");

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut dirs = layout.dirs();
        dirs.create(&path, Some(stranger_owner())).unwrap();
        panic!("after the definition may have been written");
    }));

    assert!(unwound.is_err());
    assert_eq!(owner_of(&path), (STRANGER, STRANGER));
}
