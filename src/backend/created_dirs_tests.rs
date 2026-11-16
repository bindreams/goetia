use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

use nix::unistd::{Gid, Uid, chown};

use super::*;
use crate::error::Error;
use crate::testing::elevation::{ELEVATED, require_elevated};

/// A uid no account has, to own what a test makes foreign.
pub(crate) const STRANGER: u32 = 4242;

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

    pub(crate) fn outside(&self) -> PathBuf {
        self.base().join("outside")
    }

    /// A guard that treats `system` as its only root.
    fn dirs(&self) -> CreatedDirs {
        CreatedDirs::under(vec![self.system()])
    }
}

pub(crate) fn give_to_a_stranger(path: &Path) {
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

/// `plan` and `make` of one path, in one call: the production callers plan every path before they make
/// any, which is what the ordering tests below do by hand.
pub(crate) trait Create {
    fn create(&mut self, dir: &Path, hand_to: Option<Owner>) -> Result<()>;
}

impl Create for CreatedDirs {
    fn create(&mut self, dir: &Path, hand_to: Option<Owner>) -> Result<()> {
        let plan = self.plan(dir)?;
        self.make(vec![(plan, hand_to)])
    }
}

/// The file-system uid of this thread, put back on drop. Setting one that is not root's takes away
/// the capabilities that let root change another's files and write where it has no permission.
#[cfg(target_os = "linux")]
struct Fsuid(libc::uid_t);

#[cfg(target_os = "linux")]
impl Fsuid {
    fn set(uid: libc::uid_t) -> Self {
        // SAFETY: `setfsuid` takes and returns an id and touches only this thread's credentials.
        Self(unsafe { libc::setfsuid(uid) } as libc::uid_t)
    }
}

#[cfg(target_os = "linux")]
impl Drop for Fsuid {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe { libc::setfsuid(self.0) };
    }
}

/// This thread without `CAP_CHOWN`, put back on drop: root can still make directories, and cannot give
/// one to another uid. Capabilities are per thread, so only the thread that holds the guard is affected.
#[cfg(target_os = "linux")]
struct WithoutChown {
    held: [CapData; 2],
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[cfg(target_os = "linux")]
impl WithoutChown {
    /// `_LINUX_CAPABILITY_VERSION_3`: two words of capabilities. `pid` 0 is the calling thread.
    const HEADER: CapHeader = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    const CAP_CHOWN: u32 = 1 << 0;

    fn new() -> Self {
        let mut held = [CapData::default(); 2];
        // SAFETY: `capget` fills the two words the header's version names.
        let got = unsafe { libc::syscall(libc::SYS_capget, &Self::HEADER, held.as_mut_ptr()) };
        assert_eq!(got, 0, "capget: {}", std::io::Error::last_os_error());
        let mut without = held;
        without[0].effective &= !Self::CAP_CHOWN;
        Self::set(&without).expect("capset");
        Self { held }
    }

    fn set(data: &[CapData; 2]) -> std::io::Result<()> {
        // SAFETY: `capset` reads the two words the header's version names, for this thread.
        match unsafe { libc::syscall(libc::SYS_capset, &Self::HEADER, data.as_ptr()) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for WithoutChown {
    fn drop(&mut self) {
        if let Err(e) = Self::set(&self.held) {
            eprintln!("WithoutChown: the capability was not put back: {e}");
        }
    }
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
    dirs.settle(|| Standing::Yes);

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
    let mut dirs = layout.dirs();
    let found = dirs.plan(&leaf).unwrap();
    fs::create_dir(&raced).unwrap();
    fs::set_permissions(&raced, fs::Permissions::from_mode(0o700)).unwrap();

    dirs.make(vec![(found, None)]).unwrap();

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
    let mut dirs = layout.dirs();
    let found = dirs.plan(&raced.join("b")).unwrap();
    fs::create_dir(&raced).unwrap();
    give_to_a_stranger(&raced);

    let err = dirs.make(vec![(found, None)]).expect_err("an account owns it now");

    assert!(refusal(&err).contains(&format!("uid {STRANGER}")));
    assert!(!raced.join("b").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_does_not_descend_into_a_symlink_planted_after_the_walk() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let found = dirs.plan(&layout.system().join("a/b")).unwrap();
    symlink(layout.outside(), layout.system().join("a")).unwrap();

    dirs.make(vec![(found, None)])
        .expect_err("a symlink is not a directory");

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
/// refused; it was recorded when it was made, so it is removed with the rest, whoever can write the
/// parent.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_made_directory_that_is_not_roots_is_refused_and_removed_with_the_rest() {
    for parent_after in [0o777, 0o755] {
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
        fs::set_permissions(&parent, fs::Permissions::from_mode(parent_after)).unwrap();
        drop(dirs);
        assert!(!parent.join("new").exists(), "{parent_after:o}: left");
    }
}

// Planning ------------------------------------------------------------------------------------------------------------

/// A plan reads and judges, and makes nothing: what it refuses is refused before any directory is.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_plan_makes_nothing_and_says_whether_make_would() {
    let layout = Layout::new();
    let dirs = layout.dirs();

    let missing = dirs.plan(&layout.system().join("a/b/c")).unwrap();
    let existing = dirs.plan(&layout.system()).unwrap();

    assert!(missing.makes());
    assert!(!existing.makes());
    assert!(!layout.system().join("a").exists());
    assert!(dirs.made.is_empty());
}

// Handing over --------------------------------------------------------------------------------------------------------

/// The definition can go live as soon as the write that follows starts: what is to be the account's is
/// the account's when `make` returns, and nothing but the deepest directory of a path is.
#[skuld::test(labels = [ELEVATED])]
fn make_hands_over_the_deepest_directory_to_the_owner_and_no_other_before_it_returns() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b"), Some(stranger_owner()))
        .unwrap();

    assert_eq!(owner_of(&layout.system().join("a/b")), (STRANGER, STRANGER));
    assert_eq!(owner_of(&layout.system().join("a")), (0, owner_of(&layout.system()).1));
    assert_eq!(mode(&layout.system().join("a")), 0o755);
    assert_eq!(mode(&layout.system().join("a/b")), 0o755);
}

#[skuld::test(labels = [ELEVATED])]
fn make_without_an_owner_leaves_everything_roots() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b"), None).unwrap();

    assert_eq!(owner_of(&layout.system().join("a/b")).0, 0);
}

/// A directory an earlier plan made is that plan's: a later plan that names it and is to hand it over
/// is a conflict between the spec's paths (a log directory is never handed to the account, and `cwd`
/// is), refused, never upgraded to the later plan's owner. Whether two names are one directory is
/// the file system's to say, which is why this is judged on what was made, not on the names.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_an_earlier_plan_made_without_an_owner_is_never_handed_over_by_a_later_plan_that_names_it() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let below = dirs.plan(&layout.system().join("a/b")).unwrap();
    let named = dirs.plan(&layout.system().join("a")).unwrap();

    let err = dirs
        .make(vec![(below, None), (named, Some(stranger_owner()))])
        .expect_err("the directory above `a/b` is one root's, and is not to be handed over");

    let why = refusal(&err);
    assert!(
        why.contains(&format!("uid {STRANGER}")) && why.contains("another"),
        "{why}"
    );
    assert_eq!(owner_of(&layout.system().join("a")).0, 0, "nothing was handed over");
    drop(dirs);
    assert!(
        !layout.system().join("a").exists(),
        "and the guard took back what was made"
    );
}

/// The same directory named twice by plans that hand it to the same owner, or by one that hands it and
/// one that does not hand it after, is the owner's, as the first plan said.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_an_earlier_plan_hands_over_stays_the_owners_when_a_later_plan_names_it() {
    for later in [Some(stranger_owner()), None] {
        let layout = Layout::new();
        let mut dirs = layout.dirs();
        let first = dirs.plan(&layout.system().join("a")).unwrap();
        let again = dirs.plan(&layout.system().join("a")).unwrap();

        dirs.make(vec![(first, Some(stranger_owner())), (again, later)])
            .unwrap();

        assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
    }
}

/// A hand-over that fails is the install's error, never a warning, and the whole `make` returns it: every
/// directory made, the one that was to be handed over included, is taken back by the guard.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_make_whose_hand_over_fails_is_an_error_and_the_guard_removes_every_directory_made() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let log = dirs.plan(&layout.system().join("logs/x")).unwrap();
    let cwd = dirs.plan(&layout.system().join("a/b")).unwrap();

    let result = {
        let _no_chown = WithoutChown::new();
        dirs.make(vec![(log, None), (cwd, Some(stranger_owner()))])
    };

    let err = result.expect_err("root without the right to give a directory away cannot hand one over");
    assert!(err.to_string().contains("hand over"), "{err}");
    for made in ["logs/x", "a/b"] {
        assert_eq!(
            owner_of(&layout.system().join(made)).0,
            0,
            "{made} was made, and is root's"
        );
    }
    drop(dirs);
    for made in ["logs", "a"] {
        assert!(!layout.system().join(made).exists(), "the guard left {made}");
    }
}

/// What an account has written into a directory it was handed is its own: the undo does not empty it,
/// and says it is left.
#[skuld::test(labels = [ELEVATED])]
fn undo_leaves_a_handed_directory_the_account_has_written_into_and_names_it() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), Some(stranger_owner())).unwrap();
    fs::write(layout.system().join("a/data"), b"x").unwrap();

    let left = dirs.remove(0).expect_err("it holds a file");
    assert!(left.contains("handed"), "{left}");
    drop(dirs);

    assert_eq!(fs::read(layout.system().join("a/data")).unwrap(), b"x");
}

/// One that was not handed over and has content stays too, and is named like every other that is left.
#[skuld::test(labels = [ELEVATED])]
fn undo_leaves_a_directory_that_was_not_handed_over_and_gained_content_and_names_it() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), None).unwrap();
    fs::write(layout.system().join("a/data"), b"x").unwrap();

    let left = dirs.remove(0).expect_err("it holds a file");

    assert!(left.contains("content"), "{left}");
    assert!(!left.contains("account"), "{left}");
    assert_eq!(fs::read(layout.system().join("a/data")).unwrap(), b"x");
}

/// A directory is removed by its name in its parent after its identity is checked, under any parent:
/// where others can write the parent, a directory they put at the name in between is empty, and is
/// removed as they could remove it themselves.
#[skuld::test(labels = [ELEVATED])]
fn undo_removes_a_directory_whatever_the_mode_and_owner_of_its_parent() {
    for mode in [0o755, 0o700, 0o1777, 0o775, 0o757] {
        let layout = Layout::new();
        let mut dirs = layout.dirs();
        dirs.create(&layout.system().join("a"), None).unwrap();
        fs::set_permissions(layout.system(), fs::Permissions::from_mode(mode)).unwrap();

        dirs.remove(0).unwrap_or_else(|e| panic!("{mode:o}: {e}"));

        assert!(!layout.system().join("a").exists(), "{mode:o}");
    }

    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), None).unwrap();
    give_to_a_stranger(&layout.system());

    dirs.remove(0).expect("a parent another user owns");

    assert!(!layout.system().join("a").exists(), "a parent another user owns");
}

/// A `cwd` that holds the log directory, both handed to the account, as launchd's spec makes them: a
/// failed install removes both, the one below first.
#[skuld::test(labels = [ELEVATED])]
fn undo_removes_a_handed_directory_that_holds_a_handed_one_made_below_it() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let logs = dirs.plan(&layout.system().join("x/logs")).unwrap();
    let cwd = dirs.plan(&layout.system().join("x")).unwrap();
    dirs.make(vec![(logs, Some(stranger_owner())), (cwd, Some(stranger_owner()))])
        .unwrap();
    for made in ["x", "x/logs"] {
        assert_eq!(owner_of(&layout.system().join(made)).0, STRANGER, "{made} is handed");
    }

    drop(dirs);

    assert!(!layout.system().join("x").exists());
}

/// What is left in a handed directory is the account's only if goetia did not make it: a directory the
/// install made below it and left is named as that, not as the account's writing.
#[skuld::test(labels = [ELEVATED])]
fn a_handed_directory_that_holds_what_undo_left_is_not_blamed_on_the_account() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let logs = dirs.plan(&layout.system().join("x/logs")).unwrap();
    let cwd = dirs.plan(&layout.system().join("x")).unwrap();
    dirs.make(vec![(logs, Some(stranger_owner())), (cwd, Some(stranger_owner()))])
        .unwrap();
    dirs.made[1].held = None;

    let below = dirs.remove(1).expect_err("it could not be identified");
    let above = dirs.remove(0).expect_err("it holds what was left");

    assert!(below.contains("could not be opened"), "{below}");
    assert!(
        above.contains(&layout.system().join("x/logs").display().to_string()),
        "{above}"
    );
    assert!(!above.contains("written"), "{above}");
}

/// A later plan that hands over a directory an earlier plan made and marked for an owner hands it to
/// that owner: asking for another is a bug of the caller, which gives every plan one account.
#[cfg(debug_assertions)]
#[skuld::test(labels = [ELEVATED])]
#[should_panic(expected = "another owner")]
fn a_later_plan_that_hands_a_made_directory_to_another_owner_is_a_bug() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let first = dirs.plan(&layout.system().join("a")).unwrap();
    let again = dirs.plan(&layout.system().join("a")).unwrap();
    let other = Owner::new(Uid::from_raw(STRANGER + 1), None).unwrap();

    let _ = dirs.make(vec![(first, Some(stranger_owner())), (again, Some(other))]);
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

// Settling ------------------------------------------------------------------------------------------------------------

#[skuld::test(labels = [ELEVATED])]
fn settle_keeps_every_directory_when_the_definition_stands() {
    let layout = Layout::new();
    let mut dirs = make_chain(&layout);

    dirs.settle(|| Standing::Yes);
    drop(dirs);

    assert!(layout.system().join("a/b/c").is_dir());
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
fn settle_keeps_the_directories_when_the_read_did_not_complete() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), Some(stranger_owner())).unwrap();

    dirs.settle(|| Standing::Unknown("could not read the unit".to_string()));
    drop(dirs);

    assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
}

/// A directory `verify` refused, or one that could not be opened to be judged, was never shown to be
/// the one that was made: keeping what stands says it is left.
#[skuld::test(labels = [ELEVATED])]
fn keep_leaves_a_directory_that_was_not_verified_and_names_it() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("refused"), None).unwrap();
    dirs.create(&layout.system().join("unopened"), None).unwrap();
    dirs.create(&layout.system().join("verified"), None).unwrap();
    dirs.made[0].ready = false;
    dirs.made[1].held = None;
    dirs.made[1].ready = false;

    let notes = dirs.keep();

    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].contains(&layout.system().join("refused").display().to_string()),
        "{notes:?}"
    );
    assert!(
        notes[1].contains(&layout.system().join("unopened").display().to_string()),
        "{notes:?}"
    );
}

/// A directory that is ready, marked for an owner and not yet handed (a `make` that stopped before it
/// handed anything over) is handed over when what stands is kept, whether the definition's standing is
/// unknown or the thread is unwinding from a panic.
#[skuld::test(labels = [ELEVATED])]
fn keep_hands_over_a_ready_directory_marked_for_an_owner() {
    let layout = Layout::new();
    let made_but_not_handed = |dirs: &mut CreatedDirs, name: &str| {
        let plan = dirs.plan(&layout.system().join(name)).unwrap();
        dirs.make_missing(plan.found, Some(stranger_owner())).unwrap();
        assert_eq!(
            owner_of(&layout.system().join(name)).0,
            0,
            "{name} is made and not handed"
        );
    };

    let mut dirs = layout.dirs();
    made_but_not_handed(&mut dirs, "unknown");
    dirs.settle(|| Standing::Unknown("could not read the unit".to_string()));
    drop(dirs);
    assert_eq!(owner_of(&layout.system().join("unknown")), (STRANGER, STRANGER));

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut dirs = layout.dirs();
        made_but_not_handed(&mut dirs, "panicking");
        panic!("after the definition may have been written");
    }));
    assert!(unwound.is_err());
    assert_eq!(owner_of(&layout.system().join("panicking")), (STRANGER, STRANGER));
}

/// One that cannot be handed over is kept root's and named, with what the hand-over said.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn keep_names_a_directory_it_cannot_hand_over() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let plan = dirs.plan(&layout.system().join("a")).unwrap();
    dirs.make_missing(plan.found, Some(stranger_owner())).unwrap();

    let notes = {
        let _no_chown = WithoutChown::new();
        dirs.keep()
    };

    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(&layout.system().join("a").display().to_string())
            && notes[0].contains(&format!("uid {STRANGER}"))
            && notes[0].contains("root"),
        "{notes:?}"
    );
    assert_eq!(owner_of(&layout.system().join("a")).0, 0);
}

#[skuld::test(labels = [ELEVATED])]
fn settle_does_not_ask_when_nothing_was_made() {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.settle(|| unreachable!("nothing was made"));
}

#[skuld::test(labels = [ELEVATED])]
fn drop_keeps_the_directories_while_a_panic_unwinds() {
    let layout = Layout::new();
    let path = layout.system().join("a");

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut dirs = layout.dirs();
        dirs.create(&path, Some(stranger_owner())).unwrap();
        panic!("after the definition may have been written");
    }));

    assert!(unwound.is_err());
    assert_eq!(
        owner_of(&path),
        (STRANGER, STRANGER),
        "kept, as handed over when it was made"
    );
}
