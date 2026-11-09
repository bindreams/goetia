//! Unit coverage for the pieces of the launchd backend that neither need
//! root nor the real `/Library/...` system paths: `persist_new`/
//! `write_existing` take an arbitrary target path, so the non-clobbering
//! race and the `plutil -lint` gate are both fully testable against a
//! `tempfile::tempdir()`. Everything that genuinely needs elevation and the
//! real filesystem locations (`locate`/`read`, `enable`/`disable`,
//! `start`/`stop` against real launchd) lives in
//! `tests/launchd_integration.rs` instead.

use super::*;
use crate::backend::bounded::Fate;
use crate::backend::client::fake::{Ended, Entry, Fake, RoleKind, Waited, exited, fail, lingering, reply};
use crate::backend::client::{Client, ClientError, Real};
use crate::manager::ServiceManager;
use crate::spec::DaemonFields;
use crate::spec::test_support::plain_fields;
use crate::spec::{Id, Kind, Restart, User};

use crate::backend::created_dirs::CreatedDirs;
use crate::backend::test_cleanup::{FileGuard, remove_or_warn};
use crate::backend::test_elevation::{ELEVATED, elevated};

fn sample_plist(id: &str) -> String {
    sample_plist_restarting(id, Restart::Never)
}

fn sample_plist_restarting(id: &str, restart: Restart) -> String {
    let spec = DaemonSpec::new(DaemonFields {
        restart,
        kind: Kind::Simple,
        ..plain_fields(id, vec!["/bin/true".to_string()])
    })
    .unwrap();
    generate::plist(
        &spec,
        &Identity {
            user: "root".to_string(),
        },
    )
}

// persist_new / write_existing ========================================================================================

#[skuld::test]
fn persist_new_creates_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("fresh.plist");
    let content = sample_plist("fresh");

    let staged = staged_tempfile(dir.path(), &content).unwrap();
    let result = persist_new(staged, &target).expect("persist_new over an absent target");
    assert!(matches!(result, PersistOutcome::Written));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
}

/// The mechanism `install`'s "create must not clobber" obligation rests on:
/// by the time `persist_new` runs, discovery already classified the target as
/// absent, so anything present now (a foreign plist, or a concurrent
/// installer's) arrived after that classification — exactly the shape of
/// the TOCTOU race a plain unconditional `rename` would lose. Simulating
/// "something is already there" directly (rather than trying to force a
/// genuine two-thread race, which no amount of `Barrier` synchronization
/// can *guarantee* lands inside a window of a few instructions) reproduces
/// the one fact that matters: `persist_new` must never overwrite it.
#[skuld::test]
fn persist_new_does_not_clobber_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("raced.plist");
    std::fs::write(&target, "not a goetia artifact\n").unwrap();

    let staged = staged_tempfile(dir.path(), &sample_plist("raced")).unwrap();
    let result = persist_new(staged, &target).expect("persist_new over a raced target");
    assert!(matches!(result, PersistOutcome::Raced));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "not a goetia artifact\n",
        "a losing create must never modify the winner's content"
    );
}

#[skuld::test]
fn write_existing_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("id.plist");
    std::fs::write(&target, sample_plist("id")).unwrap();

    let updated = sample_plist("id").replace("/bin/true", "/bin/false");
    write_existing(&target, &updated).expect("write_existing over an already-ours target");

    assert_eq!(std::fs::read_to_string(&target).unwrap(), updated);
}

// undo_write ==========================================================================================================

/// The put-back after an install that ended in doubt writes only over the plist `install` wrote.
#[skuld::test]
fn undo_write_puts_the_previous_plist_over_what_install_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("id.plist");
    let previous = sample_plist("id");
    let wrote = previous.replace("/bin/true", "/bin/false");
    std::fs::write(&target, &wrote).unwrap();

    undo_write(&target, &wrote, Some(&previous)).expect("undo over what install wrote");

    assert_eq!(std::fs::read_to_string(&target).unwrap(), previous);
}

/// A `Create` has no previous plist: undoing it removes the one it created.
#[skuld::test]
fn undo_write_removes_a_plist_that_install_created() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("id.plist");
    let wrote = sample_plist("id");
    std::fs::write(&target, &wrote).unwrap();

    undo_write(&target, &wrote, None).expect("undo of a create");

    assert!(!target.exists());
}

/// A concurrent install that wrote after ours owns the path now; the put-back leaves it, and the
/// failure says so.
#[skuld::test]
fn undo_write_leaves_a_plist_that_is_no_longer_what_install_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("id.plist");
    let wrote = sample_plist("id");
    std::fs::write(&target, "someone else's plist\n").unwrap();

    for previous in [Some(wrote.as_str()), None] {
        let err = undo_write(&target, &wrote, previous).expect_err("not what install wrote");
        assert!(
            matches!(
                err,
                Error::PutBackBlocked {
                    found: PutBackFound::Replaced,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains(&target.display().to_string()), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else's plist\n");
    }
}

/// A waited start read back as stopped or failed did not start; one read back as `Unknown`
/// (`spawn scheduled`, or a read that failed) is not established either way, so it is exit `4`.
#[skuld::test]
fn a_start_read_back_as_unknown_is_unconfirmed_not_failed() {
    let id = Id::try_from("x").unwrap();
    for state in [State::Stopped, State::Failed] {
        let e = not_started(&id, state, false);
        assert!(matches!(e, Error::NotStarted { .. }), "{state:?}: {e:?}");
        assert!(!e.is_indeterminate(), "{state:?}");
    }
    let e = not_started(&id, State::Unknown, true);
    assert!(matches!(e, Error::StartUnconfirmed { .. }), "{e:?}");
    assert!(e.is_indeterminate());
}

/// `install` writes a regular file, so a symlink at the path, even a dangling one, is someone
/// else's: replaced, never "no longer exists", and left in place.
#[skuld::test]
fn undo_write_calls_a_symlink_at_the_path_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("id.plist");
    let wrote = sample_plist("id");
    std::os::unix::fs::symlink(dir.path().join("nowhere"), &target).unwrap();

    let err = undo_write(&target, &wrote, None).expect_err("a symlink is not what install wrote");
    assert!(
        matches!(
            err,
            Error::PutBackBlocked {
                found: PutBackFound::Replaced,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        std::fs::symlink_metadata(&target).is_ok(),
        "the symlink was left in place"
    );
}

/// A concurrent `uninstall` or `enable` took the plist from this path; writing it back would resurrect
/// it, or put a plist in both directories.
#[skuld::test]
fn undo_write_does_not_recreate_a_plist_that_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("id.plist");
    let wrote = sample_plist("id");

    for previous in [Some(wrote.as_str()), None] {
        let err = undo_write(&target, &wrote, previous).expect_err("nothing there");
        assert!(
            matches!(
                err,
                Error::PutBackBlocked {
                    found: PutBackFound::Gone,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(!target.exists(), "the put-back recreated the plist");
    }
}

#[skuld::test]
fn staged_tempfile_rejects_content_that_fails_plutil_lint() {
    let dir = tempfile::tempdir().unwrap();

    let err = staged_tempfile(dir.path(), "<not-a-plist-at-all>").expect_err("plutil -lint should reject this");
    assert!(
        err.to_string().contains("plutil"),
        "error should name the failing tool: {err}"
    );
}

#[skuld::test]
fn staged_tempfile_accepts_a_generated_plist() {
    let dir = tempfile::tempdir().unwrap();
    staged_tempfile(dir.path(), &sample_plist("lint-ok")).expect("a real generated plist must pass `plutil -lint`");
}

// move_no_clobber =====================================================================================================

#[skuld::test]
fn move_no_clobber_moves_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.plist");
    let dest = dir.path().join("dest.plist");
    std::fs::write(&src, "content").unwrap();

    move_no_clobber(&src, &dest).expect("move over an absent destination");

    assert!(!src.exists(), "source must be gone after a successful move");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "content");
}

/// The same TOCTOU class `persist_new` guards against for `install`'s create
/// path, but for the move `enable`/`disable` perform: a plain `fs::rename`
/// would silently replace whatever landed at `dest` between a caller's own
/// checks and the move.
#[skuld::test]
fn move_no_clobber_does_not_clobber_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.plist");
    let dest = dir.path().join("dest.plist");
    std::fs::write(&src, "mover content").unwrap();
    std::fs::write(&dest, "already here").unwrap();

    let err = move_no_clobber(&src, &dest).expect_err("must not clobber an occupied destination");
    assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "already here",
        "the occupant must survive a losing move"
    );
    assert!(src.exists(), "the source must not be consumed by a failed move");
}

// ensure_dir_owned_by =================================================================================================

/// An account whose uid is the test user's own (so `chown` needs no privilege) and whose gid is a
/// group other than the one the temp directory carries, so a `chown` is visible in the result.
fn other_group_account(tmp: &Path) -> Account {
    use std::os::unix::fs::MetadataExt as _;

    let inherited = nix::unistd::Gid::from_raw(std::fs::metadata(tmp).expect("stat tempdir").gid());
    // SAFETY: a zero-length query takes no buffer; the second call's buffer holds `n` gids.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    assert!(n >= 0, "getgroups: {}", io::Error::last_os_error());
    let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
    let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    assert!(n >= 0, "getgroups: {}", io::Error::last_os_error());
    groups.truncate(n as usize);
    let gid = groups
        .into_iter()
        .map(nix::unistd::Gid::from_raw)
        .find(|g| *g != inherited)
        .expect("the test user must belong to a group other than the temp directory's");
    Account {
        name: "test".to_string(),
        uid: nix::unistd::Uid::current(),
        gid,
    }
}

fn mode_and_gid(path: &Path) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt as _;

    let meta = std::fs::metadata(path).expect("stat");
    (meta.permissions().mode() & 0o777, meta.gid())
}

#[skuld::test]
fn ensure_dir_owned_by_leaves_an_existing_directory_exactly_as_found() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let dir = tmp.path().join("existing");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(&dir);

    ensure_dir_owned_by(&dir, &account, &mut CreatedDirs::new()).expect("an existing directory is accepted");

    assert_eq!(mode_and_gid(&dir), before);
    assert_eq!(before.0, 0o700);
}

#[skuld::test]
fn ensure_dir_owned_by_creates_and_hands_over_every_missing_component() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let leaf = tmp.path().join("a").join("b").join("c");

    ensure_dir_owned_by(&leaf, &account, &mut CreatedDirs::new()).expect("create the whole chain");

    for dir in [tmp.path().join("a"), tmp.path().join("a/b"), leaf] {
        assert_eq!(
            mode_and_gid(&dir),
            (0o755, account.gid.as_raw()),
            "{} must be 0755 and the account's",
            dir.display()
        );
    }
}

/// A component another install creates after the walk and before the `mkdir` is not ours to
/// `chmod` or `chown`; the components below it still are.
#[skuld::test]
fn create_owned_leaves_a_component_that_appeared_after_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let raced = tmp.path().join("a");
    let leaf = raced.join("b");

    let missing = missing_components(&leaf).expect("walk");
    assert_eq!(missing, [raced.as_path(), leaf.as_path()]);
    std::fs::create_dir(&raced).unwrap();
    std::fs::set_permissions(&raced, std::fs::Permissions::from_mode(0o700)).unwrap();
    let before = mode_and_gid(&raced);

    create_owned(&missing, &account, &mut CreatedDirs::new()).expect("a lost race is not an error");

    assert_eq!(
        mode_and_gid(&raced),
        before,
        "the racing install's directory is untouched"
    );
    assert_eq!(mode_and_gid(&leaf), (0o755, account.gid.as_raw()));
}

/// A dangling symlink is an existing path: `exists()` would follow it and call it missing.
#[skuld::test]
fn ensure_dir_owned_by_treats_a_dangling_symlink_as_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let target = tmp.path().join("nowhere");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    ensure_dir_owned_by(&link, &account, &mut CreatedDirs::new()).expect("an existing path of any kind is accepted");

    assert!(!target.exists(), "the link's target must not be created");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
}

#[skuld::test]
fn ensure_dir_owned_by_accepts_a_regular_file() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let file = tmp.path().join("file");
    std::fs::write(&file, b"x").unwrap();

    ensure_dir_owned_by(&file, &account, &mut CreatedDirs::new()).expect("an existing path of any kind is accepted");

    assert_eq!(std::fs::read(&file).unwrap(), b"x");
}

/// `cwd` is made before the parent of `logs` is looked at, so a failure on the latter leaves `cwd`
/// for the caller's guard to take back.
#[skuld::test]
fn prepare_parent_dirs_records_the_cwd_it_made_before_the_logs_parent_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let file = tmp.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    let cwd = tmp.path().join("cwd");
    let mut fields = plain_fields("dirs", vec!["/bin/true".to_string()]);
    fields.cwd = Some(cwd.clone());
    fields.logs = Some(file.join("sub").join("out.log"));
    let spec = DaemonSpec::new(fields).unwrap();
    let mut created = CreatedDirs::new();

    prepare_parent_dirs(&spec, &account, &mut created).expect_err("the logs parent is under a file");
    assert!(cwd.is_dir(), "the cwd was made before the failure");
    drop(created);

    assert!(!cwd.exists());
}

// plist_stands ========================================================================================================

#[skuld::test]
fn a_regular_file_holding_the_plist_stands() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("id.plist");
    std::fs::write(&path, "text").unwrap();

    assert!(plist_stands(&path, "text"));
}

#[skuld::test]
fn an_absent_plist_does_not_stand() {
    let tmp = tempfile::tempdir().unwrap();

    assert!(!plist_stands(&tmp.path().join("id.plist"), "text"));
}

#[skuld::test]
fn a_plist_holding_other_text_does_not_stand() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("id.plist");
    std::fs::write(&path, "someone else's").unwrap();

    assert!(!plist_stands(&path, "text"));
}

/// Never opened, so it cannot block.
#[skuld::test]
fn a_fifo_does_not_stand() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("id.plist");
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();

    assert!(!plist_stands(&path, "text"));
}

#[skuld::test]
fn a_dangling_symlink_does_not_stand() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("id.plist");
    std::os::unix::fs::symlink(tmp.path().join("nowhere"), &path).unwrap();

    assert!(!plist_stands(&path, "text"));
}

/// A read that does not complete says nothing about the plist; the directories stay.
#[skuld::test]
fn a_plist_that_cannot_be_read_stands() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    std::fs::write(&file, "x").unwrap();

    // `ENOTDIR` on the parent: presence is unsettled.
    assert!(plist_stands(&file.join("id.plist"), "text"));
}

// resolve_account =====================================================================================================

#[skuld::test]
fn resolve_account_root_is_uid_zero() {
    let account = resolve_account(&User::Root).expect("root always exists");
    assert_eq!(account.uid.as_raw(), 0);
    assert_eq!(account.name, "root");
}

#[skuld::test]
fn resolve_account_uid_zero_is_also_root() {
    // `User::Id(AccountId::Uid(0))` and `User::Root` must resolve to the
    // same account: both name uid 0, and `install`'s comparison between the
    // new spec's identity and the embedded spec's identity has to agree on
    // that regardless of which spelling either one used.
    let account = resolve_account(&crate::spec::User::Id(crate::spec::AccountId::Uid(0))).unwrap();
    assert_eq!(account.name, "root");
}

#[skuld::test]
fn resolve_account_sid_is_rejected_on_macos() {
    let err = resolve_account(&crate::spec::User::Id(crate::spec::AccountId::Sid(
        "S-1-5-21-0".to_string(),
    )))
    .expect_err("a Windows SID is meaningless on macOS");
    assert!(err.to_string().contains("SID"), "{err}");
}

#[skuld::test]
fn resolve_account_rejects_a_nonexistent_user() {
    let err = resolve_account(&User::Name("goetia-no-such-user-xyz".to_string())).expect_err("no such user");
    assert!(err.to_string().contains("goetia-no-such-user-xyz"), "{err}");
}

// locate ==============================================================================================================

#[skuld::test]
fn locate_reports_absent_for_an_unknown_id() {
    // Exercises the real, hardcoded `STAGING_DIR`/`ENABLED_DIR` — safe and
    // needs no elevation, since a random id that has never been installed
    // is absent from both regardless of who owns those directories.
    let id = format!("goetia-manager-unit-test-absent-{}", std::process::id());
    let location = locate(&id).expect("locate should not error for an absent id");
    assert!(location.is_none());
}

// occupied ============================================================================================================

/// A path whose parent component is a regular file: `ENOTDIR`. Not `NotFound`, not
/// `PermissionDenied`, and — the reason it is the probe used here rather than a mode-`000`
/// directory — identical for root and everyone else. CI runs every test binary under `sudo`
/// (`.github/workflows/ci.yaml`), which traverses a mode-`000` directory to a plain `NotFound`, so a
/// permission fixture in this binary would report absence and fail there for a reason that is about
/// the runner's uid rather than about the code. The permission shape this whole class exists for
/// needs a second uid and is exercised end to end by
/// `tests/launchd_integration/launchd.rs::unelevated_list_reports_a_root_only_plist_as_undetermined`.
/// `src/backend/systemd/manager/discover_tests.rs` splits its own coverage on the same line.
fn unreachable_path(tmp: &Path) -> PathBuf {
    let file = tmp.join("not-a-directory");
    fs::write(&file, "").expect("write the blocking file");
    file.join("x.plist")
}

/// The defect this type exists to remove: `symlink_metadata(path).is_ok()` answered "I could not
/// look" as "nothing is there", so `locate` returned `Ok(None)` and every verb reported
/// `NotInstalled` for a daemon that is right there.
#[skuld::test]
fn occupied_distinguishes_a_denied_stat_from_absence() {
    let tmp = tempfile::tempdir().expect("tempdir");

    match occupied(&unreachable_path(tmp.path())) {
        Presence::Undetermined { source } => assert_ne!(
            source.kind(),
            io::ErrorKind::NotFound,
            "a `NotFound` belongs in `Absent`, not here: {source}"
        ),
        other => panic!("a stat that never completed established no absence: {other:?}"),
    }
}

#[skuld::test]
fn occupied_reports_absence_and_presence_when_the_stat_completes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("x.plist");

    assert!(matches!(occupied(&path), Presence::Absent));

    fs::write(&path, "anything at all").expect("write the plist");
    assert!(matches!(occupied(&path), Presence::Present));
}

// obtain ==============================================================================================================

fn mkfifo(path: &Path) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a temp path with no NUL");
    // SAFETY: `c_path` is a NUL-terminated pointer valid for the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), io::Error::last_os_error());
}

/// Run `f` on its own thread and report — rather than hang — if it never returns.
///
/// The bound is a failure bound on a kernel wait that may genuinely never end: `open(2)` on a FIFO
/// with `O_RDONLY` blocks until a writer arrives, and these tests never create one. It synchronizes
/// nothing. A correct classification is one `stat` and a send, so no passing run's outcome depends
/// on the bound's value — only how long a regression takes to be *reported* instead of wedging the
/// whole suite, which is what an unguarded call would do.
fn without_blocking<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || drop(tx.send(f())));
    rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap_or_else(|_| {
        panic!("{what} never returned: it opened the FIFO for reading, which blocks until a writer arrives")
    })
}

/// The case that makes classifying before opening mandatory rather than tidy. `list` reads every
/// `*.plist` name in `/Library/LaunchDaemons`, so one `mkfifo` there would otherwise wedge the
/// listing for the whole host — and `install`/`status` for that id forever.
#[skuld::test]
fn a_fifo_is_non_regular_without_blocking() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fifo = tmp.path().join("x.plist");
    mkfifo(&fifo);

    let obtained = without_blocking("obtain", move || obtain(&fifo));

    assert!(
        matches!(obtained, Obtained::NonRegular),
        "a FIFO is classified, never opened for reading"
    );
}

/// The same path through the reader every verb but `list` goes down. A FIFO is not a plist goetia
/// failed to read — it is positively not a plist — so the answer is foreign, which is what empty
/// text means here, and never [`Error::Undetermined`].
#[skuld::test]
fn read_artifact_treats_a_fifo_as_foreign_without_blocking() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fifo = tmp.path().join("x.plist");
    mkfifo(&fifo);

    let text = without_blocking("read_artifact", move || {
        read_artifact(&fifo, "x").map_err(|e| e.to_string())
    })
    .expect("a FIFO is identified, not a read that did not complete");

    assert!(
        generate::extract(&text).expect("empty text decodes cleanly").is_none(),
        "nothing goetia wrote is at this path, so every caller must refuse it as foreign"
    );
}

#[skuld::test]
fn a_directory_where_a_plist_should_be_is_non_regular() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("x.plist");
    fs::create_dir(&dir).expect("seed a directory where a plist should be");

    assert!(matches!(obtain(&dir), Obtained::NonRegular));
}

#[skuld::test]
fn obtain_reads_a_regular_file_and_reports_a_missing_one_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("x.plist");

    assert!(matches!(obtain(&path), Obtained::Absent));

    fs::write(&path, b"<plist/>\n").expect("write the plist");
    match obtain(&path) {
        Obtained::Bytes(bytes) => assert_eq!(bytes, b"<plist/>\n".to_vec()),
        _ => panic!("a regular file is read, not classified away"),
    }
}

/// A symlink to a regular plist still resolves and is still read — deliberately unlike the systemd
/// backend, where `systemctl mask` makes the symlink itself the meaningful artifact. See
/// [`obtain`]'s doc comment.
#[skuld::test]
fn a_symlink_to_a_plist_is_still_followed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("real.plist");
    let link = tmp.path().join("x.plist");
    fs::write(&target, b"<plist/>\n").expect("write the plist");
    std::os::unix::fs::symlink(&target, &link).expect("plant the symlink");

    match obtain(&link) {
        Obtained::Bytes(bytes) => assert_eq!(bytes, b"<plist/>\n".to_vec()),
        _ => panic!("launchd has no masking, so a symlinked plist is just an indirection to a plist"),
    }
}

/// A dangling symlink is the one path where `metadata` (follows) and
/// `locate`'s `symlink_metadata` (does not) disagree. Left as `Absent`, the
/// two classifiers never reconcile: every verb answers "not installed" while
/// `link`(2) refuses to create over the link with `EEXIST`, so the id becomes
/// an unclearable dead end whose only message says nothing is there.
/// `NonRegular` is the honest answer — the link is positively not a plist,
/// the same presence fact a FIFO is.
#[skuld::test]
fn a_dangling_symlink_is_not_absence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join("x.plist");
    std::os::unix::fs::symlink(tmp.path().join("nothing-here.plist"), &link).expect("plant the symlink");

    assert!(
        fs::metadata(&link).is_err(),
        "the fixture must actually dangle, or this pins nothing"
    );
    assert!(matches!(obtain(&link), Obtained::NonRegular));
}

// read_artifact =======================================================================================================

#[skuld::test]
fn read_artifact_maps_a_vanished_plist_to_not_installed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("gone.plist");

    // `locate` stat'd it and the read no longer finds it: an uninstall that completed between the
    // two syscalls. Absence is the truth about the id, so this is not a failure to determine.
    let err = read_artifact(&path, "gone").expect_err("a missing artifact is not readable");
    assert!(matches!(err, Error::NotInstalled { .. }), "{err}");
}

#[skuld::test]
fn read_artifact_maps_an_unreadable_plist_to_undetermined() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = unreachable_path(tmp.path());

    let err = read_artifact(&path, "opaque").expect_err("`ENOTDIR` is not a readable artifact");

    let Error::Undetermined { id, reason, .. } = &err else {
        panic!("`Error::Os` reaches `status` as `unusable`, which claims goetia owns the id: {err:?}");
    };
    assert_eq!(id, "opaque");
    let reason = reason.report().to_string();
    assert!(
        reason.contains(&path.display().to_string()),
        "the reason must name the path that could not be read: {reason}"
    );
}

/// `ENABLED_DIR` is `/Library/LaunchDaemons` — every vendor's daemons, not goetia's — and every
/// encoding below is one a vendor legitimately ships there: `plutil -convert binary1` and `defaults
/// write` produce a binary plist by default, UTF-16 is a legal property-list encoding whose `FF FE`
/// BOM is not UTF-8, and a Latin-1 byte in a description is an ordinary accident. None of them is
/// UTF-8 XML, which is the only thing `generate::plist` writes, so each is a positive
/// identification of a file goetia did not write — foreign, exactly like an unmarked XML plist.
///
/// Answering any of them `undetermined` would give a macOS host a permanent, unclearable
/// `undetermined` entry and a permanent exit `4` for a service goetia has no business reporting on
/// at all. Pinned as one table because the whole defect was these three getting two different
/// answers.
///
/// `list` reaches the same verdict through the same [`classify`], which is what keeps the two from
/// describing one file differently; its own end is
/// `tests/launchd_integration/launchd.rs::list_stays_clean_on_a_host_carrying_binary_plists`.
#[skuld::test]
fn bytes_that_are_not_utf8_xml_are_foreign_not_undetermined() {
    let mut binary = b"bplist00".to_vec();
    binary.extend_from_slice(&[0xd1, 0x01, 0x02, 0x5f, 0x10, 0x00, 0xff]);
    let utf16 = b"\xff\xfe<\x00?\x00x\x00m\x00l\x00".to_vec();
    let latin1 = b"<!-- Caf\xe9 -->".to_vec();

    let tmp = tempfile::tempdir().expect("tempdir");
    for (label, bytes) in [
        ("a binary plist", binary),
        ("a UTF-16 XML plist", utf16),
        ("one Latin-1 byte", latin1),
    ] {
        let path = tmp.path().join(format!("{label}.plist"));
        fs::write(&path, &bytes).expect("write the vendor plist");

        assert!(
            matches!(classify(bytes), ClassifyOutcome::NotOurs),
            "{label}: not the UTF-8 XML goetia writes, so not goetia's"
        );

        let text = read_artifact(&path, "vendor")
            .unwrap_or_else(|e| panic!("{label}: the bytes were obtained, so nothing here is undetermined: {e:?}"));
        assert!(
            generate::extract(&text).expect("empty text decodes cleanly").is_none(),
            "{label}: a format goetia never emits carries no goetia marker, so every caller must \
             refuse it as foreign"
        );
    }
}

// undetermined ========================================================================================================

/// The substance `manager::fake`'s own `Error::Undetermined` carries, asserted of this backend's
/// constructor: the two describe one condition and must agree about it without sharing a function.
/// Split across the errno rather than crammed into a single sentence — elevation is advice only a
/// permission boundary earns, and offering it for a failing disk sends the reader somewhere useless
/// — so "both causes" means the pair covers both, one each. Never `uninstall`, in either: that is
/// `Outcome::RefuseUnusable`'s remedy and it certifies the ownership this read never established.
#[skuld::test]
fn the_launchd_undetermined_recovery_names_both_causes_and_not_uninstall() {
    let path = Path::new(ENABLED_DIR).join("x.plist");

    for (source, wants_elevation) in [
        (io::Error::from(io::ErrorKind::PermissionDenied), true),
        (io::Error::from(io::ErrorKind::Other), false),
    ] {
        let rendered = format!("{source:?}");
        let Error::Undetermined { id, reason, recovery } = undetermined("x", "read", &path, source) else {
            panic!("{rendered}: every non-`NotFound` failure is undetermined, not just a permission denial");
        };

        assert_eq!(id, "x");
        let reason = reason.report().to_string();
        assert!(reason.contains(&path.display().to_string()), "{rendered}: {reason}");
        assert!(recovery.contains("re-run"), "{rendered}: {recovery}");
        assert_eq!(
            recovery.contains("re-run as root"),
            wants_elevation,
            "{rendered}: {recovery}"
        );
        assert!(
            !recovery.contains("uninstall"),
            "{rendered}: uninstall certifies ownership this read never established: {recovery}"
        );
    }
}

// Enumeration: a scan that did not finish =============================================================================

/// A plist directory that will not open is reported, not propagated (see `flow::scan`): an `Err` here
/// reaches the CLI as `Kind::Unavailable` — exit `1` over an empty document, which is `list`
/// reporting a populated host as having nothing on it.
#[skuld::test]
fn a_plist_directory_that_cannot_be_opened_is_reported_not_propagated() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blocking_file = tmp.path().join("not-a-directory");
    fs::write(&blocking_file, "").expect("write the blocking file");
    // `ENOTDIR`: not `NotFound`, and identical for root and everyone else — which is what makes it
    // the right probe in a binary CI runs elevated.
    let dir = blocking_file.join("LaunchDaemons");

    let scan = scan_plists(&dir.to_string_lossy());

    assert!(scan.plists.is_empty(), "nothing was enumerated: {scan:?}");
    match &scan.incomplete {
        Some(Installed::Undetermined { name: None, reason }) => {
            let reason = reason.to_string();
            assert!(reason.contains("failed to read directory: "), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

/// The case that is deliberately *not* this one: an absent directory — the staging one does not
/// exist until the first `install` ever creates it — establishes that nothing is there. A
/// determinate answer is reported as one, on every backend; see [`crate::manager::ServiceManager::list`].
#[skuld::test]
fn an_absent_directory_is_an_empty_scan_not_a_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let scan = scan_plists(&tmp.path().join("nowhere").to_string_lossy());

    assert!(scan.plists.is_empty(), "{scan:?}");
    assert!(
        scan.incomplete.is_none(),
        "an absent directory answers the question rather than leaving it open: {scan:?}"
    );
}

#[skuld::test]
fn a_pass_that_finishes_names_every_plist_and_nothing_else() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("kept.plist"), "").expect("write the plist");
    fs::write(tmp.path().join("ignored.txt"), "").expect("write the non-plist");

    let scan = scan_plists(&tmp.path().to_string_lossy());

    assert_eq!(
        scan.plists.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["kept"],
        "{scan:?}"
    );
    assert!(
        scan.incomplete.is_none(),
        "a pass that finished has nothing to report: {scan:?}"
    );
}

/// Two directories, scanned as the real ones are: the second is the enabled one.
fn scanned(staging: &Path, enabled: &Path) -> Scan<Plist> {
    scan_in([(&staging.to_string_lossy(), false), (&enabled.to_string_lossy(), true)])
}

#[skuld::test]
fn a_marked_plist_is_scanned_as_marked_in_the_directory_it_was_found_in() {
    let (staging, enabled) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    let path = enabled.path().join("ours.plist");
    fs::write(&path, sample_plist("ours")).expect("write the plist");

    let scan = scanned(staging.path(), enabled.path());

    assert!(scan.reported.is_empty(), "{:?}", scan.reported);
    match scan.entries.as_slice() {
        [(name, ScanEntry::Read(ReadOutcome::Marked(snapshot)))] => {
            assert_eq!(name, "ours");
            assert_eq!(snapshot.id().as_str(), "ours");
            assert!(snapshot.blob().is_ok(), "the marker decodes");
            assert!(snapshot.location.enabled);
            assert_eq!(snapshot.location.path, path);
            assert_eq!(snapshot.text, sample_plist("ours"));
        }
        _ => panic!("one marked entry expected, got {} entries", scan.entries.len()),
    }
}

/// A plist that cannot be read in either directory is one entry, whose reason holds both failed
/// reads. A symlink loop fails the read for root and everyone else alike.
#[skuld::test]
fn a_plist_unreadable_in_both_directories_reports_both_failures() {
    let (staging, enabled) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    for dir in [&staging, &enabled] {
        let path = dir.path().join("looped.plist");
        std::os::unix::fs::symlink(&path, &path).expect("make the symlink loop");
    }

    let scan = scanned(staging.path(), enabled.path());

    assert!(scan.entries.is_empty());
    match scan.reported.as_slice() {
        [Installed::Undetermined { name, reason }] => {
            assert_eq!(name.as_deref(), Some("looped"));
            let reason = reason.to_string();
            for dir in [&staging, &enabled] {
                let path = dir.path().join("looped.plist");
                assert_eq!(reason.matches(&*path.to_string_lossy()).count(), 1, "{reason}");
            }
            assert!(reason.contains("; then: "), "{reason}");
        }
        other => panic!("one undetermined entry expected, got {other:?}"),
    }
}

#[skuld::test]
fn a_plist_without_the_marker_is_scanned_as_foreign() {
    let (staging, enabled) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    fs::write(staging.path().join("theirs.plist"), "<plist/>").expect("write the plist");

    let scan = scanned(staging.path(), enabled.path());

    assert!(scan.reported.is_empty(), "{:?}", scan.reported);
    assert!(
        matches!(scan.entries.as_slice(), [(name, ScanEntry::Read(ReadOutcome::Foreign { .. }))] if name == "theirs"),
        "{} entries",
        scan.entries.len()
    );
}

fn assert_reported_in_both(reported: &[Installed]) {
    let [entry @ Installed::OursUnusable { name, reason }] = reported else {
        panic!("one unusable entry expected, got {reported:?}");
    };
    assert_eq!(name, "twice");
    assert!(
        reason.cause().is_none(),
        "goetia's own words, no error under them: {reason:?}"
    );
    assert_eq!(
        entry.unusable_reason(),
        Some(format!("present in both {STAGING_DIR} and {ENABLED_DIR}"))
    );
}

/// A state none of this backend's operations produces, and one `locate` refuses for every verb: the
/// listing names it rather than choosing a directory.
#[skuld::test]
fn a_plist_in_both_directories_is_reported_not_classified() {
    let (staging, enabled) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    for dir in [staging.path(), enabled.path()] {
        fs::write(dir.join("twice.plist"), sample_plist("twice")).expect("write the plist");
    }

    let scan = scanned(staging.path(), enabled.path());

    assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
    assert_reported_in_both(&scan.reported);
}

/// `/Library/LaunchDaemons` holds every vendor's daemons: a stem it shares with a goetia plist in
/// staging does not make the vendor's plist goetia's, but the marked one is still a state to name.
#[skuld::test]
fn a_marked_plist_beside_an_unmarked_one_of_the_same_stem_is_reported() {
    let (staging, enabled) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    fs::write(staging.path().join("twice.plist"), sample_plist("twice")).expect("write the marked plist");
    fs::write(enabled.path().join("twice.plist"), "<plist/>").expect("write the unmarked plist");

    let scan = scanned(staging.path(), enabled.path());

    assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
    assert_reported_in_both(&scan.reported);
}

/// Nothing at the stem is goetia's, and `list` omits foreign entries, so neither an entry nor a
/// report is made.
#[skuld::test]
fn an_unmarked_plist_in_both_directories_is_not_reported() {
    let (staging, enabled) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    for dir in [staging.path(), enabled.path()] {
        fs::write(dir.join("theirs.plist"), "<plist/>").expect("write the plist");
    }

    let scan = scanned(staging.path(), enabled.path());

    assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
    assert!(scan.reported.is_empty(), "{:?}", scan.reported);
}

// Writing: a race that has cost no call ===============================================================================

/// A plist that vanished between `read` and `write` — a concurrent `uninstall` — before `install`
/// made any `launchctl` call is `Raced`, so the flow reads again. The rendering is unchanged (only
/// the version differs), so no `is_loaded` was asked, and the empty table fails the test if any
/// `launchctl` call is made: a second pass stays inside the reservation. The counterpart, a vanish
/// after that call, is `a_plist_removed_after_a_read_fails_the_install_without_further_launchctl_calls`.
#[skuld::test]
fn a_write_over_a_plist_that_vanished_before_any_call_is_raced() {
    use crate::backend::Backend as _;

    let (mgr, script) = scripted(&[]);
    let spec = spec_named("goetia-raced-before-any-call");
    let desired = mgr.render(&spec).expect("render");
    let on_disk = desired.replace(&format!("Version: {}", crate::version()), "Version: 0.0.1");
    assert_ne!(on_disk, desired);
    let tmp = tempfile::tempdir().expect("tempdir");
    let snapshot = Plist {
        location: Location {
            path: tmp.path().join("gone.plist"),
            enabled: false,
        },
        text: on_disk,
        ..marked_plist(&spec.id)
    };
    let stale = Outcome::Stale {
        from_version: "0.0.1".to_string(),
    };
    mgr.begin_install(&spec).expect("begin_install");
    let mut reservation = reserved(&[Step::Install]);

    let written = mgr
        .write(&spec, &desired, &stale, Some(&snapshot), &mut (), &mut reservation)
        .expect("write");

    assert!(matches!(written, WriteOutcome::Raced));
    script.assert_spent("a race before any call makes none");
}

// Reservation: a verb never fails halfway for want of a reaper or a file ==============================================

/// An id nothing is installed at.
fn never_installed() -> Id {
    Id::try_from("goetia-reaper-probe-never-installed").unwrap()
}

/// A marked plist for `id` that no file backs, as the ownership gate would hand a verb. The verbs
/// run over a fake `launchctl` (`scripted`), so these tests touch nothing on the host.
fn marked_plist(id: &Id) -> Plist {
    marked_plist_restarting(id, Restart::Never)
}

/// [`marked_plist`] of a job with `restart`: `Always` is the one `start`'s corrective cycle runs for.
fn marked_plist_restarting(id: &Id, restart: Restart) -> Plist {
    let text = sample_plist_restarting(id.as_str(), restart);
    let blob = generate::extract(&text)
        .transpose()
        .expect("sample_plist carries the marker");
    Plist {
        id: id.clone(),
        blob: blob.map_err(Arc::new),
        location: Location {
            path: PathBuf::from("/nonexistent/goetia-marked.plist"),
            enabled: false,
        },
        text,
    }
}

/// A reservation of the stated size: `requests` reapers and the files of `calls` calls.
fn reservation(requests: usize, calls: usize) -> Reservation {
    Reservation::make(requests, calls * Role::COUNTED_FILES).expect("make a reservation")
}

/// What [`Launchd::reserve`] makes for `steps`.
fn reserved(steps: &[Step]) -> Reservation {
    backend::Backend::reserve(&Launchd::new(), steps, Budget::DEFAULT).expect("reserve")
}

/// `reserve` makes what the steps count, and nothing else: the reapers and files of their longest
/// paths, summed over a sequence. The numbers are written out so that changing a count is deliberate.
#[skuld::test]
fn a_reservation_holds_what_its_steps_count() {
    let cases: [(&[Step], (usize, usize)); 7] = [
        (&[Step::Install], (1, 6)),
        (&[Step::Start], (5, 24)),
        (&[Step::Stop], (1, 2)),
        (&[Step::Kill], (1, 2)),
        (&[Step::Uninstall], (1, 2)),
        (&[Step::RequestStartAfterStop], (5, 24)),
        (&[Step::Stop, Step::Start], (6, 26)),
    ];
    for (steps, held) in cases {
        assert_eq!(bounded::test_hook::left(&reserved(steps)), held, "{steps:?}");
    }
}

/// A reservation one reaper or one file short of what a sequence counts is refused as a whole, so a
/// `restart` (`[Stop, Start]`) or an `install --start` (`[Start]`) that cannot have everything sends
/// nothing — under every budget, since `launchctl` is waited on under all of them.
#[skuld::test]
fn a_reservation_that_cannot_be_made_whole_is_refused() {
    use crate::backend::Backend;

    type Allowance = fn(usize) -> bounded::test_hook::Allowance;
    let mgr = Launchd::new();
    for steps in [&[Step::Stop, Step::Start][..], &[Step::Start]] {
        let (reapers, files) = counted(steps);
        for budget in [Budget::DEFAULT, Budget::Immediate, Budget::Unbounded] {
            let shortfalls: [(Allowance, usize, &str); 2] = [
                (bounded::test_hook::threads, reapers - 1, "no thread"),
                (bounded::test_hook::temp_files, files - 1, "no temp file"),
            ];
            for (allow, n, what) in shortfalls {
                let _short = allow(n);

                let e = mgr.reserve(steps, budget).expect_err("one short");

                assert!(
                    matches!(&e, Error::Reservation { .. }) && e.report().to_string().contains(what),
                    "{steps:?} {budget:?}: {e:?}"
                );
            }
        }
    }
}

/// One verb over a table of stated replies, and what it draws.
struct Drawing {
    /// The step it is counted under.
    step: Step,
    restart: Restart,
    /// Its longest path, call for call.
    calls: &'static [(&'static str, Answer)],
    /// The requests and calls that path makes: what `needs` counts when it is `step`'s longest.
    drew: (usize, usize),
    run: fn(&Launchd, Plist, &mut Reservation) -> Result<()>,
}

/// Every verb that draws, down its longest path.
fn drawings() -> [Drawing; 5] {
    use crate::backend::Backend;

    [
        Drawing {
            step: Step::Start,
            restart: Restart::Always,
            calls: &START_LONGEST,
            drew: (START_REQUESTS, START_CALLS),
            run: |mgr, plist, reservation| Backend::start(mgr, plist, Budget::DEFAULT, reservation),
        },
        // `start` under a budget that does not wait: counted as `start` is, and so over-reserved.
        Drawing {
            step: Step::RequestStartAfterStop,
            restart: Restart::Always,
            calls: &START_NOT_WAITING,
            drew: (2, 5),
            run: |mgr, plist, reservation| Backend::request_start_after_stop(mgr, plist, reservation),
        },
        Drawing {
            step: Step::Stop,
            restart: Restart::Never,
            calls: &BOOTOUT,
            drew: (STOP_REQUESTS, STOP_CALLS),
            run: |mgr, plist, reservation| Backend::stop(mgr, plist, Budget::DEFAULT, reservation),
        },
        Drawing {
            step: Step::Kill,
            restart: Restart::Never,
            calls: &[("kill", DONE)],
            drew: (KILL_REQUESTS, KILL_CALLS),
            run: |mgr, plist, reservation| Backend::kill(mgr, plist, reservation),
        },
        // The `bootout`, and then the removal of a plist nothing backs, which fails.
        Drawing {
            step: Step::Uninstall,
            restart: Restart::Never,
            calls: &BOOTOUT,
            drew: (STOP_REQUESTS, STOP_CALLS),
            run: |mgr, plist, reservation| match Backend::uninstall(mgr, plist, reservation) {
                Err(Error::Os { what, source, .. })
                    if what == "remove"
                        && source
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    Ok(())
                }
                other => other,
            },
        },
    ]
}

/// The fake `launchctl` draws as the real one does, so every verb run over it checks its step's count:
/// a verb that draws more than was counted fails the debug assertion, and one that draws less than the
/// longest path leaves exactly the difference. The longest paths themselves are the elevated test's, over
/// a real child.
#[skuld::test]
fn every_verb_draws_what_its_step_counts() {
    let id = never_installed();
    for drawing in drawings() {
        let (mgr, script) = scripted(drawing.calls);
        let mut held = reserved(&[drawing.step]);

        let result = (drawing.run)(&mgr, marked_plist_restarting(&id, drawing.restart), &mut held);

        result.unwrap_or_else(|e| panic!("{:?}: {e:?}", drawing.step));
        script.assert_spent(&format!("{:?}'s longest path", drawing.step));
        let (counted_requests, counted_calls) = needs(drawing.step);
        assert_eq!(
            bounded::test_hook::left(&held),
            (
                counted_requests - drawing.drew.0,
                (counted_calls - drawing.drew.1) * Role::COUNTED_FILES
            ),
            "{:?}",
            drawing.step
        );
    }
}

/// A verb handed one reaper or one file fewer than its step counts fails at the draw, in every
/// build that has debug assertions: an undercount cannot pass an ordinary test.
#[skuld::test]
fn a_verb_handed_one_item_too_few_trips_the_assertion() {
    let id = never_installed();
    for drawing in drawings() {
        let (requests, calls) = drawing.drew;
        for (what, (reapers, files)) in [
            ("reaper", (requests - 1, calls * Role::COUNTED_FILES)),
            ("temp file", (requests, calls * Role::COUNTED_FILES - 1)),
        ] {
            let (mgr, _script) = scripted(drawing.calls);
            let mut short = Reservation::make(reapers, files).expect("make a reservation");

            let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drop((drawing.run)(
                    &mgr,
                    marked_plist_restarting(&id, drawing.restart),
                    &mut short,
                ));
            }));

            let payload = drawn.expect_err(&format!("{:?}: a {what} short went unnoticed", drawing.step));
            let message = payload.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(
                message.contains(&format!("a {what} was drawn past the reservation")),
                "{:?}: {message}",
                drawing.step
            );
        }
    }
}

/// `restart` reserves both legs before the stop goes out, and when it cannot, nothing is sent:
/// exit `1`, no `launchctl` called, so a daemon is never left stopped for want of a thread or a
/// file its start needed. One reaper and one file short each fail it.
#[skuld::test]
fn a_restart_that_cannot_reserve_sends_nothing() {
    use crate::cli::wait::WaitArgs;
    use crate::cli::{ExitCode, restart};

    type Allowance = fn(usize) -> bounded::test_hook::Allowance;
    let (reapers, files) = counted(&[Step::Stop, Step::Start]);
    let shortfalls: [(Allowance, usize); 2] = [
        (bounded::test_hook::threads, reapers - 1),
        (bounded::test_hook::temp_files, files - 1),
    ];
    for (allow, n) in shortfalls {
        let script = table(&[]);
        let args = restart::Args {
            ids: vec![never_installed().to_string()],
            wait: WaitArgs::default(),
        };
        let manager = {
            let script = script.clone();
            move || -> Result<Box<dyn ServiceManager>> { Ok(Box::new(launchd_over(script.clone()))) }
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let _short = allow(n);

        let code = restart::run(&args, &manager, &|| true, &mut out, &mut err);

        let err = String::from_utf8(err).unwrap();
        assert_eq!(code, ExitCode::Error, "{err}");
        assert!(err.contains("so none was sent"), "{err}");
        assert_eq!(script.calls(), Vec::<Vec<String>>::new(), "nothing may be sent");
    }
}

/// An `install` that cannot reserve sends nothing, and fails as the install of its id. It is the one
/// single verb reachable here: the others pass the ownership gate first, which reads the host's real
/// `/Library` paths. What each single verb reserves for, and how each refuses, is `flow_tests`' over
/// the test `Fake`, since the blanket impl is the same for every backend.
#[skuld::test]
fn an_install_that_cannot_reserve_sends_nothing_and_names_its_id() {
    type Allowance = fn(usize) -> bounded::test_hook::Allowance;
    let (reapers, files) = counted(&[Step::Install]);
    let shortfalls: [(Allowance, usize); 2] = [
        (bounded::test_hook::threads, reapers - 1),
        (bounded::test_hook::temp_files, files - 1),
    ];
    for (allow, n) in shortfalls {
        let (mgr, script) = scripted(&[]);
        let spec = spec_named("goetia-reservation-refused");
        let _short = allow(n);

        let result = ServiceManager::install(&mgr, &spec, false);

        assert!(
            matches!(&result, Err(Error::Reservation { id: Some(id), .. }) if id == "goetia-reservation-refused"),
            "{result:?}"
        );
        assert_eq!(script.calls(), Vec::<Vec<String>>::new(), "nothing may be sent");
    }
}

// A request in doubt ==================================================================================================

/// A `launchctl` request that may have started before goetia lost it — cosca failing past `exec`,
/// or the wait failing — may have reached launchd: [`Error::RequestInDoubt`], exit `4`, and never
/// known to have, since `launchctl` never says so. A read changes nothing, so the same failure there
/// is a plain one. A failure placed before the spawn is a plain one for both.
#[skuld::test]
fn a_launchctl_request_that_may_have_run_is_in_doubt() {
    let args = ["bootout", "system/goetia-reaper-probe-never-installed"];
    let deadline = Budget::Unbounded.start();
    fn forced() -> cosca::error::Error {
        cosca::error::Error::Containment {
            detail: "forced".into(),
        }
    }
    let failing = |error: ClientError| Launchctl::new(Box::new(Fake::new([fail(&["bootout"], error)])));
    let after_exec: [fn() -> ClientError; 2] = [
        || ClientError::MayHaveRun(forced()),
        || ClientError::Lost {
            error: forced(),
            announced: false,
        },
    ];
    for error in after_exec {
        let mut held = reservation(1, 1);
        let role = request(&mut held).unwrap();
        let e = failing(error())
            .run(&args, deadline, role, &mut held)
            .expect_err("failed");
        assert!(matches!(e, Error::RequestInDoubt { reached: false, .. }), "{e:?}");

        let e = failing(error())
            .run(&args, deadline, Role::Query, &mut reservation(0, 1))
            .expect_err("failed");
        assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
        assert!(!e.is_indeterminate(), "{e:?}");
    }
    let mut held = reservation(1, 1);
    let role = request(&mut held).unwrap();
    let e = failing(ClientError::NotRun(forced()))
        .run(&args, deadline, role, &mut held)
        .expect_err("failed");
    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
    assert!(!e.is_indeterminate(), "{e:?}");
}

/// A `launchctl` that cannot be run says nothing about the job: `status` fails, and `list` (via
/// `flow`) reports the entry unusable, rather than either showing a plausible `Unknown` state.
#[skuld::test]
fn a_live_status_whose_launchctl_cannot_be_run_is_an_error() {
    use crate::backend::Backend as _;

    let unrunnable = ClientError::NotRun(io::Error::from_raw_os_error(libc::EAGAIN).into());
    let mgr = launchd_over(Fake::new([fail(&["print"], unrunnable)]));
    let snapshot = Plist {
        location: Location {
            path: PathBuf::from("/nonexistent/goetia-live-status.plist"),
            enabled: false,
        },
        text: String::new(),
        ..marked_plist(&never_installed())
    };

    let result = mgr.live_status(&snapshot);

    assert!(matches!(result, Err(Error::Spawn { .. })), "{result:?}");
}

// Every verb path stays inside its reservation ========================================================================

/// What one `launchctl` call answers with: its exit code, stdout and stderr.
type Answer = (i32, &'static str, &'static str);

const NOT_LOADED: Answer = (113, "", "");
const DONE: Answer = (0, "", "");
const REFUSED: Answer = (5, "", "Bootstrap failed: 5: Input/output error\n");
/// A `print` that failed for a reason other than "no such job".
const UNREADABLE: Answer = (5, "", "Could not print: 5: Input/output error\n");
const RUNNING: Answer = (0, "\tstate = running\n\tpid = 4242\n", "");
const NOT_RUNNING: Answer = (0, "\tstate = not running\n", "");
const PID: Answer = (0, "4242\n", "");
/// A `print` that `state::classify` reads as `Unknown`: launchd has decided to start the job and not
/// yet forked it.
const SPAWN_SCHEDULED: Answer = (0, "\tstate = spawn scheduled\n", "");
/// A `print` whose `state` no recognised value names.
const UNRECOGNISED: Answer = (0, "\tstate = someday-new-apple-state\n", "");

/// `start`'s longest path, every one of its [`START_CALLS`] `launchctl` calls: not loaded; a
/// `bootstrap` refused, by a concurrent start that loaded it meanwhile; a `kickstart -p` that names
/// no pid, so a bounded read decides; and then, `restart: always` reading not running, the
/// corrective `bootout`, `bootstrap` and `kickstart`, and the read that confirms it.
const START_LONGEST: [(&str, Answer); START_CALLS] = [
    ("print", NOT_LOADED),
    ("bootstrap", REFUSED),
    ("print", DONE),
    ("print", NOT_RUNNING),
    ("kickstart", DONE),
    ("print", RUNNING),
    ("print", NOT_RUNNING),
    ("bootout", DONE),
    ("bootstrap", DONE),
    ("print", NOT_RUNNING),
    ("kickstart", PID),
    ("print", RUNNING),
];

/// `request_start_after_stop`'s longest path: `start` under a budget that does not wait, which
/// neither reads the pid nor takes the corrective cycle.
const START_NOT_WAITING: [(&str, Answer); 5] = [
    ("print", NOT_LOADED),
    ("bootstrap", REFUSED),
    ("print", DONE),
    ("print", NOT_RUNNING),
    ("kickstart", DONE),
];

/// `install`'s longest path, [`INSTALL_CALLS`] calls: a `Create`'s read, the `bootout` of the stale job
/// it finds loaded, and the read after the write. An `Update` makes two reads, before the write and
/// after it.
const INSTALL_LONGEST: [(&str, Answer); INSTALL_CALLS] = [("print", DONE), ("bootout", DONE), ("print", NOT_LOADED)];

/// A `Create` over no loaded job: the read before the write and the read after it.
const CREATED: [(&str, Answer); 2] = [("print", NOT_LOADED), ("print", NOT_LOADED)];

/// `stop`'s and `uninstall`'s one call.
const BOOTOUT: [(&str, Answer); STOP_CALLS] = [("bootout", DONE)];

/// One scripted call: `verb` answers `answer`.
fn answer(verb: &str, (code, stdout, stderr): Answer) -> Entry {
    reply(&[verb], exited(code, stdout, stderr))
}

/// A fake `launchctl` that answers the calls it gets, in order, from `calls`. A call with another
/// verb, or past the last, fails the test, and so does a call left unmade at [`Fake::assert_spent`].
fn table(calls: &[(&str, Answer)]) -> Fake {
    Fake::new(calls.iter().map(|&(verb, a)| answer(verb, a)))
}

/// A launchd whose `launchctl` is `client`.
fn launchd_over(client: impl Client + 'static) -> Launchd {
    Launchd {
        launchctl: Launchctl::new(Box::new(client)),
    }
}

/// A launchd over [`table`]`(calls)`, and the table.
fn scripted(calls: &[(&str, Answer)]) -> (Launchd, Fake) {
    let fake = table(calls);
    (launchd_over(fake.clone()), fake)
}

/// Every call list, concatenated: one table for a prepared sequence.
fn then<'a>(steps: &[&[(&'a str, Answer)]]) -> Vec<(&'a str, Answer)> {
    steps.iter().flat_map(|step| step.iter().copied()).collect()
}

/// A launchd whose `launchctl` is a real `sh` child that answers the calls it gets, in order, from
/// `calls`, and fails any call past the last. For the tests that drive the real spawn, which draws
/// from the reservation as the [`Fake`] does. The script is never a file on disk: `exec`ing one just
/// written races every `fork` another test thread makes.
fn over_sh(calls: &[(&'static str, Answer)]) -> (Launchd, OverSh) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("n"), "0").unwrap();
    let quoted = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let arms: String = calls
        .iter()
        .enumerate()
        .map(|(i, (_, (code, stdout, stderr)))| {
            format!(
                "{}) printf %s {} ; printf %s {} >&2; exit {code};; ",
                i + 1,
                quoted(stdout),
                quoted(stderr)
            )
        })
        .collect();
    let script = format!(
        "d='{}'; n=$(( $(cat \"$d/n\") + 1 )); echo \"$n\" > \"$d/n\"; echo \"$*\" >> \"$d/log\"; \
         case \"$n\" in {arms}*) echo \"unscripted launchctl call $n: $*\" >&2; exit 99;; esac",
        dir.path().display()
    );
    let mgr = launchd_over(Real::new(
        ["/bin/sh", "-c", script.as_str(), "launchctl"]
            .map(String::from)
            .to_vec(),
        |_| {},
    ));
    let expected = calls.iter().map(|(verb, _)| *verb).collect();
    (mgr, OverSh { dir, expected })
}

/// What [`over_sh`] logged.
struct OverSh {
    dir: tempfile::TempDir,
    expected: Vec<&'static str>,
}

impl OverSh {
    /// Every call was made, in the order scripted: the path was driven end to end.
    fn assert_driven(&self, what: &str) {
        let log = std::fs::read_to_string(self.dir.path().join("log")).unwrap_or_default();
        let made: Vec<&str> = log
            .lines()
            .map(|line| line.split_whitespace().next().unwrap_or_default())
            .collect();
        assert_eq!(made, self.expected, "{what}: the calls made");
    }
}

/// Removes the plists a test's install wrote, if a failure left them. Register it before the install.
/// `new` first removes what a killed run left, which would fail the next install.
struct Plists {
    _staging: FileGuard,
    _enabled: FileGuard,
}

impl Plists {
    fn new(name: &str) -> Self {
        let (staging, enabled) = (staging_path(name), enabled_path(name));
        remove_or_warn(&staging, false);
        remove_or_warn(&enabled, false);
        Self {
            _staging: FileGuard::new(staging),
            _enabled: FileGuard::new(enabled),
        }
    }
}

/// Every launchd verb, down its longest path, draws no more than the reservation it made at entry —
/// or that a sequence made: a draw past it fails a debug assertion, so a count one short fails here.
/// Under a sequence, what it reserved is spent exactly, so a count one over fails too. A real `sh`
/// child answers every call in place of `launchctl`, so the real spawn draws too, and the longest
/// paths — `start`'s corrective cycle among them — are driven deterministically, and no job is ever
/// loaded.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn every_launchd_verb_path_stays_inside_its_reservation() {
    let name = format!("goetia-reservation-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let id = Id::try_from(name.as_str()).unwrap();
    let spec = DaemonSpec::new(DaemonFields {
        restart: Restart::Always,
        kind: Kind::Simple,
        ..plain_fields(&name, vec!["/bin/sleep".to_string(), "300".to_string()])
    })
    .unwrap();
    let exhausted = |reservation: &Reservation| {
        assert_eq!(
            bounded::test_hook::left(reservation),
            (0, 0),
            "the sequence reserved more than its steps spent"
        );
    };

    // Each verb alone, under its own reservation.
    let (mgr, script) = over_sh(&INSTALL_LONGEST);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("install, created over a loaded job");

    let (mgr, script) = over_sh(&START_LONGEST);
    mgr.start(&id, Budget::DEFAULT).expect("start");
    script.assert_driven("start, longest path");

    let (mgr, script) = over_sh(&START_NOT_WAITING);
    mgr.request_start_after_stop(&id).expect("request_start_after_stop");
    script.assert_driven("request_start_after_stop, longest path");

    let (mgr, script) = over_sh(&BOOTOUT);
    mgr.stop(&id, Budget::DEFAULT).expect("stop");
    script.assert_driven("stop");

    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    let (mgr, script) = over_sh(&[("print", NOT_LOADED), ("print", NOT_LOADED)]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_driven("install, updating a job that is not loaded");

    // `restart`: the stop and the start under one reservation, spent exactly.
    let (mgr, script) = over_sh(&then(&[&BOOTOUT, &START_LONGEST]));
    let mut held = backend::Backend::reserve(&mgr, &[Step::Stop, Step::Start], Budget::DEFAULT).unwrap();
    backend::Backend::stop(&mgr, flow::owned(&mgr, &id).unwrap(), Budget::DEFAULT, &mut held).expect("restart's stop");
    backend::Backend::start(&mgr, flow::owned(&mgr, &id).unwrap(), Budget::DEFAULT, &mut held)
        .expect("restart's start");
    exhausted(&held);
    script.assert_driven("restart");

    // `install --start`: the start's reservation is made first, and the install reserves for itself;
    // each is spent exactly. The install is a `Create` over a stale loaded job, the one install path
    // that sends a request.
    std::fs::remove_file(staging_path(&name)).expect("remove the plist so the install creates it");
    let (mgr, script) = over_sh(&then(&[&INSTALL_LONGEST, &START_LONGEST]));
    let mut held = backend::Backend::reserve(&mgr, &[Step::Start], Budget::DEFAULT).unwrap();
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    backend::Backend::start(&mgr, flow::owned(&mgr, &id).unwrap(), Budget::DEFAULT, &mut held)
        .expect("install --start's start");
    exhausted(&held);
    script.assert_driven("install --start");

    // `status` counts nothing and is handed no reservation: it makes its own files, over a real child
    // too, and trips no assertion.
    let (mgr, script) = over_sh(&[("print", RUNNING)]);
    assert_eq!(mgr.status(&id).expect("status").state, State::Running);
    script.assert_driven("status");

    let (mgr, script) = over_sh(&BOOTOUT);
    mgr.uninstall(&id).expect("uninstall");
    script.assert_driven("uninstall");
    assert!(!staging_path(&name).exists(), "uninstall left the plist");
}

// roles and expiry ====================================================================================================

/// A [`Launchctl`] over a fake that answers its one call, to `verb`, with `finished`, and the fake.
fn launchctl_replying(verb: &str, finished: Finished) -> (Launchctl, Fake) {
    let fake = Fake::new([reply(&[verb], finished)]);
    (Launchctl::new(Box::new(fake.clone())), fake)
}

/// A call that changes the job goes out as a request, and one that only reads goes out as a query:
/// the role is what decides whether an expiry leaves the child running or kills it. `status` and
/// `list` read as an uncounted query. (`kill_job`'s role is pinned with its argv, below.)
#[skuld::test]
fn launchctl_sends_requests_as_requests_and_reads_as_queries() {
    let id = never_installed();
    let id = id.as_str();
    let deadline = Budget::DEFAULT.start();
    let path = Path::new("/nonexistent/goetia-roles.plist");
    let mut held = reservation(3, 5);
    let done = |verb| launchctl_replying(verb, exited(0, "", ""));
    let waited = |role| [Waited { role, bounded: true }];

    let (launchctl, fake) = done("bootstrap");
    launchctl.bootstrap(path, deadline, &mut held).unwrap();
    assert_eq!(fake.waits(), waited(RoleKind::Request));

    let (launchctl, fake) = done("bootout");
    launchctl.bootout(id, deadline, &mut held).unwrap();
    assert_eq!(fake.waits(), waited(RoleKind::Request));

    let (launchctl, fake) = done("kickstart");
    launchctl
        .kickstart(id, Budget::DEFAULT, deadline, Evidence::QueuedOrRunning, &mut held)
        .unwrap();
    assert_eq!(fake.waits(), waited(RoleKind::Request));

    let (launchctl, fake) = done("print");
    launchctl.is_loaded(id, deadline, &mut held).unwrap();
    assert_eq!(fake.waits(), waited(RoleKind::Query));

    let (launchctl, fake) = done("print");
    launchctl.read_live_state(id, deadline, Role::Query, &mut held).unwrap();
    assert_eq!(fake.waits(), waited(RoleKind::Query));

    let (launchctl, fake) = done("print");
    launchctl
        .read_live_state(id, deadline, Role::UncountedQuery, &mut held)
        .unwrap();
    assert_eq!(fake.waits(), waited(RoleKind::UncountedQuery));
    assert_eq!(
        bounded::test_hook::left(&held),
        (0, 0),
        "three requests and five counted calls, and the uncounted read drew nothing"
    );
}

/// A deadline that expired before `launchctl` finished is neither a success nor a failure of the
/// call: each call says so as `Ok(None)`, and a read of live state as `Unknown`, which the verb
/// above it turns into a wait timeout or a state it does not know. A read expires as
/// [`Finished::Expired`], a request as [`Finished::Left`]; an expired `kickstart` is followed by
/// the one `print` that decides whether it is ended (see `end_if_held`).
#[skuld::test]
fn launchctl_calls_that_expire_report_the_expiry() {
    let id = never_installed();
    let id = id.as_str();
    let deadline = Budget::DEFAULT.start();
    let path = Path::new("/nonexistent/goetia-expiry.plist");
    let mut held = reservation(4, 7);
    let expired = |verb| launchctl_replying(verb, Finished::Expired).0;
    let left = |verb| launchctl_replying(verb, Finished::Left(Left::new(|_| {}))).0;

    let loaded = expired("print").is_loaded(id, deadline, &mut held);
    assert!(loaded.expect("an expiry is not a failure").is_none());
    let bootstrapped = left("bootstrap").bootstrap(path, deadline, &mut held);
    assert!(bootstrapped.expect("an expiry is not a failure").is_none());
    let fake = Fake::new([
        reply(&["kickstart"], Finished::Left(Left::new(|_| {}))),
        answer("print", NOT_RUNNING),
    ]);
    let started = Launchctl::new(Box::new(fake.clone())).kickstart(
        id,
        Budget::DEFAULT,
        deadline,
        Evidence::QueuedOrRunning,
        &mut held,
    );
    assert!(started.expect("an expiry is not a failure").is_none());
    fake.assert_spent("an expired kickstart is followed by one print");
    let booted_out = left("bootout").bootout(id, deadline, &mut held);
    assert!(booted_out.expect("an expiry is not a failure").is_none());
    let killed = left("kill").kill_job(id, deadline, &mut held);
    assert!(killed.expect("an expiry is not a failure").is_none());

    for role in [Role::Query, Role::UncountedQuery] {
        let live = expired("print").read_live_state(id, deadline, role, &mut held);
        assert!(
            matches!(live, Ok((State::Unknown, None))),
            "a state not fully read is not known: {live:?}"
        );
    }
}

/// `stop` over a `bootout` that outlasts a bounded budget is a wait timeout, not a success: the
/// request is out, and goetia says it stopped waiting.
#[skuld::test]
fn a_stop_whose_bootout_expires_is_a_wait_timeout() {
    use crate::backend::Backend;

    let id = never_installed();
    let fake = Fake::new([reply(&["bootout"], Finished::Left(Left::new(|_| {})))]);
    let mgr = launchd_over(fake.clone());

    let e = Backend::stop(&mgr, marked_plist(&id), Budget::DEFAULT, &mut reserved(&[Step::Stop])).expect_err("expired");

    assert!(
        matches!(&e, Error::WaitTimeout { id: named, awaited: "stopped", .. } if named == id.as_str()),
        "{e:?}"
    );
    fake.assert_spent("the bootout was sent");
}

/// A waited `start` of a loaded job that reads `NOT_RUNNING`, whose `kickstart` outlasts the budget
/// and is left running, then `print` answered by `after`.
fn start_whose_kickstart_expires(after: Entry) -> (Error, Ended) {
    let (e, ended, _fake) = start_whose_kickstart_expires_over(NOT_RUNNING, after);
    (e, ended)
}

/// [`start_whose_kickstart_expires`] after a read of the job that answered `before`.
fn start_whose_kickstart_expires_over(before: Answer, after: Entry) -> (Error, Ended, Fake) {
    let id = never_installed();
    let (kickstart, ended) = lingering(&["kickstart"]);
    let fake = Fake::new([answer("print", DONE), answer("print", before), kickstart, after]);
    let mgr = launchd_over(fake.clone());

    let e = backend::Backend::start(&mgr, marked_plist(&id), Budget::DEFAULT, &mut reserved(&[Step::Start]))
        .expect_err("a kickstart that outlasts the budget");

    fake.assert_spent("the start was driven to the expiry and the read after it");
    (e, ended, fake)
}

/// The `print` after an expired `kickstart` is a counted read under a deadline of its own: it is
/// never left running (a query is killed on expiry) and never waited on without end.
fn assert_ends_with_a_bounded_query(fake: &Fake) {
    assert_eq!(
        fake.waits().last(),
        Some(&Waited {
            role: RoleKind::Query,
            bounded: true
        })
    );
}

fn assert_wait_timeout(e: &Error) {
    assert!(matches!(e, Error::WaitTimeout { awaited: "running", .. }), "{e:?}");
}

/// A `kickstart` the budget ran out on is ended once a `print` shows the start queued or the job running.
#[skuld::test]
fn a_kickstart_that_outlasts_the_budget_is_ended_once_launchd_holds_the_start() {
    for held in [SPAWN_SCHEDULED, RUNNING] {
        let (e, ended) = start_whose_kickstart_expires(answer("print", held));

        assert_wait_timeout(&e);
        assert_eq!(ended.fate(), Some(Fate::Kill), "{held:?}");
    }
}

/// After a read that was not a clean `Stopped`, only a queued start ends the `kickstart`.
#[skuld::test]
fn a_kickstart_after_an_unknown_read_is_ended_only_by_a_queued_start() {
    for before in [UNREADABLE, SPAWN_SCHEDULED, UNRECOGNISED] {
        for (shown, fate) in [(RUNNING, Fate::Release), (SPAWN_SCHEDULED, Fate::Kill)] {
            let (e, ended, _fake) = start_whose_kickstart_expires_over(before, answer("print", shown));

            assert_wait_timeout(&e);
            assert_eq!(ended.fate(), Some(fate), "{before:?} then {shown:?}");
        }
    }
}

/// The read that decides is a bounded query, so a `launchctl` that never answers cannot hold a
/// `start --timeout` past its timeout without end.
#[skuld::test]
fn the_print_after_an_expired_kickstart_is_a_bounded_query() {
    let (_e, _ended, fake) = start_whose_kickstart_expires_over(NOT_RUNNING, answer("print", SPAWN_SCHEDULED));

    assert_ends_with_a_bounded_query(&fake);
}

/// A `print` that outlasts its bound is cut short as an expired query, which is doubt: the same
/// timeout is reported and the `kickstart` is left running.
#[skuld::test]
fn a_kickstart_that_outlasts_the_budget_is_left_running_when_the_print_expires() {
    let (e, ended) = start_whose_kickstart_expires(reply(&["print"], Finished::Expired));

    assert_wait_timeout(&e);
    assert_eq!(ended.fate(), Some(Fate::Release));
}

/// `start` of a `restart: always` job down to the corrective cycle's `kickstart`, which expires:
/// [`START_LONGEST`]'s path with its last two calls replaced, the `print` taking the place of the
/// verb's last read, so the worst path stays within its reservation.
fn corrective_start_whose_kickstart_expires(after: Answer) -> (Error, Ended, Fake) {
    let id = never_installed();
    let (kickstart, ended) = lingering(&["kickstart"]);
    let head = &START_LONGEST[..START_CALLS - 2];
    let fake = Fake::new(
        head.iter()
            .map(|&(verb, a)| answer(verb, a))
            .chain([kickstart, answer("print", after)]),
    );
    let mgr = launchd_over(fake.clone());

    let e = backend::Backend::start(
        &mgr,
        marked_plist_restarting(&id, Restart::Always),
        Budget::DEFAULT,
        &mut reserved(&[Step::Start]),
    )
    .expect_err("a corrective kickstart that outlasts the budget");

    fake.assert_spent("the start was driven through its longest path to the expiry");
    (e, ended, fake)
}

/// After the corrective cycle's `bootout`, only a queued start ends the `kickstart`.
#[skuld::test]
fn a_corrective_kickstart_is_ended_only_by_a_queued_start() {
    for (shown, fate) in [(SPAWN_SCHEDULED, Fate::Kill), (RUNNING, Fate::Release)] {
        let (e, ended, fake) = corrective_start_whose_kickstart_expires(shown);

        assert_wait_timeout(&e);
        assert_eq!(ended.fate(), Some(fate), "{shown:?}");
        assert_ends_with_a_bounded_query(&fake);
    }
}

/// A `print` that does not show the start held leaves the `kickstart` running: killed before launchd
/// has the request, the start is lost, so only evidence ends it.
#[skuld::test]
fn a_kickstart_that_outlasts_the_budget_is_left_running_when_the_start_is_not_shown_held() {
    for shown in [NOT_RUNNING, UNRECOGNISED] {
        let (e, ended) = start_whose_kickstart_expires(answer("print", shown));

        assert_wait_timeout(&e);
        assert_eq!(ended.fate(), Some(Fate::Release), "{shown:?}");
    }
}

/// A `print` that failed, or whose output is not whole, is doubt, and doubt never kills: the same
/// timeout is reported and the `kickstart` is left running.
#[skuld::test]
fn a_kickstart_that_outlasts_the_budget_is_left_running_when_the_print_fails() {
    let cut = Finished::Exited {
        status: std::os::unix::process::ExitStatusExt::from_raw(0),
        capture: bounded::Capture {
            stdout: SPAWN_SCHEDULED.1.as_bytes().to_vec(),
            stderr: Vec::new(),
            complete: false,
        },
    };
    let not_run = || ClientError::NotRun(std::io::Error::other("no launchctl").into());
    let failures = [
        ("a nonzero exit", answer("print", UNREADABLE)),
        (
            "a nonzero exit with a state in its output",
            answer("print", (5, SPAWN_SCHEDULED.1, "")),
        ),
        ("not loaded", answer("print", NOT_LOADED)),
        ("output that is not whole", reply(&["print"], cut)),
        ("no launchctl to run", fail(&["print"], not_run())),
    ];
    for (what, entry) in failures {
        let (e, ended) = start_whose_kickstart_expires(entry);

        assert_wait_timeout(&e);
        assert_eq!(ended.fate(), Some(Fate::Release), "{what}");
    }
}

// kill ================================================================================================================

/// `kill_job` over `id`, with `answer` standing in for what `launchctl kill` says.
fn kill_answered(answer: Answer) -> (Result<Option<()>>, Fake) {
    let script = table(&[("kill", answer)]);
    let launchctl = Launchctl::new(Box::new(script.clone()));
    let mut held = reserved(&[Step::Kill]);
    let result = launchctl.kill_job(never_installed().as_str(), Budget::Unbounded.start(), &mut held);
    (result, script)
}

/// SIGKILL to the job by label, in the system domain; the signal is named so a failure of the job
/// is what launchd records, whatever `restart` then does.
#[skuld::test]
fn kill_sends_sigkill_to_the_job_in_the_system_domain() {
    let id = never_installed();
    let (result, script) = kill_answered(DONE);

    assert!(matches!(result, Ok(Some(()))), "{result:?}");
    assert_eq!(script.calls(), [["kill", "SIGKILL", format!("system/{id}").as_str()]]);
    assert_eq!(
        script.waits(),
        [Waited {
            role: RoleKind::Request,
            bounded: false
        }]
    );
}

/// The two ways launchd says there is no process to signal: the job is not loaded (113), or it is
/// loaded and has no process (3, "No such process"). Classified off the kill's own answer — a
/// separate `print` first would leave a window for the job to exit between the two.
#[skuld::test]
fn kill_of_a_job_with_no_process_is_not_running() {
    for answer in [(3, "", ""), NOT_LOADED] {
        let id = never_installed();
        let (result, script) = kill_answered(answer);

        let e = result.expect_err("no process to signal");
        assert!(
            matches!(&e, Error::NotRunning { id: named } if named == id.as_str()),
            "{answer:?}: {e:?}"
        );
        script.assert_spent("a kill classified off its own answer, with no read before it");
    }
}

/// Any other refusal is a failure of the request, with launchctl's words.
#[skuld::test]
fn kill_refused_for_any_other_reason_is_a_plain_failure() {
    let (result, _script) = kill_answered((1, "", "Operation not permitted\n"));

    let e = result.expect_err("refused");
    assert!(
        matches!(&e, Error::Command { stderr, .. } if stderr.contains("Operation not permitted")),
        "{e:?}"
    );
}

// A changed spec over a loaded job ====================================================================================

/// A changed spec over a job launchd has loaded is refused before anything is written, whether or
/// not the job has a process, and is installed once the job is not loaded. The table answers
/// `print` only: a `bootout` of the loaded job would be an unscripted call and fail the test.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_changed_spec_is_refused_while_the_job_is_loaded() {
    let name = format!("goetia-refuse-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    for loaded in [RUNNING, NOT_RUNNING] {
        let (mgr, script) = scripted(&[("print", loaded)]);
        let refused = mgr.install(&spec, false).expect_err("a changed spec over a loaded job");
        assert!(
            refused.to_string().contains(&format!("goetia daemon stop {name}")),
            "{refused}"
        );
        script.assert_spent("install over a loaded job sends nothing but the read");
        assert_eq!(
            std::fs::read_to_string(staging_path(&name)).unwrap(),
            installed,
            "a refused install left the plist as it was"
        );
    }

    let (mgr, script) = scripted(&[("print", NOT_LOADED), ("print", NOT_LOADED)]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_spent("install once the job is not loaded");
    assert_ne!(std::fs::read_to_string(staging_path(&name)).unwrap(), installed);
}

/// The spec the tests below install, under a name of their own: the tests run on parallel threads of
/// one process, so each needs plists no other writes.
fn spec_named(name: &str) -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        kind: Kind::Simple,
        ..plain_fields(name, vec!["/bin/sleep".to_string(), "300".to_string()])
    })
    .unwrap()
}

/// A `print` that fails for a reason other than "no such job" does not say the job is not loaded, so
/// `install` over an installed plist and over none, and `start`, fail on it instead of acting as if
/// the job were absent. Each table answers `print` only: a write under a loaded job, a `bootout`
/// or a `bootstrap` would be an unscripted call, and `assert_spent` would see it.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_print_that_fails_otherwise_is_an_error_not_a_job_that_is_not_loaded() {
    let name = format!("goetia-unreadable-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);
    let names_print = |e: &Error| matches!(e, Error::Command { command, .. } if command.starts_with("launchctl print"));

    // `Create`: the stale-job check, which comes before anything is written.
    let (mgr, script) = scripted(&[("print", UNREADABLE)]);
    let e = mgr.install(&spec, false).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_spent("create, unreadable print");
    assert!(
        !staging_path(&name).exists(),
        "a create that could not tell whether a job is loaded wrote nothing"
    );

    // `Create` over a loaded job whose `bootout` fails: nothing written either, so a rerun is a
    // `Create` again and reaches the stale job.
    let (mgr, script) = scripted(&[("print", DONE), ("bootout", REFUSED)]);
    let e = mgr.install(&spec, false).expect_err("a refused bootout");
    assert!(matches!(e, Error::Command { .. }), "{e:?}");
    script.assert_spent("create, refused bootout");
    assert!(
        !staging_path(&name).exists(),
        "a create whose stale job could not be booted out wrote nothing"
    );

    let (mgr, script) = scripted(&INSTALL_LONGEST);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("create over a loaded job");

    let (mgr, script) = scripted(&[("print", UNREADABLE)]);
    let e = mgr.start(&spec.id, Budget::DEFAULT).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_spent("start, unreadable first read");

    let (mgr, script) = scripted(&[("print", NOT_LOADED), ("bootstrap", REFUSED), ("print", UNREADABLE)]);
    let e = mgr.start(&spec.id, Budget::DEFAULT).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_spent("start, unreadable read after a refused bootstrap");

    // `Update`: the refusal check. The plist exists from the rerun above.
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();
    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    let (mgr, script) = scripted(&[("print", UNREADABLE)]);
    let e = mgr.install(&spec, false).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_spent("update, unreadable print");
    assert_eq!(
        std::fs::read_to_string(staging_path(&name)).unwrap(),
        installed,
        "an install that could not tell whether the job is loaded left the plist as it was"
    );
}

/// Installs a job named `name` with `restart` (no job loaded), then runs a waited `start` over the
/// fake `calls` and returns the error it failed with, having made every call scripted.
fn failed_start(name: &str, restart: Restart, calls: &[(&str, Answer)]) -> Error {
    let spec = DaemonSpec::new(DaemonFields {
        restart,
        ..spec_named(name).into_fields()
    })
    .unwrap();

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("install");

    let (mgr, script) = scripted(calls);
    let e = mgr
        .start(&spec.id, Budget::DEFAULT)
        .expect_err("a start that never read Running");
    script.assert_spent("start");
    e
}

/// `kickstart -p` named no pid and the bounded read decides: a job read back as `spawn scheduled` is
/// not established either way (exit `4`), one read back as stopped did not start (exit `1`).
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_kickstart_that_names_no_pid_and_reads_back_unknown_is_unconfirmed() {
    let name = format!("goetia-nopid-unknown-{:x}", std::process::id());
    let _plists = Plists::new(&name);

    let e = failed_start(
        &name,
        Restart::Never,
        &[
            ("print", DONE),
            ("print", NOT_RUNNING),
            ("kickstart", DONE),
            ("print", SPAWN_SCHEDULED),
        ],
    );

    assert!(matches!(e, Error::StartUnconfirmed { .. }), "{e:?}");
    assert!(e.is_indeterminate());
}

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_kickstart_that_names_no_pid_and_reads_back_stopped_did_not_start() {
    let name = format!("goetia-nopid-stopped-{:x}", std::process::id());
    let _plists = Plists::new(&name);

    let e = failed_start(
        &name,
        Restart::Never,
        &[
            ("print", DONE),
            ("print", NOT_RUNNING),
            ("kickstart", DONE),
            ("print", NOT_RUNNING),
        ],
    );

    assert!(matches!(e, Error::NotStarted { keep_alive: false, .. }), "{e:?}");
    assert!(!e.is_indeterminate());
}

/// A `restart: always` start down to the corrective cycle's `kickstart`, ahead of its closing read:
/// a pid named, then not running, so `bootout`, `bootstrap` and `kickstart` again.
const CORRECTIVE_CYCLE: [(&str, Answer); 8] = [
    ("print", DONE),
    ("print", NOT_RUNNING),
    ("kickstart", PID),
    ("print", NOT_RUNNING),
    ("bootout", DONE),
    ("bootstrap", DONE),
    ("print", NOT_RUNNING),
    ("kickstart", PID),
];

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_corrective_cycle_that_reads_back_unknown_is_unconfirmed() {
    let name = format!("goetia-cycle-unknown-{:x}", std::process::id());
    let _plists = Plists::new(&name);

    let e = failed_start(
        &name,
        Restart::Always,
        &then(&[&CORRECTIVE_CYCLE, &[("print", SPAWN_SCHEDULED)]]),
    );

    assert!(matches!(e, Error::StartUnconfirmed { .. }), "{e:?}");
    assert!(e.is_indeterminate());
}

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_corrective_cycle_that_reads_back_stopped_did_not_start() {
    let name = format!("goetia-cycle-stopped-{:x}", std::process::id());
    let _plists = Plists::new(&name);

    let e = failed_start(
        &name,
        Restart::Always,
        &then(&[&CORRECTIVE_CYCLE, &[("print", NOT_RUNNING)]]),
    );

    assert!(matches!(e, Error::NotStarted { keep_alive: true, .. }), "{e:?}");
    assert!(!e.is_indeterminate());
}

/// After an exit-`4` install a rerun is refused, not read as `UpToDate`.
fn assert_restored_and_rerun_refused(spec: &DaemonSpec, name: &str, installed: &str) {
    assert_eq!(
        std::fs::read_to_string(staging_path(name)).unwrap(),
        installed,
        "the previous plist was put back"
    );
    let (mgr, script) = scripted(&[("print", RUNNING)]);
    let refused = mgr.install(spec, false).expect_err("the rerun, job loaded");
    assert!(
        matches!(refused, Error::LoadedJob { .. })
            && refused.to_string().contains(&format!("goetia daemon stop {name}")),
        "a refusal, not an exit-4 or an up-to-date: {refused}"
    );
    script.assert_spent("the rerun reads the job and is refused");
}

/// A job that is not loaded when `install` checks but is loaded once it has written, because someone
/// else started it between the two, may have read either plist: the install is in doubt
/// ([`Error::LoadedJobInDoubt`], exit `4`), and the previous plist is put back.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_job_loaded_between_the_check_and_the_write_leaves_the_install_in_doubt() {
    let name = format!("goetia-indoubt-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    let (mgr, script) = scripted(&[("print", NOT_LOADED), ("print", RUNNING)]);
    let e = mgr.install(&spec, false).expect_err("loaded after the write");
    assert!(
        matches!(
            e,
            Error::LoadedJobInDoubt {
                restore_failed: None,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(e.to_string().contains(&format!("goetia daemon stop {name}")), "{e}");
    script.assert_spent("install, job loaded after the write");
    assert_restored_and_rerun_refused(&spec, &name, &installed);
}

/// The recheck after the write is a read like the first; one that fails leaves the loaded state
/// unknown, so the install is in doubt (exit `4`), not a plain failure (exit `1`), and the previous
/// plist is put back.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn an_unreadable_recheck_after_the_write_leaves_the_install_in_doubt() {
    let name = format!("goetia-recheck-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    let (mgr, script) = scripted(&[("print", NOT_LOADED), ("print", UNREADABLE)]);
    let e = mgr.install(&spec, false).expect_err("an unreadable recheck");
    assert!(
        matches!(
            e,
            Error::LoadedStateInDoubt {
                restore_failed: None,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(e.to_string().contains(&format!("goetia daemon stop {name}")), "{e}");
    assert!(e.report().to_string().contains("Input/output error"), "{}", e.report());
    script.assert_spent("install, unreadable recheck");
    assert_restored_and_rerun_refused(&spec, &name, &installed);
}

/// What launchd reads decides the refusal, not the spec: a spec change the plist does not render
/// (`name` is in the embedded metadata only) is written over a loaded job without a single
/// `launchctl` call, and the loaded job is as the new plist would have it.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_spec_change_the_plist_does_not_render_is_written_over_a_loaded_job() {
    let name = format!("goetia-samerender-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    let mut fields = spec.into_fields();
    fields.name = "another name".to_string();
    let spec = DaemonSpec::new(fields).unwrap();
    let (mgr, script) = scripted(&[]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_spent("an install whose rendering is unchanged asks launchd nothing");
    assert_ne!(
        std::fs::read_to_string(staging_path(&name)).unwrap(),
        installed,
        "the metadata comment was refreshed"
    );
}

// A put-back, and a Create, that races ================================================================================

/// Installs a spec, then a changed one whose read after the write first runs `during` (standing for
/// something else on the host) and then answers "loaded". Returns that install's error and the text of
/// the changed plist it wrote.
fn install_loaded_after_write(name: &str, during: impl FnOnce() + Send + 'static) -> (Error, String) {
    let spec = spec_named(name);
    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");

    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    let changed = generate::plist(
        &spec,
        &Identity {
            user: "root".to_string(),
        },
    );
    let script = Fake::new([answer("print", NOT_LOADED), answer("print", RUNNING).then(during)]);
    let mgr = launchd_over(script.clone());
    let e = mgr.install(&spec, false).expect_err("loaded after the write");
    script.assert_spent("install, job loaded after the write");
    (e, changed)
}

/// The failed put-back of an install that ended in doubt, or a panic.
fn restore_failure_of(e: &Error) -> &Error {
    match e {
        Error::LoadedJobInDoubt {
            restore_failed: Some(cause),
            ..
        } => cause,
        other => panic!("expected an exit-4 error whose put-back failed: {other:?}"),
    }
}

/// The put-back writes only over what this install wrote: a concurrent install that wrote after it
/// owns the path, and a put-back over it would destroy that write.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_put_back_does_not_overwrite_a_plist_that_is_no_longer_ours() {
    let name = format!("goetia-putback-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let target = staging_path(&name);

    let taken = target.clone();
    let (e, _) = install_loaded_after_write(&name, move || std::fs::write(taken, "someone else\n").unwrap());

    assert!(
        matches!(
            restore_failure_of(&e),
            Error::PutBackBlocked {
                found: PutBackFound::Replaced,
                ..
            }
        ),
        "{e}"
    );
    assert!(e.report().to_string().contains(&target.display().to_string()), "{e}");
    assert!(!e.to_string().contains("plist is back as it was"), "{e}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else\n");
}

/// A concurrent `enable` moved the plist to `ENABLED_DIR` before the put-back: writing the previous
/// plist back to staging would leave one in both directories, which `locate` refuses for every verb.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_put_back_does_not_recreate_a_plist_that_was_enabled_meanwhile() {
    let name = format!("goetia-putback-enabled-{:x}", std::process::id());
    let _plists = Plists::new(&name);

    let (staging, enabled) = (staging_path(&name), enabled_path(&name));
    let (e, changed) = install_loaded_after_write(&name, move || std::fs::rename(staging, enabled).unwrap());

    assert!(
        matches!(
            restore_failure_of(&e),
            Error::PutBackBlocked {
                found: PutBackFound::Gone,
                ..
            }
        ),
        "{e}"
    );
    assert!(!staging_path(&name).exists(), "the put-back recreated the staged plist");
    assert_eq!(std::fs::read_to_string(enabled_path(&name)).unwrap(), changed);
}

/// A concurrent `uninstall` removed the plist before the put-back: writing the previous one back would
/// resurrect a daemon that was just uninstalled.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_put_back_does_not_resurrect_a_plist_that_was_uninstalled_meanwhile() {
    let name = format!("goetia-putback-removed-{:x}", std::process::id());
    let _plists = Plists::new(&name);

    let staged = staging_path(&name);
    let (e, _) = install_loaded_after_write(&name, move || std::fs::remove_file(staged).unwrap());

    assert!(
        matches!(
            restore_failure_of(&e),
            Error::PutBackBlocked {
                found: PutBackFound::Gone,
                ..
            }
        ),
        "{e}"
    );
    assert!(!staging_path(&name).exists(), "the put-back resurrected the plist");
}

/// A plist from an older release is `Stale`, and `Stale` is refused over a loaded job exactly when
/// its rendering differs from what would be written: a release that renders the same text only
/// refreshes the metadata, without asking launchd anything.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_stale_plist_is_refused_over_a_loaded_job_only_when_its_rendering_differs() {
    let name = format!("goetia-stale-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    let older = installed
        .replace(
            &crate::blob::encode(&spec),
            &crate::blob::encode_with_version(&spec, "0.0.1"),
        )
        .replace(&format!("Version: {}", crate::version()), "Version: 0.0.1");
    assert_ne!(older, installed);

    let rendered_differently = older.replace("<string>300</string>", "<string>299</string>");
    assert_ne!(rendered_differently, older);
    std::fs::write(staging_path(&name), &rendered_differently).unwrap();
    assert!(matches!(mgr.preview_install(&spec), Ok(Outcome::Stale { .. })));
    let (mgr, script) = scripted(&[("print", RUNNING)]);
    let refused = mgr
        .install(&spec, false)
        .expect_err("a stale plist that renders differently");
    assert!(
        matches!(refused, Error::LoadedJob { .. })
            && refused.to_string().contains(&format!("goetia daemon stop {name}")),
        "{refused}"
    );
    script.assert_spent("the stale plist's rendering differs, so launchd is asked");
    assert_eq!(
        std::fs::read_to_string(staging_path(&name)).unwrap(),
        rendered_differently,
        "a refused install left the plist as it was"
    );

    std::fs::write(staging_path(&name), &older).unwrap();
    assert!(matches!(mgr.preview_install(&spec), Ok(Outcome::Stale { .. })));
    let (mgr, script) = scripted(&[]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Stale { .. })));
    script.assert_spent("a stale plist that renders the same asks launchd nothing");
    assert_eq!(std::fs::read_to_string(staging_path(&name)).unwrap(), installed);
}

/// A `Create` that fails before it has a plist to write has not stopped the job under its label: the
/// stale job is booted out only once the plist is staged, so a rerun that fails the same way leaves
/// the predecessor running.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_create_that_cannot_stage_its_plist_boots_nothing_out() {
    let name = format!("goetia-unstaged-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, "").unwrap();
    let spec = DaemonSpec::new(DaemonFields {
        cwd: Some(blocker.join("cwd")),
        ..spec_named(&name).into_fields()
    })
    .unwrap();

    let (mgr, script) = scripted(&[]);
    let e = mgr.install(&spec, false).expect_err("a cwd that cannot be created");
    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    script.assert_spent("a create that cannot stage asks launchd nothing");
    assert!(!staging_path(&name).exists());
}

/// A job loaded between a `Create`'s check and its write is the job the plist was written under, which
/// `start` would find loaded and running: the install is in doubt (exit `4`), the plist it created is
/// removed, and the rerun is a `Create` again, not `UpToDate`. Reading the job back fails the same way.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_job_loaded_after_a_create_leaves_the_install_in_doubt_with_no_plist() {
    let name = format!("goetia-create-indoubt-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);

    for (calls, what) in [
        (
            vec![("print", NOT_LOADED), ("print", RUNNING)],
            "loaded after the write",
        ),
        (
            vec![("print", DONE), ("bootout", DONE), ("print", RUNNING)],
            "reloaded after the bootout",
        ),
    ] {
        let (mgr, script) = scripted(&calls);
        let e = mgr.install(&spec, false).expect_err(what);
        assert!(
            matches!(
                e,
                Error::LoadedJobInDoubt {
                    restore_failed: None,
                    ..
                }
            ),
            "{what}: {e:?}"
        );
        script.assert_spent(what);
        assert!(!staging_path(&name).exists(), "{what}: the created plist was left");
    }

    let (mgr, script) = scripted(&[("print", NOT_LOADED), ("print", UNREADABLE)]);
    let e = mgr.install(&spec, false).expect_err("an unreadable recheck");
    assert!(
        matches!(
            e,
            Error::LoadedStateInDoubt {
                restore_failed: None,
                ..
            }
        ),
        "{e:?}"
    );
    script.assert_spent("create, unreadable recheck");
    assert!(!staging_path(&name).exists(), "the created plist was left");

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the rerun is a create again");
}

/// A plist appears at the target while the stale job's `bootout` runs. A request has been sent, so the
/// install does not go on to re-derive into further `launchctl` calls: it reports that the job was
/// booted out and what is at the path (exit `4`), and leaves what is there.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_plist_taken_after_the_bootout_is_reported_without_further_launchctl_calls() {
    let name = format!("goetia-taken-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);
    let target = staging_path(&name);

    let taken = target.clone();
    let script = Fake::new([
        answer("print", DONE),
        answer("bootout", DONE).then(move || std::fs::write(taken, "someone else\n").unwrap()),
    ]);
    let mgr = launchd_over(script.clone());
    let e = mgr
        .install(&spec, false)
        .expect_err("the plist was taken after the bootout");
    assert!(matches!(e, Error::StaleJobBootedOut { .. }), "{e:?}");
    assert!(e.to_string().contains("booted out"), "{e}");
    assert!(e.to_string().contains(&target.display().to_string()), "{e}");
    script.assert_spent("nothing is sent after the race");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else\n");
}

/// The same race after a read only: nothing was sent and nothing written, but `install` does not
/// re-derive into a second pass, whose reads the verb did not reserve. It fails (exit `1`) and says to
/// run it again, and leaves what is at the path.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_plist_taken_after_a_read_fails_the_install_without_further_launchctl_calls() {
    let name = format!("goetia-taken-early-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);
    let target = staging_path(&name);

    let taken = target.clone();
    let script =
        Fake::new([answer("print", NOT_LOADED).then(move || std::fs::write(taken, "someone else\n").unwrap())]);
    let mgr = launchd_over(script.clone());
    let e = mgr.install(&spec, false).expect_err("the plist was taken after a read");
    assert!(matches!(e, Error::ConcurrentChange { .. }), "{e:?}");
    assert!(e.to_string().contains("re-run"), "{e}");
    assert!(e.to_string().contains(&target.display().to_string()), "{e}");
    script.assert_spent("nothing is sent after the race");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else\n");
}

/// The plist is removed (a concurrent `uninstall`) after the read that found no job loaded, while a
/// changed spec is being written over it. A call has been made, so `install` does not re-derive: a
/// second pass is a `Create` over what may be a job loaded meanwhile, which needs more calls than the
/// verb reserved. It fails (exit `1`) and writes nothing.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_plist_removed_after_a_read_fails_the_install_without_further_launchctl_calls() {
    let name = format!("goetia-vanished-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let spec = spec_named(&name);
    let target = staging_path(&name);

    let (mgr, script) = scripted(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_spent("the first install");

    let mut fields = spec.into_fields();
    fields.command.push("updated".to_string());
    let spec = DaemonSpec::new(fields).unwrap();
    let vanished = target.clone();
    let script = Fake::new([answer("print", NOT_LOADED).then(move || std::fs::remove_file(vanished).unwrap())]);
    let mgr = launchd_over(script.clone());
    let e = mgr
        .install(&spec, false)
        .expect_err("the plist was removed after a read");
    assert!(matches!(e, Error::ConcurrentChange { .. }), "{e:?}");
    assert!(e.to_string().contains("re-run"), "{e}");
    script.assert_spent("nothing is sent after the vanish");
    assert!(!target.exists(), "the install wrote a plist over the vanished one");
}

/// `install --dry-run` and `diff` read no job state, and `install --start` over a loaded job with a
/// changed rendering is refused before it bootstraps anything: the table answers one `print`, so
/// any other call fails the test.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn dry_run_and_diff_ask_launchd_nothing_and_install_start_is_refused_before_it_starts() {
    use crate::cli::wait::WaitArgs;
    use crate::cli::{ExitCode, diff, install};

    let name = format!("goetia-cli-{:x}", std::process::id());
    let _plists = Plists::new(&name);
    let dir = tempfile::tempdir().unwrap();
    let manifest = |seconds: &str| {
        std::fs::write(
            dir.path().join("goetia.yaml"),
            format!("id: {name}\ncommand: [/bin/sleep, \"{seconds}\"]\n"),
        )
        .unwrap();
    };
    let manager_over = |script: &Fake| {
        let script = script.clone();
        move || -> Result<Box<dyn ServiceManager>> { Ok(Box::new(launchd_over(script.clone()))) }
    };
    let run_install = |script: &Fake, start: bool, dry_run: bool| {
        let args = install::Args {
            file: dir.path().to_path_buf(),
            force: false,
            start,
            enable: false,
            dry_run,
            wait: WaitArgs::default(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = install::run(&args, &manager_over(script), &|| true, &mut out, &mut err);
        (code, String::from_utf8(err).unwrap())
    };

    manifest("300");
    let script = table(&CREATED);
    assert_eq!(run_install(&script, false, false).0, ExitCode::Success);
    script.assert_spent("the first install");

    manifest("299");
    let script = table(&[]);
    assert_eq!(
        run_install(&script, false, true).0,
        ExitCode::Success,
        "install --dry-run"
    );
    let args = diff::Args {
        file: dir.path().to_path_buf(),
    };
    let code = diff::run(&args, &manager_over(&script), &mut Vec::new(), &mut Vec::new());
    assert_eq!(code, ExitCode::Drift, "diff reports the drift");
    script.assert_spent("neither --dry-run nor diff asks launchd");

    let script = table(&[("print", RUNNING)]);
    let (code, err) = run_install(&script, true, false);
    assert_eq!(code, ExitCode::Error, "{err}");
    assert!(err.contains(&format!("goetia daemon stop {name}")), "{err}");
    script.assert_spent("install --start is refused after one read, before any bootstrap");
}
