use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

use nix::unistd::{Gid, Uid, chown};

use super::*;
use crate::error::Error;
use crate::testing::elevation::ELEVATED;
use crate::testing::groups::root_dirs;

/// A uid no account has, to own what a test makes foreign.
pub(crate) const STRANGER: u32 = 4242;

/// A directory under `/tmp`, which is root's on every host (a macOS temp directory is the
/// logged-in user's), holding `system`, the one root of the tests, and `outside`, which is not.
pub(crate) struct Layout {
    tmp: tempfile::TempDir,
}

impl Layout {
    pub(crate) fn new() -> Self {
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

    /// A root below a regular file: opening it says `ENOTDIR`.
    pub(crate) fn broken_root(&self) -> PathBuf {
        let file = self.base().join("a-file");
        fs::write(&file, b"x").unwrap();
        file.join("root")
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

/// The temporary directories an install leaves in `dir`.
fn leftovers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".goetia-"))
        .collect();
    names.sort();
    names
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
pub(crate) struct Fsuid(libc::uid_t);

#[cfg(target_os = "linux")]
impl Fsuid {
    pub(crate) fn set(uid: libc::uid_t) -> Self {
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
fn create_makes_every_missing_component_under_a_root_as_roots_0755(#[fixture(root_dirs)] _root: &()) {
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
fn create_leaves_a_directory_that_exists_as_found_wherever_it_is_and_whoever_owns_it(#[fixture(root_dirs)] _root: &()) {
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
fn create_uses_a_path_that_exists_whole_through_directories_and_links_an_account_owns(
    #[fixture(root_dirs)] _root: &(),
) {
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
fn create_refuses_a_missing_directory_outside_the_roots_naming_the_path_and_the_roots(
    #[fixture(root_dirs)] _root: &(),
) {
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
fn create_refuses_a_missing_directory_when_no_root_exists(#[fixture(root_dirs)] _root: &()) {
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
fn create_decides_under_a_root_on_the_resolved_path_not_the_text(#[fixture(root_dirs)] _root: &()) {
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

/// The service manager resolves the path as written when it opens it, so every directory the path
/// passes through decides where it leads, a directory it leaves through `..` included: the account that
/// owns `u` can replace `u/sub` with a symlink, and `u/sub/..` then leads elsewhere.
#[skuld::test(labels = [ELEVATED])]
fn create_judges_a_directory_the_path_leaves_through_dotdot(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let account = layout.system().join("u");
    fs::create_dir(&account).unwrap();
    give_to_a_stranger(&account);
    let mut dirs = layout.dirs();

    let err = dirs
        .create(&account.join("../new/deep"), None)
        .expect_err("a `..` from an account's directory is decided by that account");

    assert!(refusal(&err).contains(account.to_str().unwrap()), "{err:?}");
    assert!(!layout.system().join("new").exists());
}

/// An account that owns an ancestor of a symlink's holder can swap the holder, so the directories
/// before an absolute symlink's holder are judged too, though the link's own path restarts at `/`.
/// A directory that holds the link, and a link to `..`, are judged as well.
#[skuld::test(labels = [ELEVATED])]
fn create_judges_the_directories_before_the_holder_of_a_link(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let account = layout.system().join("u");
    fs::create_dir_all(account.join("v")).unwrap();
    give_to_a_stranger(&account);
    symlink(layout.system(), account.join("v/to-system")).unwrap();
    let mut dirs = layout.dirs();

    let err = dirs
        .create(&account.join("v/to-system/new"), None)
        .expect_err("u, the account's, can swap v, which holds the link");

    assert!(refusal(&err).contains(account.to_str().unwrap()), "{err:?}");
    assert!(!layout.system().join("new").exists());
    symlink(layout.system(), account.join("holds")).unwrap();
    symlink("..", account.join("up")).unwrap();
    for link in ["holds", "up"] {
        let err = dirs
            .create(&account.join(link).join("other"), None)
            .expect_err("the account that holds the link decides where it leads");
        assert!(refusal(&err).contains(account.to_str().unwrap()), "{link}: {err:?}");
    }
}

#[skuld::test(labels = [ELEVATED])]
fn create_follows_a_root_owned_link_into_a_root_and_judges_the_target_not_the_name(#[fixture(root_dirs)] _root: &()) {
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
fn create_refuses_a_missing_component_below_a_directory_an_account_owns(#[fixture(root_dirs)] _root: &()) {
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

/// A symlink's owner does not decide where it leads: only whoever can write the directory that holds
/// it can replace it, and that directory is judged like any other. A link an account made in a
/// directory only root can write is followed.
#[skuld::test(labels = [ELEVATED])]
fn create_makes_below_a_symlink_an_account_owns_when_only_root_can_write_its_holder(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let link = layout.system().join("link");
    symlink(layout.system().join("real"), &link).unwrap();
    fs::create_dir(layout.system().join("real")).unwrap();
    std::os::unix::fs::lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&link.join("new"), None).expect("root holds the link");

    assert!(layout.system().join("real/new").is_dir());
}

/// In a sticky directory a group or everyone can write (`/tmp`), the owner of an entry can delete it and
/// make another, so a link there that an account owns can be made to lead anywhere: it is refused. A link
/// root owns there, or an account's in a directory only root can write, is followed.
#[skuld::test(labels = [ELEVATED])]
fn create_refuses_to_make_below_a_symlink_an_account_owns_in_a_sticky_directory_others_can_write(
    #[fixture(root_dirs)] _root: &(),
) {
    for writable in [0o1777, 0o1775] {
        let layout = Layout::new();
        let real = layout.system().join("real");
        fs::create_dir(&real).unwrap();
        let sticky = layout.system().join("tmp");
        fs::create_dir(&sticky).unwrap();
        fs::set_permissions(&sticky, fs::Permissions::from_mode(writable)).unwrap();
        let link = sticky.join("link");
        symlink(&real, &link).unwrap();
        std::os::unix::fs::lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();
        let mut dirs = layout.dirs();

        let err = dirs
            .create(&link.join("new"), None)
            .expect_err("the account can replace its own link in a sticky directory");

        let why = refusal(&err);
        assert!(
            why.contains(link.to_str().unwrap()) && why.contains(&format!("uid {STRANGER}")),
            "{writable:o}: {why}"
        );
        assert!(!real.join("new").exists());

        fs::set_permissions(&sticky, fs::Permissions::from_mode(0o1755)).unwrap();
        dirs.create(&link.join("new"), None)
            .expect("only root can write the holder");
        fs::set_permissions(&sticky, fs::Permissions::from_mode(writable)).unwrap();
        std::os::unix::fs::lchown(&link, Some(0), Some(0)).unwrap();
        dirs.create(&link.join("new/deeper"), None).expect("root's link");
        assert!(real.join("new/deeper").is_dir());
    }
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_to_make_anything_below_a_dangling_link(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    symlink(layout.system().join("nowhere"), layout.system().join("link")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("link/a"), None)
        .expect_err("that would make the link's target");

    assert!(!layout.system().join("nowhere").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_dangling_link_or_a_file_at_the_last_name(#[fixture(root_dirs)] _root: &()) {
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

/// A symlink to a file is refused as the file is: the same error.
#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_symlink_to_a_file_at_the_last_name_as_a_file_is_refused(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::write(layout.system().join("file"), b"x").unwrap();
    for target in ["file", "../system/file", "./file/."] {
        symlink(target, layout.system().join("link")).unwrap();
        let mut dirs = layout.dirs();

        let err = dirs
            .create(&layout.system().join("link"), None)
            .expect_err("a file is not a directory");

        assert!(matches!(err, Error::NotADirectory { .. }), "{target}: {err:?}");
        fs::remove_file(layout.system().join("link")).unwrap();
    }
}

#[skuld::test(labels = [ELEVATED])]
fn create_under_a_regular_file_fails_and_makes_nothing(#[fixture(root_dirs)] _root: &()) {
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
fn create_accepts_a_symlink_to_a_directory_as_existing(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("real")).unwrap();
    symlink("real", layout.system().join("link")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("link"), None).unwrap();

    assert_eq!(fs::read_dir(layout.system().join("real")).unwrap().count(), 0);
}

#[skuld::test(labels = [ELEVATED])]
fn create_stops_at_a_symlink_loop(#[fixture(root_dirs)] _root: &()) {
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

/// A root whose path runs through a regular file (`ENOTDIR`) is not a root, like one that is not there:
/// it is skipped, whatever the other roots are, and when no other holds the path the path is outside.
#[skuld::test(labels = [ELEVATED])]
fn create_skips_a_root_below_a_regular_file(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let broken = layout.broken_root();
    let held = layout.system().join("a");

    for roots in [
        vec![broken.clone(), layout.system()],
        vec![layout.system(), broken.clone()],
    ] {
        let mut dirs = CreatedDirs::under(roots);
        dirs.create(&held, None).expect("under the root that holds it");
        assert!(held.is_dir());
        drop(dirs);
        assert!(!held.exists(), "taken back");
    }

    let err = CreatedDirs::under(vec![broken])
        .create(&held, None)
        .expect_err("no root holds it");
    assert!(refusal(&err).contains("create it first"), "{err:?}");
}

/// Any other failure to open a root is goetia's to report, not a root to skip: the install fails, naming
/// the root and the path being judged, even where a later root would have held the path.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn create_fails_with_a_root_it_cannot_read_naming_the_root_and_the_path(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::set_permissions(layout.base(), fs::Permissions::from_mode(0o711)).unwrap();
    let locked = layout.system().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = locked.join("root");
    let path = layout.system().join("a");
    let mut dirs = CreatedDirs::under(vec![unreadable.clone(), layout.system()]);

    let err = {
        let _as_a_stranger = Fsuid::set(STRANGER);
        dirs.create(&path, None).expect_err("the root cannot be read")
    };

    let Error::Os { path: at, .. } = &err else {
        panic!("expected an OS error, got {err:?}")
    };
    assert_eq!(at.as_deref(), Some(path.as_path()), "{err:?}");
    let source = std::error::Error::source(&err).expect("the OS error").to_string();
    assert!(
        source.contains(unreadable.to_str().unwrap()) && source.contains("Permission denied"),
        "{source}"
    );
    assert!(!path.exists());
}

/// A path that exists whole is used as it is, wherever it is: the roots are opened only when a
/// component is missing and the walk asks whether it is under one.
#[skuld::test(labels = [ELEVATED])]
fn create_uses_a_path_that_exists_whatever_state_the_roots_are_in(#[fixture(root_dirs)] _root: &()) {
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
fn create_skips_a_root_that_does_not_exist(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let nowhere = layout.base().join("nowhere");
    let mut dirs = CreatedDirs::under(vec![nowhere.clone(), layout.system()]);

    dirs.create(&layout.system().join("a"), None).unwrap();

    assert!(layout.system().join("a").is_dir());
    let err = CreatedDirs::under(vec![nowhere])
        .create(&layout.system().join("b"), None)
        .expect_err("no root holds it");
    assert!(refusal(&err).contains("create it first"), "{err:?}");
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_a_relative_path(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(Path::new("a/b"), None)
        .expect_err("a spec's directory is absolute");
}

#[skuld::test(labels = [ELEVATED])]
fn create_refuses_dotdot_below_a_component_that_does_not_exist(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/../b"), None)
        .expect_err("`a` would have to exist");

    assert!(!layout.system().join("a").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn walking_dotdot_at_the_root_stays_at_the_root(#[fixture(root_dirs)] _root: &()) {
    let mut dirs = CreatedDirs::under(vec![PathBuf::from("/")]);

    dirs.create(Path::new("/../.."), None).expect("the root is a directory");
}

/// A path with nothing left to walk is a directory that exists, wherever the roots are: `/` has no
/// last component, and a path that ends in `..` has none the walk takes.
#[skuld::test(labels = [ELEVATED])]
fn create_uses_the_root_directory_whatever_the_roots_are(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(Path::new("/"), None).expect("`/` exists");
    dirs.create(Path::new("/."), None).expect("`/.` is `/`");
}

#[skuld::test(labels = [ELEVATED])]
fn create_uses_a_path_that_ends_in_dotdot_as_the_directory_it_names(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::create_dir(layout.outside().join("deep")).unwrap();
    let mut dirs = layout.dirs();

    dirs.create(&layout.outside().join("deep/.."), None)
        .expect("it names the directory above `deep`, which exists");

    assert!(!layout.outside().join("deep/new").exists());
}

/// What an install made is put in place by a rename that replaces nothing, so what is at a name by then
/// is not the install's: it fails, whatever it is and whoever owns it, the entry is left as it is, and
/// nothing of the install's is left beside it. Running the install again uses it.
#[skuld::test(labels = [ELEVATED])]
fn make_refuses_what_appeared_at_the_final_name_after_the_walk_and_leaves_it_as_it_is(
    #[fixture(root_dirs)] _root: &(),
) {
    for kind in [
        "a stranger's directory",
        "a root-owned directory",
        "a file",
        "a symlink to a directory",
    ] {
        let layout = Layout::new();
        let raced = layout.system().join("a");
        let mut dirs = layout.dirs();
        let found = dirs.plan(&raced.join("b")).unwrap();
        match kind {
            "a file" => fs::write(&raced, b"x").unwrap(),
            "a symlink to a directory" => symlink(layout.outside(), &raced).unwrap(),
            _ => {
                fs::create_dir(&raced).unwrap();
                fs::set_permissions(&raced, fs::Permissions::from_mode(0o700)).unwrap();
                if kind == "a stranger's directory" {
                    give_to_a_stranger(&raced);
                }
            }
        }

        let err = dirs.make(vec![(found, None)]).expect_err("the name is taken");

        let why = refusal(&err);
        assert!(why.contains("appeared while the install ran"), "{kind}: {err:?}");
        // Only a root-owned directory is one the install uses when it is run again.
        assert_eq!(
            why.contains("running the install again uses it"),
            kind == "a root-owned directory",
            "{kind}: {err:?}"
        );
        let says = match kind {
            "a stranger's directory" => "owned by uid 4242",
            "a file" => "a file",
            "a symlink to a directory" => "a symlink",
            _ => "uses it",
        };
        assert!(
            why.contains(says) && why.contains("cannot use") == (kind != "a root-owned directory"),
            "{kind}: {err:?}"
        );
        drop(dirs);
        assert_eq!(leftovers(&layout.system()), Vec::<String>::new(), "{kind}");
        assert_eq!(fs::read_dir(layout.outside()).unwrap().count(), 0, "{kind}");
        if kind.ends_with("directory") && !kind.starts_with("a symlink") {
            assert_eq!(mode(&raced), 0o700, "{kind}");
            assert_eq!(
                fs::read_dir(&raced).unwrap().count(),
                0,
                "{kind}: nothing was put in it"
            );
        }
        if kind == "a stranger's directory" {
            assert_eq!(owner_of(&raced), (STRANGER, STRANGER));
        }
    }
}

/// An existing directory is used as it is found, whatever stands above it: a group-writable ancestor is
/// judged by its mode, and the walk reads nothing more of it (its POSIX ACL is read by the log check
/// alone, which needs it). One that this account cannot read, as a directory on NFS or FUSE may not be,
/// still lets an install that worked before work.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn create_uses_an_existing_directory_below_a_group_writable_one_that_cannot_be_read(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::set_permissions(layout.base(), fs::Permissions::from_mode(0o711)).unwrap();
    let shared = layout.outside().join("shared");
    let cwd = shared.join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    fs::set_permissions(&cwd, fs::Permissions::from_mode(0o755)).unwrap();
    // Group-writable, and others may search it but not open it for reading.
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o731)).unwrap();
    let mut dirs = layout.dirs();
    let _as_a_stranger = Fsuid::set(STRANGER);

    dirs.create(&cwd, None).expect("an existing path is used as it is");
}

/// What stands at the final name is a directory this same install made: the spec names one directory
/// twice (on a file system that ignores case, under two spellings). The refusal says so.
#[skuld::test(labels = [ELEVATED])]
fn make_says_two_paths_name_the_same_directory_when_the_name_taken_is_one_it_made(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let first = dirs.plan(&layout.system().join("a/x")).unwrap();
    let second = dirs.plan(&layout.system().join("b/y")).unwrap();
    dirs.make_missing(first.found, None).unwrap();
    dirs.make_missing(second.found, None).unwrap();
    // What a file system that ignores case does: the second tree's final name is the first's.
    let second_top = dirs
        .made
        .iter()
        .position(|made| made.pending.as_deref() == Some(OsStr::new("b")))
        .unwrap();
    dirs.made[second_top].pending = Some(OsString::from("a"));

    let err = dirs.place().expect_err("the first tree is at the name");

    assert!(
        refusal(&err).contains("two paths in the spec name the same directory"),
        "{err:?}"
    );
}

#[skuld::test(labels = [ELEVATED])]
fn create_does_not_descend_into_a_symlink_planted_after_the_walk(#[fixture(root_dirs)] _root: &()) {
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
fn a_made_directory_in_a_setgid_parent_is_accepted_and_given_0755(#[fixture(root_dirs)] _root: &()) {
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
fn a_made_directory_that_is_not_roots_is_refused_and_removed_with_the_rest(#[fixture(root_dirs)] _root: &()) {
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
        assert_eq!(leftovers(&parent).len(), 1, "it was made, under a temporary name");
        // It names the path the spec wrote, and says where the directory was while it was judged.
        let Error::DirectoryRefused { path, why } = &err else {
            unreachable!("`refusal` read it")
        };
        assert_eq!(path, &parent.join("new"));
        let temporary = parent.join(&leftovers(&parent)[0]);
        assert!(
            why.ends_with(&format!("(while it was being made at {})", temporary.display())),
            "{why}"
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(parent_after)).unwrap();
        drop(dirs);
        assert_eq!(leftovers(&parent), Vec::<String>::new(), "{parent_after:o}: left");
    }
}

/// Where the first directory of a tree is made, the name of the OS error is the spec's path, and the
/// temporary name it was being made under is said too.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_directory_that_cannot_be_made_names_the_specs_path_and_the_temporary_name_it_was_made_under(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let parent = layout.system().join("closed");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    let mut dirs = layout.dirs();
    let plan = dirs.plan(&parent.join("new")).unwrap();

    let result = {
        let _fs = Fsuid::set(STRANGER);
        dirs.make(vec![(plan, None)])
    };

    let err = result.expect_err("the stranger cannot write the directory");
    let Error::Os { what, path, source } = &err else {
        panic!("expected an OS error, got {err:?}")
    };
    assert_eq!(what, "create directory");
    assert_eq!(path.as_deref(), Some(parent.join("new").as_path()), "{err}");
    assert!(
        source.to_string().contains(&format!(
            "(while it was being made at {}",
            parent.join(".goetia-").display()
        )),
        "{err}"
    );
}

/// The temporary name means nothing to whoever wrote the spec: an OS error while the directory is made
/// names the path the spec wrote, and says where the directory is until it is placed.
#[skuld::test]
fn an_os_error_while_a_directory_is_made_names_the_specs_path_and_where_it_is_being_made() {
    let spec = Path::new("/var/log/app");
    let at = Path::new("/var/log/.goetia-0123");

    let err = os_while_making("open", spec, at, Errno::EACCES);

    let Error::Os { what, path, source } = &err else {
        panic!("expected an OS error, got {err:?}")
    };
    assert_eq!(what, "open");
    assert_eq!(path.as_deref(), Some(spec));
    let source = source.to_string();
    assert!(
        source.starts_with(&Errno::EACCES.desc().to_string())
            && source.ends_with("(while it was being made at /var/log/.goetia-0123)"),
        "{source}"
    );
}

// Planning ------------------------------------------------------------------------------------------------------------

/// A plan reads and judges, and makes nothing: what it refuses is refused before any directory is.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_plan_makes_nothing_and_says_whether_make_would(#[fixture(root_dirs)] _root: &()) {
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
fn make_hands_over_the_deepest_directory_to_the_owner_and_no_other_before_it_returns(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.create(&layout.system().join("a/b"), Some(stranger_owner()))
        .unwrap();

    assert_eq!(owner_of(&layout.system().join("a/b")), (STRANGER, STRANGER));
    assert_eq!(owner_of(&layout.system().join("a")), (0, owner_of(&layout.system()).1));
    assert_eq!(mode(&layout.system().join("a")), 0o755);
    assert_eq!(mode(&layout.system().join("a/b")), 0o755);
}

/// A `cwd` and a log directory inside it, both handed to the account, and a second path of its own: every
/// directory is at its name in its final state when `make` returns, and no temporary one is left.
#[skuld::test(labels = [ELEVATED])]
fn nested_directories_appear_in_their_final_state_and_leave_no_temporary_directory(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let logs = dirs.plan(&layout.system().join("x/y/logs")).unwrap();
    let cwd = dirs.plan(&layout.system().join("x/y")).unwrap();
    let other = dirs.plan(&layout.system().join("z/w")).unwrap();
    dirs.make(vec![
        (logs, Some(stranger_owner())),
        (cwd, Some(stranger_owner())),
        (other, None),
    ])
    .unwrap();

    for (made, uid) in [
        ("x", 0),
        ("x/y", STRANGER),
        ("x/y/logs", STRANGER),
        ("z", 0),
        ("z/w", 0),
    ] {
        let path = layout.system().join(made);
        assert_eq!(mode(&path), 0o755, "{made}");
        assert_eq!(owner_of(&path).0, uid, "{made}");
    }
    assert_eq!(leftovers(&layout.system()), Vec::<String>::new());
    drop(dirs);
    assert!(!layout.system().join("x").exists() && !layout.system().join("z").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn make_without_an_owner_leaves_everything_roots(#[fixture(root_dirs)] _root: &()) {
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
fn a_directory_an_earlier_plan_made_without_an_owner_is_never_handed_over_by_a_later_plan_that_names_it(
    #[fixture(root_dirs)] _root: &(),
) {
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
    let temporary = leftovers(&layout.system());
    assert_eq!(temporary.len(), 1, "{temporary:?}");
    assert_eq!(
        owner_of(&layout.system().join(&temporary[0])).0,
        0,
        "nothing was handed over"
    );
    assert!(!layout.system().join("a").exists());
    drop(dirs);
    assert!(
        !layout.system().join("a").exists(),
        "and the guard took back what was made"
    );
}

/// The same directory named twice by plans that hand it to the same owner is the owner's, as the first
/// plan said.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_an_earlier_plan_hands_over_stays_the_owners_when_a_later_plan_hands_it_too(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let first = dirs.plan(&layout.system().join("a")).unwrap();
    let again = dirs.plan(&layout.system().join("a")).unwrap();

    dirs.make(vec![(first, Some(stranger_owner())), (again, Some(stranger_owner()))])
        .unwrap();

    assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
}

/// The conflict between a plan that hands a directory over and one that does not is the same whichever
/// comes first: the directory, or one below it, is the account's, and so cannot be what the other plan
/// keeps as root's. Each order is refused and names the directory handed over and the account; nothing
/// is handed over, and the guard takes back what was made.
#[skuld::test(labels = [ELEVATED])]
fn a_plan_that_hands_nothing_over_and_one_that_hands_a_directory_over_may_not_share_it_in_either_order(
    #[fixture(root_dirs)] _root: &(),
) {
    // (handed, kept): the directory handed over is `a`; `kept` is `a` itself or below it.
    for kept in ["a", "a/sub"] {
        // The plan that hands nothing over first (a log's) and second.
        for kept_first in [true, false] {
            let layout = Layout::new();
            let mut dirs = layout.dirs();
            let handed = dirs.plan(&layout.system().join("a")).unwrap();
            let kept_plan = dirs.plan(&layout.system().join(kept)).unwrap();
            let plans = if kept_first {
                vec![(kept_plan, None), (handed, Some(stranger_owner()))]
            } else {
                vec![(handed, Some(stranger_owner())), (kept_plan, None)]
            };

            let err = dirs
                .make(plans)
                .expect_err("one directory cannot be root's and the account's");

            let Error::DirectoryRefused { path, why } = &err else {
                panic!("{kept} {kept_first}: {err:?}")
            };
            assert_eq!(path, &layout.system().join("a"), "{kept} {kept_first}: {why}");
            assert!(
                why.contains(&format!("uid {STRANGER}")) && !why.contains("  "),
                "{kept} {kept_first}: {why}"
            );
            assert!(
                !layout.system().join("a").exists(),
                "{kept} {kept_first}: nothing is placed"
            );
            drop(dirs);
            assert_eq!(
                leftovers(&layout.system()),
                Vec::<String>::new(),
                "{kept} {kept_first}: the guard took back what was made"
            );
        }
    }
}

/// A plan that hands nothing over and passes through or ends at a directory that another plan hands over
/// is a conflict whatever the order of the plans, also when a third plan made the directory and the one
/// that hands it over comes after the one that does not.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_a_plan_hands_over_is_refused_when_one_that_hands_nothing_over_passed_it_in_any_order(
    #[fixture(root_dirs)] _root: &(),
) {
    const ORDERS: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    // The plans: `a/b/c` handed over (it makes `a/b`), `kept` handed to no one, and `a/b` handed over.
    for kept in ["a/b", "a/b/e"] {
        for order in ORDERS {
            let layout = Layout::new();
            let mut dirs = layout.dirs();
            let mut plans = [
                (
                    dirs.plan(&layout.system().join("a/b/c")).unwrap(),
                    Some(stranger_owner()),
                ),
                (dirs.plan(&layout.system().join(kept)).unwrap(), None),
                (dirs.plan(&layout.system().join("a/b")).unwrap(), Some(stranger_owner())),
            ]
            .map(Some);
            let ordered: Vec<_> = order.into_iter().map(|index| plans[index].take().unwrap()).collect();

            let err = dirs
                .make(ordered)
                .expect_err("`a/b` cannot be root's for one path and the account's for another");

            let Error::DirectoryRefused { path, why } = &err else {
                panic!("{kept} {order:?}: {err:?}")
            };
            assert_eq!(path, &layout.system().join("a/b"), "{kept} {order:?}: {why}");
            assert!(why.contains(&format!("uid {STRANGER}")), "{kept} {order:?}: {why}");
            drop(dirs);
            assert_eq!(
                leftovers(&layout.system()),
                Vec::<String>::new(),
                "{kept} {order:?}: the guard took back what was made"
            );
            assert!(!layout.system().join("a").exists(), "{kept} {order:?}");
        }
    }
}

/// A plan that hands nothing over beside a directory handed over, or above it, is no conflict in either
/// order: that is the log directory beside or above the `cwd`.
#[skuld::test(labels = [ELEVATED])]
fn a_plan_that_hands_nothing_over_beside_or_above_a_handed_directory_is_accepted_in_either_order(
    #[fixture(root_dirs)] _root: &(),
) {
    for (handed, kept) in [("a/work", "a/logs"), ("a/work", "a")] {
        for kept_first in [true, false] {
            let layout = Layout::new();
            let mut dirs = layout.dirs();
            let handed_plan = dirs.plan(&layout.system().join(handed)).unwrap();
            let kept_plan = dirs.plan(&layout.system().join(kept)).unwrap();
            let plans = if kept_first {
                vec![(kept_plan, None), (handed_plan, Some(stranger_owner()))]
            } else {
                vec![(handed_plan, Some(stranger_owner())), (kept_plan, None)]
            };

            dirs.make(plans)
                .unwrap_or_else(|e| panic!("{handed} {kept} {kept_first}: {e}"));

            assert_eq!(owner_of(&layout.system().join(handed)), (STRANGER, STRANGER));
            assert_eq!(
                owner_of(&layout.system().join(kept)).0,
                0,
                "{handed} {kept} {kept_first}"
            );
        }
    }
}

/// A hand-over that fails is the install's error, never a warning, and the whole `make` returns it: every
/// directory made, the one that was to be handed over included, is taken back by the guard.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_make_whose_hand_over_fails_is_an_error_and_the_guard_removes_every_directory_made(
    #[fixture(root_dirs)] _root: &(),
) {
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
    let said = err.report().to_string();
    let temporary = leftovers(&layout.system());
    assert!(
        temporary.iter().any(|name| said.contains(name.as_str()))
            && said.contains(layout.system().join("a/b").to_str().unwrap()),
        "the error names the path the spec wrote and where the directory is: {said}"
    );
    for made in ["logs", "a"] {
        assert!(
            !layout.system().join(made).exists(),
            "{made} is not at its name until everything is made and handed over"
        );
    }
    assert_eq!(
        leftovers(&layout.system()).len(),
        2,
        "what was made is beside, under temporary names"
    );
    drop(dirs);
    assert_eq!(
        leftovers(&layout.system()),
        Vec::<String>::new(),
        "the guard took them back"
    );
}

/// What an account has written into a directory it was handed is its own: the undo does not empty it,
/// and says it is left.
#[skuld::test(labels = [ELEVATED])]
fn undo_leaves_a_handed_directory_the_account_has_written_into_and_names_it(#[fixture(root_dirs)] _root: &()) {
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
fn undo_leaves_a_directory_that_was_not_handed_over_and_gained_content_and_names_it(#[fixture(root_dirs)] _root: &()) {
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
fn undo_removes_a_directory_whatever_the_mode_and_owner_of_its_parent(#[fixture(root_dirs)] _root: &()) {
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
fn undo_removes_a_handed_directory_that_holds_a_handed_one_made_below_it(#[fixture(root_dirs)] _root: &()) {
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
fn a_handed_directory_that_holds_what_undo_left_is_not_blamed_on_the_account(#[fixture(root_dirs)] _root: &()) {
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
/// that owner: asking for another is an error that names the two, not a guess.
#[skuld::test(labels = [ELEVATED])]
fn a_later_plan_that_hands_a_made_directory_to_another_owner_is_an_error(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    let first = dirs.plan(&layout.system().join("a")).unwrap();
    let again = dirs.plan(&layout.system().join("a")).unwrap();
    let other = Owner::new(Uid::from_raw(STRANGER + 1), None).unwrap();

    let err = dirs
        .make(vec![(first, Some(stranger_owner())), (again, Some(other))])
        .expect_err("one directory has one owner");

    let why = refusal(&err);
    assert!(
        why.contains(&STRANGER.to_string()) && why.contains(&(STRANGER + 1).to_string()),
        "{why}"
    );
    drop(dirs);
    assert!(!layout.system().join("a").exists(), "nothing is left");
}

/// The refusal names the path of the plan that asks, in the spelling its spec wrote, not the earlier
/// plan's spelling of the same directory.
#[skuld::test(labels = [ELEVATED])]
fn the_refusal_of_a_second_owner_names_the_path_the_asking_plan_wrote(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    symlink(".", layout.system().join("link")).unwrap();
    let mut dirs = layout.dirs();
    let first = dirs.plan(&layout.system().join("a")).unwrap();
    let again_path = layout.system().join("link/a");
    let again = dirs.plan(&again_path).unwrap();
    let other = Owner::new(Uid::from_raw(STRANGER + 1), None).unwrap();

    let err = dirs
        .make(vec![(first, Some(stranger_owner())), (again, Some(other))])
        .expect_err("one directory has one owner");

    let Error::DirectoryRefused { path, .. } = &err else {
        panic!("expected a refused directory, got {err:?}")
    };
    assert_eq!(path, &again_path);
}

#[skuld::test(labels = [ELEVATED])]
fn an_owner_refuses_the_ids_that_mean_unchanged(#[fixture(root_dirs)] _root: &()) {
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
fn drop_removes_every_directory_made_deepest_first(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();

    drop(make_chain(&layout));

    assert!(!layout.system().join("a").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_that_existed_before(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("before")).unwrap();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("before/new"), None).unwrap();

    drop(dirs);

    assert!(layout.system().join("before").is_dir());
    assert!(!layout.system().join("before/new").exists());
}

#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_that_is_no_longer_empty_and_its_ancestors(#[fixture(root_dirs)] _root: &()) {
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
fn drop_does_not_follow_a_symlink_swapped_in_for_a_made_directory(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::create_dir(layout.outside().join("c")).unwrap();
    let dirs = make_chain(&layout);
    fs::rename(layout.system().join("a/b"), layout.system().join("moved")).unwrap();
    symlink(layout.outside(), layout.system().join("a/b")).unwrap();

    drop(dirs);

    assert!(layout.outside().join("c").is_dir(), "the link's target was emptied");
}

#[skuld::test(labels = [ELEVATED])]
fn drop_leaves_a_directory_that_is_no_longer_the_one_made(#[fixture(root_dirs)] _root: &()) {
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
fn drop_leaves_a_directory_it_could_not_identify(#[fixture(root_dirs)] _root: &()) {
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
fn settle_keeps_every_directory_when_the_definition_stands(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = make_chain(&layout);

    dirs.settle(|| Standing::Yes);
    drop(dirs);

    assert!(layout.system().join("a/b/c").is_dir());
}

#[skuld::test(labels = [ELEVATED])]
fn settle_takes_the_directories_back_when_the_definition_does_not_stand(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = make_chain(&layout);

    dirs.settle(|| Standing::No);
    drop(dirs);

    assert!(!layout.system().join("a").exists());
}

/// An unreadable definition may stand: removing what it names could break it, so the directories
/// stay, handed over, as an install that stands leaves them.
#[skuld::test(labels = [ELEVATED])]
fn settle_keeps_the_directories_when_the_read_did_not_complete(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("a"), Some(stranger_owner())).unwrap();

    dirs.settle(|| Standing::Unknown("could not read the unit".to_string()));
    drop(dirs);

    assert_eq!(owner_of(&layout.system().join("a")), (STRANGER, STRANGER));
}

/// A `make` that did not finish (a panic unwinding out of it) leaves its trees at their temporary names,
/// which no definition can name: keeping what stands removes them and keeps what was put in place.
#[skuld::test(labels = [ELEVATED])]
fn keep_removes_a_tree_that_was_not_put_in_place_and_keeps_the_rest(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();
    dirs.create(&layout.system().join("placed"), None).unwrap();
    let plan = dirs.plan(&layout.system().join("a/b")).unwrap();
    dirs.make_missing(plan.found, Some(stranger_owner())).unwrap();
    assert_eq!(leftovers(&layout.system()).len(), 1);

    dirs.keep();

    assert_eq!(leftovers(&layout.system()), Vec::<String>::new());
    assert!(!layout.system().join("a").exists());
    assert!(layout.system().join("placed").is_dir());
}

#[skuld::test(labels = [ELEVATED])]
fn settle_does_not_ask_when_nothing_was_made(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let mut dirs = layout.dirs();

    dirs.settle(|| unreachable!("nothing was made"));
}

#[skuld::test(labels = [ELEVATED])]
fn drop_keeps_the_directories_while_a_panic_unwinds(#[fixture(root_dirs)] _root: &()) {
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

/// What an undo says of a directory it could not remove depends on how it came to be made: one of a
/// tree that was never put in place belongs to no definition, and the install did not fail.
#[skuld::test]
fn a_directory_that_could_not_be_removed_is_described_by_how_it_was_made() {
    let at = Path::new("/var/lib/app");

    let undone = removal_failure(at, "it holds a file", Removal::AfterFailure);
    let unplaced = removal_failure(at, "it holds a file", Removal::Unplaced);

    assert_eq!(
        undone,
        "could not remove /var/lib/app, which the failed install created: it holds a file"
    );
    assert!(unplaced.contains("never put in place"), "{unplaced}");
    assert!(!unplaced.contains("failed install"), "{unplaced}");
    assert!(
        unplaced.contains("/var/lib/app") && unplaced.ends_with("it holds a file"),
        "{unplaced}"
    );
}

/// A path from the spec is text a person reads on stderr: a control character in it is written escaped,
/// as the CLI writes what it quotes.
#[skuld::test]
fn a_warning_escapes_the_paths_it_names() {
    let path = Path::new("/var/lib/a\nerror: b\u{1b}[2J");

    let line = warning_line(format_args!("could not remove {}", path.display()));

    assert_eq!(line, r"warning: could not remove /var/lib/a\u{a}error: b\u{1b}[2J");
}
