//! Unit coverage for the pieces of the launchd backend that neither need
//! root nor the real `/Library/...` system paths: `persist_new`/
//! `write_existing` take an arbitrary target path, so the non-clobbering
//! race and the `plutil -lint` gate are both fully testable against a
//! `tempfile::tempdir()`. Everything that genuinely needs elevation and the
//! real filesystem locations (`locate`/`read`, `enable`/`disable`,
//! `start`/`stop` against real launchd) lives in
//! `tests/launchd_integration.rs` instead.

use super::*;
use crate::manager::ServiceManager;
use crate::spec::{Id, Kind, Restart, User};

fn sample_plist(id: &str) -> String {
    let spec = DaemonSpec {
        id: Id::try_from(id).unwrap(),
        name: id.to_string(),
        command: vec!["/bin/true".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    };
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
        assert!(err.to_string().contains("no longer holds"), "{err}");
        assert!(err.to_string().contains(&target.display().to_string()), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else's plist\n");
    }
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
        assert!(err.to_string().contains("no longer"), "{err}");
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

    ensure_dir_owned_by(&dir, &account).expect("an existing directory is accepted");

    assert_eq!(mode_and_gid(&dir), before);
    assert_eq!(before.0, 0o700);
}

#[skuld::test]
fn ensure_dir_owned_by_creates_and_hands_over_every_missing_component() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let leaf = tmp.path().join("a").join("b").join("c");

    ensure_dir_owned_by(&leaf, &account).expect("create the whole chain");

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

    create_owned(&missing, &account).expect("a lost race is not an error");

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

    ensure_dir_owned_by(&link, &account).expect("an existing path of any kind is accepted");

    assert!(!target.exists(), "the link's target must not be created");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
}

#[skuld::test]
fn ensure_dir_owned_by_accepts_a_regular_file() {
    let tmp = tempfile::tempdir().unwrap();
    let account = other_group_account(tmp.path());
    let file = tmp.path().join("file");
    std::fs::write(&file, b"x").unwrap();

    ensure_dir_owned_by(&file, &account).expect("an existing path of any kind is accepted");

    assert_eq!(std::fs::read(&file).unwrap(), b"x");
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
        panic!("`Error::Io` reaches `status` as `unreadable`, which claims goetia owns the id: {err:?}");
    };
    assert_eq!(id, "opaque");
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
/// `Outcome::RefuseUnreadable`'s remedy and it certifies the ownership this read never established.
#[skuld::test]
fn the_launchd_undetermined_recovery_names_both_causes_and_not_uninstall() {
    let path = Path::new(ENABLED_DIR).join("x.plist");

    for (source, wants_elevation) in [
        (io::Error::from(io::ErrorKind::PermissionDenied), true),
        (io::Error::from(io::ErrorKind::Other), false),
    ] {
        let rendered = format!("{source:?}");
        let Error::Undetermined { id, reason, recovery } = undetermined("x", "read", &path, &source) else {
            panic!("{rendered}: every non-`NotFound` failure is undetermined, not just a permission denial");
        };

        assert_eq!(id, "x");
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
        [(name, ReadOutcome::Marked { blob, snapshot })] => {
            assert_eq!(name, "ours");
            assert!(blob.is_ok(), "the marker decodes");
            assert!(snapshot.location.enabled);
            assert_eq!(snapshot.location.path, path);
            assert_eq!(snapshot.text, sample_plist("ours"));
        }
        _ => panic!("one marked entry expected, got {} entries", scan.entries.len()),
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
        matches!(scan.entries.as_slice(), [(name, ReadOutcome::Foreign { .. })] if name == "theirs"),
        "{} entries",
        scan.entries.len()
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
    assert_eq!(
        scan.reported,
        [Installed::OursUnreadable {
            name: "twice".to_string(),
            reason: format!("present in both {STAGING_DIR} and {ENABLED_DIR}"),
        }]
    );
}

// Writing: a race that has cost no call ===============================================================================

/// A plist that vanished between `read` and `write` — a concurrent `uninstall` — before `install`
/// made any `launchctl` call is `Raced`, so the flow reads again. The rendering is unchanged (only the
/// version differs), so no `is_loaded` was asked: a second pass stays inside the reservation. The
/// counterpart, a vanish after that call, is
/// `a_plist_removed_after_a_read_fails_the_install_without_further_launchctl_calls`.
#[skuld::test]
fn a_write_over_a_plist_that_vanished_before_any_call_is_raced() {
    use crate::backend::Backend as _;

    let mgr = LaunchdManager::new();
    let spec = spec_named("goetia-raced-before-any-call");
    let desired = mgr.render(&spec).expect("render");
    let on_disk = desired.replace(&format!("Version: {}", crate::version()), "Version: 0.0.1");
    assert_ne!(on_disk, desired);
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("gone.plist");
    let snapshot = Plist {
        location: Location {
            path: path.clone(),
            enabled: false,
        },
        text: on_disk,
    };
    let stale = Outcome::Stale {
        from_version: "0.0.1".to_string(),
    };
    let mut scope = mgr.begin_install(&spec).expect("begin_install");

    let written = mgr
        .write(&spec, &desired, &stale, Some(&snapshot), &mut scope)
        .expect("write");

    assert!(matches!(written, WriteOutcome::Raced));
    assert!(!path.exists(), "a raced write puts nothing back");
}

// Reapers: a sequence never fails halfway for want of one =============================================================

/// An id nothing is installed at: a verb that gets past its reapers stops at discovery, before any
/// `launchctl` runs, so these tests touch nothing on the host.
fn never_installed() -> Id {
    Id::try_from("goetia-reaper-probe-never-installed").unwrap()
}

/// Whether `result` is the failure of a verb that could not make its reapers — as opposed to one that
/// made them and went on to discovery.
fn refused_for_want_of_a_reaper<T: std::fmt::Debug>(result: &Result<T>) -> bool {
    matches!(result, Err(e) if e.to_string().contains("no thread could be made to reap a `launchctl` request, so none was sent"))
}

/// Every verb makes every reaper it can need before anything else — discovery included, so before
/// any `launchctl` runs: one short, and the verb fails having sent nothing. `start`'s corrective
/// cycle is its fourth and fifth requests, so four reapers are one short.
#[skuld::test]
fn a_verb_one_reaper_short_sends_nothing() {
    let mgr = LaunchdManager::new();
    let id = never_installed();
    let spec = DaemonSpec {
        id: id.clone(),
        name: id.to_string(),
        command: vec!["/bin/true".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Always,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    };
    for budget in [Budget::DEFAULT, Budget::Immediate] {
        let _four = bounded::test_hook::threads(START_REQUESTS - 1);
        assert!(
            refused_for_want_of_a_reaper(&mgr.start(&id, budget)),
            "start {budget:?}"
        );
    }
    let _four = bounded::test_hook::threads(START_REQUESTS - 1);
    assert!(refused_for_want_of_a_reaper(&mgr.request_start_after_stop(&id)));
    let _none = bounded::test_hook::threads(0);
    assert!(refused_for_want_of_a_reaper(&mgr.stop(&id, Budget::DEFAULT)));
    assert!(refused_for_want_of_a_reaper(&mgr.kill(&id)));
    assert!(refused_for_want_of_a_reaper(&mgr.uninstall(&id)));
    assert!(refused_for_want_of_a_reaper(&mgr.install(&spec, false)));
}

/// With every reaper made, the same verbs go on to discovery, which finds nothing installed.
#[skuld::test]
fn a_verb_with_its_reapers_goes_on_to_discovery() {
    let mgr = LaunchdManager::new();
    let id = never_installed();
    let _five = bounded::test_hook::threads(START_REQUESTS);
    assert!(!refused_for_want_of_a_reaper(&mgr.start(&id, Budget::DEFAULT)));
    let _one = bounded::test_hook::threads(STOP_REQUESTS);
    assert!(!refused_for_want_of_a_reaper(&mgr.stop(&id, Budget::DEFAULT)));
    let _one = bounded::test_hook::threads(KILL_REQUESTS);
    assert!(!refused_for_want_of_a_reaper(&mgr.kill(&id)));
}

/// Whether `result` is the failure of a verb that could not make the temp files its `launchctl`
/// calls write to — as opposed to one that made them and went on to discovery.
fn refused_for_want_of_a_file<T: std::fmt::Debug>(result: &Result<T>) -> bool {
    matches!(result, Err(e) if e.to_string().contains("no temp file could be made for a `launchctl` call's output, so nothing was sent"))
}

/// Every verb makes the temp files every `launchctl` call it can make writes to, reads and requests
/// alike, before anything else: one short, and the verb fails having sent nothing.
#[skuld::test]
fn a_verb_one_temp_file_short_sends_nothing() {
    let mgr = LaunchdManager::new();
    let id = never_installed();
    let spec = DaemonSpec {
        id: id.clone(),
        name: id.to_string(),
        command: vec!["/bin/true".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Always,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    };
    let short = |calls: usize| bounded::test_hook::temp_files(calls * FILES_PER_CALL - 1);
    for budget in [Budget::DEFAULT, Budget::Immediate] {
        let _short = short(START_CALLS);
        assert!(refused_for_want_of_a_file(&mgr.start(&id, budget)), "start {budget:?}");
    }
    {
        let _short = short(START_CALLS);
        assert!(refused_for_want_of_a_file(&mgr.request_start_after_stop(&id)));
    }
    let _short = short(STOP_CALLS);
    assert!(refused_for_want_of_a_file(&mgr.stop(&id, Budget::DEFAULT)));
    let _short = short(STOP_CALLS);
    assert!(refused_for_want_of_a_file(&mgr.uninstall(&id)));
    let _short = short(KILL_CALLS);
    assert!(refused_for_want_of_a_file(&mgr.kill(&id)));
    let _short = short(INSTALL_CALLS);
    assert!(refused_for_want_of_a_file(&mgr.install(&spec, false)));
    let _enough = bounded::test_hook::temp_files(START_CALLS * FILES_PER_CALL);
    assert!(!refused_for_want_of_a_file(&mgr.start(&id, Budget::DEFAULT)));
}

/// `restart`'s two legs, and `install --start`'s, draw on reapers and temp files `prepare` made
/// before either sent anything: once it succeeded, neither leg can fail for want of one. It fails as
/// a whole when one is short. `uninstall` stands in for the install, which would write a plist: each
/// takes one reaper, and no more temp files than the install.
#[skuld::test]
fn prepared_steps_need_nothing_made_later() {
    let mgr = LaunchdManager::new();
    let id = never_installed();
    for (steps, reapers, calls) in [
        (
            [Step::Stop, Step::Start],
            STOP_REQUESTS + START_REQUESTS,
            STOP_CALLS + START_CALLS,
        ),
        (
            [Step::Install, Step::Start],
            INSTALL_REQUESTS + START_REQUESTS,
            INSTALL_CALLS + START_CALLS,
        ),
    ] {
        // Every budget: `launchctl` is waited on under all of them.
        for budget in [Budget::DEFAULT, Budget::Immediate, Budget::Unbounded] {
            {
                let _short = bounded::test_hook::threads(reapers - 1);
                assert!(mgr.prepare(&steps, budget).is_err(), "{steps:?} {budget:?}");
            }
            let _short = bounded::test_hook::temp_files(calls * FILES_PER_CALL - 1);
            assert!(mgr.prepare(&steps, budget).is_err(), "{steps:?} {budget:?}");
        }
        let prepared = mgr.prepare(&steps, Budget::DEFAULT).expect("prepare");
        let _no_thread = bounded::test_hook::threads(0);
        let _no_file = bounded::test_hook::temp_files(0);
        let first = if steps[0] == Step::Stop {
            mgr.stop(&id, Budget::DEFAULT)
        } else {
            mgr.uninstall(&id)
        };
        assert!(first.is_err(), "{steps:?}: nothing is installed");
        assert!(!refused_for_want_of_a_reaper(&first), "{steps:?}");
        assert!(!refused_for_want_of_a_file(&first), "{steps:?}");
        let started = mgr.start(&id, Budget::DEFAULT);
        assert!(!refused_for_want_of_a_reaper(&started), "{steps:?}");
        assert!(!refused_for_want_of_a_file(&started), "{steps:?}");
        drop(prepared);
    }
}

// A request in doubt ==================================================================================================

/// A `launchctl` request that may have started before goetia lost it — cosca failing past `exec`,
/// or the wait failing — may have reached launchd: [`Error::RequestInDoubt`], exit `4`, and never
/// known to have, since `launchctl` never says so. A read changes nothing, so the same failure there
/// is a plain one. A failure placed before the spawn is a plain one for both. The spawn failures are
/// injected, so nothing runs for them; the failed waits are injected once a stand-in that exits `0`
/// has run, so no `launchctl` ever runs, and launchd is never asked anything.
#[skuld::test]
fn a_launchctl_request_that_may_have_run_is_in_doubt() {
    let _stand_in = launchctl_stand_in::set("exit 0".to_string());
    let args = ["bootout", "system/goetia-reaper-probe-never-installed"];
    let deadline = Budget::Unbounded.start();
    let request = || Role::Request(bounded::reapers::<1>().unwrap().into_iter().next().unwrap());
    type Inject = fn(fn() -> cosca::error::Error);
    let after_exec: [Inject; 2] = [bounded::test_hook::spawn_fails, bounded::test_hook::wait_fails];
    for inject in after_exec {
        inject(|| cosca::error::Error::Containment {
            detail: "forced".into(),
        });
        let e = launchctl(&args, deadline, request()).err().expect("injected");
        assert!(matches!(e, Error::RequestInDoubt { reached: false, .. }), "{e:?}");

        inject(|| cosca::error::Error::Containment {
            detail: "forced".into(),
        });
        let e = launchctl(&args, deadline, Role::Query).err().expect("injected");
        assert!(matches!(e, Error::CommandFailed { .. }), "{e:?}");
    }
    bounded::test_hook::spawn_fails(|| io::Error::from_raw_os_error(libc::EAGAIN).into());
    let e = launchctl(&args, deadline, request()).err().expect("injected");
    assert!(matches!(e, Error::CommandFailed { .. }), "{e:?}");
}

// Every verb path stays inside its reservation ========================================================================

#[skuld::label]
const ELEVATED: skuld::Label;

fn elevated() -> std::result::Result<(), String> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err("writes a plist under /Library/Application Support/Goetia; re-run under sudo".to_string())
    }
}

/// What the stand-in answers one `launchctl` call with: a shell snippet.
const NOT_LOADED: &str = "exit 113";
const DONE: &str = "exit 0";
const REFUSED: &str = "echo 'Bootstrap failed: 5: Input/output error' >&2; exit 5";
/// A `print` that failed for a reason other than "no such job".
const UNREADABLE: &str = "echo 'Could not print: 5: Input/output error' >&2; exit 5";
const RUNNING: &str = "printf '\\tstate = running\\n\\tpid = 4242\\n'";
const NOT_RUNNING: &str = "printf '\\tstate = not running\\n'";
const PID: &str = "echo 4242";

/// `start`'s longest path, every one of its [`START_CALLS`] `launchctl` calls: not loaded; a
/// `bootstrap` refused, by a concurrent start that loaded it meanwhile; a `kickstart -p` that names
/// no pid, so a bounded read decides; and then, `restart: always` reading not running, the
/// corrective `bootout`, `bootstrap` and `kickstart`, and the read that confirms it.
const START_LONGEST: [(&str, &str); START_CALLS] = [
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
const START_NOT_WAITING: [(&str, &str); 5] = [
    ("print", NOT_LOADED),
    ("bootstrap", REFUSED),
    ("print", DONE),
    ("print", NOT_RUNNING),
    ("kickstart", DONE),
];

/// `install`'s longest path, [`INSTALL_CALLS`] calls: a `Create`'s read, the `bootout` of the stale job
/// it finds loaded, and the read after the write. An `Update` makes two reads, before the write and
/// after it.
const INSTALL_LONGEST: [(&str, &str); INSTALL_CALLS] = [("print", DONE), ("bootout", DONE), ("print", NOT_LOADED)];

/// A `Create` over no loaded job: the read before the write and the read after it.
const CREATED: [(&str, &str); 2] = [("print", NOT_LOADED), ("print", NOT_LOADED)];

/// `stop`'s and `uninstall`'s one call.
const BOOTOUT: [(&str, &str); STOP_CALLS] = [("bootout", DONE)];

/// A stand-in `launchctl` for this thread that answers the calls it gets, in order, from
/// `expected`, logs each call's arguments, and fails any call past the last.
struct Scripted {
    dir: tempfile::TempDir,
    expected: Vec<&'static str>,
    _stand_in: launchctl_stand_in::Guard,
}

impl Scripted {
    fn new(expected: &[(&'static str, &str)]) -> Scripted {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n"), "0").unwrap();
        let arms: String = expected
            .iter()
            .enumerate()
            .map(|(i, (_, answer))| format!("{}) {answer};; ", i + 1))
            .collect();
        let script = format!(
            "d='{}'; n=$(( $(cat \"$d/n\") + 1 )); echo \"$n\" > \"$d/n\"; echo \"$*\" >> \"$d/log\"; \
             case \"$n\" in {arms}*) echo \"unscripted launchctl call $n: $*\" >&2; exit 99;; esac",
            dir.path().display()
        );
        Scripted {
            expected: expected.iter().map(|(verb, _)| *verb).collect(),
            _stand_in: launchctl_stand_in::set(script),
            dir,
        }
    }

    /// Every call was made, in the order scripted — the path was driven end to end.
    fn assert_driven(&self, what: &str) {
        let log = std::fs::read_to_string(self.dir.path().join("log")).unwrap_or_default();
        let made: Vec<&str> = log
            .lines()
            .map(|line| line.split_whitespace().next().unwrap_or_default())
            .collect();
        assert_eq!(made, self.expected, "{what}: the calls made");
    }
}

/// Every stand-in call list, concatenated: one script for a prepared sequence.
fn then(steps: &[&[(&'static str, &'static str)]]) -> Vec<(&'static str, &'static str)> {
    steps.iter().flat_map(|step| step.iter().copied()).collect()
}

/// Removes the plists a test's install wrote, if a failure left them.
struct Plists(String);

impl Drop for Plists {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(staging_path(&self.0));
        let _ = std::fs::remove_file(enabled_path(&self.0));
    }
}

/// Every launchd verb, down its longest path, makes no thread and no temp file outside the
/// reservation it made at entry — or that `prepare` made for its sequence: one made on the spot
/// while a reservation is live fails a debug assertion, so a count one short fails here. Under
/// `prepare`, what the sequence reserved is spent exactly, so a count one over fails too. A
/// stand-in `launchctl` answers every call, so the longest paths — `start`'s corrective cycle
/// among them — are driven deterministically, and no job is ever loaded.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn every_launchd_verb_path_stays_inside_its_reservation() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-reservation-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let id = Id::try_from(name.as_str()).unwrap();
    let mut spec = DaemonSpec {
        id: id.clone(),
        name: name.clone(),
        command: vec!["/bin/sleep".to_string(), "300".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Always,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    };
    let exhausted = || {
        assert_eq!(
            bounded::test_hook::pooled(),
            bounded::Needs::default(),
            "the sequence reserved more than its steps spent"
        );
    };

    // Each verb alone, under its own reservation.
    let script = Scripted::new(&INSTALL_LONGEST);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("install, created over a loaded job");

    let script = Scripted::new(&START_LONGEST);
    mgr.start(&id, Budget::DEFAULT).expect("start");
    script.assert_driven("start, longest path");

    let script = Scripted::new(&START_NOT_WAITING);
    mgr.request_start_after_stop(&id).expect("request_start_after_stop");
    script.assert_driven("request_start_after_stop, longest path");

    let script = Scripted::new(&BOOTOUT);
    mgr.stop(&id, Budget::DEFAULT).expect("stop");
    script.assert_driven("stop");

    spec.command.push("updated".to_string());
    let script = Scripted::new(&[("print", NOT_LOADED), ("print", NOT_LOADED)]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_driven("install, updating a job that is not loaded");

    // `restart`: the stop and the start under one reservation, spent exactly.
    let script = Scripted::new(&then(&[&BOOTOUT, &START_LONGEST]));
    let prepared = mgr.prepare(&[Step::Stop, Step::Start], Budget::DEFAULT).unwrap();
    mgr.stop(&id, Budget::DEFAULT).expect("restart's stop");
    mgr.start(&id, Budget::DEFAULT).expect("restart's start");
    exhausted();
    drop(prepared);
    script.assert_driven("restart");

    // `install --start`: the install and the start under one reservation, spent exactly. The install
    // is a `Create` over a stale loaded job, the one install path that sends a request.
    std::fs::remove_file(staging_path(&name)).expect("remove the plist so the install creates it");
    let script = Scripted::new(&then(&[&INSTALL_LONGEST, &START_LONGEST]));
    let prepared = mgr.prepare(&[Step::Install, Step::Start], Budget::DEFAULT).unwrap();
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    mgr.start(&id, Budget::DEFAULT).expect("install --start's start");
    exhausted();
    drop(prepared);
    script.assert_driven("install --start");

    // `status` counts nothing, so it draws on no reservation: inside one with spares left, and one
    // spent, it makes its own files, trips no assertion, and leaves the pool as it was.
    let script = Scripted::new(&[("print", RUNNING), ("bootout", DONE), ("print", RUNNING)]);
    let prepared = mgr.prepare(&[Step::Stop], Budget::DEFAULT).unwrap();
    let pooled = bounded::test_hook::pooled();
    assert_eq!(mgr.status(&id).expect("status").state, State::Running);
    assert_eq!(bounded::test_hook::pooled(), pooled, "status drew on the reservation");
    mgr.stop(&id, Budget::DEFAULT).expect("stop");
    exhausted();
    assert_eq!(mgr.status(&id).expect("status").state, State::Running);
    exhausted();
    drop(prepared);
    script.assert_driven("status inside a reservation");

    let script = Scripted::new(&BOOTOUT);
    mgr.uninstall(&id).expect("uninstall");
    script.assert_driven("uninstall");
    assert!(!staging_path(&name).exists(), "uninstall left the plist");
}

// kill ================================================================================================================

/// `kill_job` over `id`, with `answer` standing in for what `launchctl kill` says.
fn kill_answered(answer: &'static str) -> (Result<Option<()>>, Scripted) {
    let script = Scripted::new(&[("kill", answer)]);
    let [reaper] = reapers::<KILL_REQUESTS>(&never_installed()).unwrap();
    let _files = files(&never_installed(), KILL_CALLS).unwrap();
    let result = kill_job(never_installed().as_str(), Budget::Unbounded.start(), reaper);
    (result, script)
}

/// SIGKILL to the job by label, in the system domain; the signal is named so a failure of the job
/// is what launchd records, whatever `restart` then does.
#[skuld::test]
fn kill_sends_sigkill_to_the_job_in_the_system_domain() {
    let id = never_installed();
    let (result, script) = kill_answered(DONE);

    assert!(matches!(result, Ok(Some(()))), "{result:?}");
    let log = std::fs::read_to_string(script.dir.path().join("log")).unwrap();
    assert_eq!(log, format!("kill SIGKILL system/{id}\n"));
}

/// The two ways launchd says there is no process to signal: the job is not loaded (113), or it is
/// loaded and has no process (3, "No such process"). Classified off the kill's own answer — a
/// separate `print` first would leave a window for the job to exit between the two.
#[skuld::test]
fn kill_of_a_job_with_no_process_is_not_running() {
    for code in ["exit 3", NOT_LOADED] {
        let id = never_installed();
        let (result, script) = kill_answered(code);

        let e = result.expect_err(code);
        assert!(
            matches!(&e, Error::NotRunning { id: named } if named == id.as_str()),
            "{code}: {e:?}"
        );
        script.assert_driven("a kill classified off its own answer, with no read before it");
    }
}

/// Any other refusal is a failure of the request, with launchctl's words.
#[skuld::test]
fn kill_refused_for_any_other_reason_is_a_plain_failure() {
    let (result, _script) = kill_answered("echo 'Operation not permitted' >&2; exit 1");

    let e = result.expect_err("refused");
    assert!(
        matches!(&e, Error::CommandFailed { stderr, .. } if stderr.contains("Operation not permitted")),
        "{e:?}"
    );
}

// A changed spec over a loaded job ====================================================================================

/// A changed spec over a job launchd has loaded is refused before anything is written, whether or
/// not the job has a process, and is installed once the job is not loaded. The stand-in answers
/// `print` only: a `bootout` of the loaded job would be an unscripted call and fail the test.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_changed_spec_is_refused_while_the_job_is_loaded() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-refuse-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let mut spec = DaemonSpec {
        id: Id::try_from(name.as_str()).unwrap(),
        name: name.clone(),
        command: vec!["/bin/sleep".to_string(), "300".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    };

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    spec.command.push("updated".to_string());
    for loaded in [RUNNING, NOT_RUNNING] {
        let script = Scripted::new(&[("print", loaded)]);
        let refused = mgr.install(&spec, false).expect_err("a changed spec over a loaded job");
        assert!(
            refused.to_string().contains(&format!("goetia daemon stop {name}")),
            "{refused}"
        );
        script.assert_driven("install over a loaded job sends nothing but the read");
        assert_eq!(
            std::fs::read_to_string(staging_path(&name)).unwrap(),
            installed,
            "a refused install left the plist as it was"
        );
    }

    let script = Scripted::new(&[("print", NOT_LOADED), ("print", NOT_LOADED)]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_driven("install once the job is not loaded");
    assert_ne!(std::fs::read_to_string(staging_path(&name)).unwrap(), installed);
}

/// The spec the tests below install, under a name of their own: the tests run on parallel threads of
/// one process, so each needs plists no other writes.
fn spec_named(name: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(name).unwrap(),
        name: name.to_owned(),
        command: vec!["/bin/sleep".to_string(), "300".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

/// A `print` that fails for a reason other than "no such job" does not say the job is not loaded, so
/// `install` over an installed plist and over none, and `start`, fail on it instead of acting as if
/// the job were absent. Each stand-in answers `print` only: a write under a loaded job, a `bootout`
/// or a `bootstrap` would be an unscripted call, and `assert_driven` would see it.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_print_that_fails_otherwise_is_an_error_not_a_job_that_is_not_loaded() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-unreadable-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let mut spec = spec_named(&name);
    let names_print =
        |e: &Error| matches!(e, Error::CommandFailed { command, .. } if command.starts_with("launchctl print"));

    // `Create`: the stale-job check, which comes before anything is written.
    let script = Scripted::new(&[("print", UNREADABLE)]);
    let e = mgr.install(&spec, false).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_driven("create, unreadable print");
    assert!(
        !staging_path(&name).exists(),
        "a create that could not tell whether a job is loaded wrote nothing"
    );

    // `Create` over a loaded job whose `bootout` fails: nothing written either, so a rerun is a
    // `Create` again and reaches the stale job.
    let script = Scripted::new(&[("print", DONE), ("bootout", REFUSED)]);
    let e = mgr.install(&spec, false).expect_err("a refused bootout");
    assert!(matches!(e, Error::CommandFailed { .. }), "{e:?}");
    script.assert_driven("create, refused bootout");
    assert!(
        !staging_path(&name).exists(),
        "a create whose stale job could not be booted out wrote nothing"
    );

    let script = Scripted::new(&INSTALL_LONGEST);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("create over a loaded job");

    let script = Scripted::new(&[("print", UNREADABLE)]);
    let e = mgr.start(&spec.id, Budget::DEFAULT).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_driven("start, unreadable first read");

    let script = Scripted::new(&[("print", NOT_LOADED), ("bootstrap", REFUSED), ("print", UNREADABLE)]);
    let e = mgr.start(&spec.id, Budget::DEFAULT).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_driven("start, unreadable read after a refused bootstrap");

    // `Update`: the refusal check. The plist exists from the rerun above.
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();
    spec.command.push("updated".to_string());
    let script = Scripted::new(&[("print", UNREADABLE)]);
    let e = mgr.install(&spec, false).expect_err("an unreadable print");
    assert!(names_print(&e), "{e:?}");
    script.assert_driven("update, unreadable print");
    assert_eq!(
        std::fs::read_to_string(staging_path(&name)).unwrap(),
        installed,
        "an install that could not tell whether the job is loaded left the plist as it was"
    );
}

/// After an exit-`4` install a rerun is refused, not read as `UpToDate`.
fn assert_restored_and_rerun_refused(mgr: &LaunchdManager, spec: &DaemonSpec, name: &str, installed: &str) {
    assert_eq!(
        std::fs::read_to_string(staging_path(name)).unwrap(),
        installed,
        "the previous plist was put back"
    );
    let script = Scripted::new(&[("print", RUNNING)]);
    let refused = mgr.install(spec, false).expect_err("the rerun, job loaded");
    assert!(
        matches!(refused, Error::Other(_)) && refused.to_string().contains(&format!("goetia daemon stop {name}")),
        "a refusal, not an exit-4 or an up-to-date: {refused}"
    );
    script.assert_driven("the rerun reads the job and is refused");
}

/// A job that is not loaded when `install` checks but is loaded once it has written, because someone
/// else started it between the two, may have read either plist: the install is in doubt
/// ([`Error::LoadedJobInDoubt`], exit `4`), and the previous plist is put back.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_job_loaded_between_the_check_and_the_write_leaves_the_install_in_doubt() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-indoubt-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let mut spec = spec_named(&name);

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    spec.command.push("updated".to_string());
    let script = Scripted::new(&[("print", NOT_LOADED), ("print", RUNNING)]);
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
    script.assert_driven("install, job loaded after the write");
    assert_restored_and_rerun_refused(&mgr, &spec, &name, &installed);
}

/// The recheck after the write is a read like the first; one that fails leaves the loaded state
/// unknown, so the install is in doubt (exit `4`), not a plain failure (exit `1`), and the previous
/// plist is put back.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn an_unreadable_recheck_after_the_write_leaves_the_install_in_doubt() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-recheck-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let mut spec = spec_named(&name);

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    spec.command.push("updated".to_string());
    let script = Scripted::new(&[("print", NOT_LOADED), ("print", UNREADABLE)]);
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
    assert!(e.to_string().contains("Input/output error"), "{e}");
    script.assert_driven("install, unreadable recheck");
    assert_restored_and_rerun_refused(&mgr, &spec, &name, &installed);
}

/// What launchd reads decides the refusal, not the spec: a spec change the plist does not render
/// (`name` is in the embedded metadata only) is written over a loaded job without a single
/// `launchctl` call, and the loaded job is as the new plist would have it.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_spec_change_the_plist_does_not_render_is_written_over_a_loaded_job() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-samerender-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let mut spec = spec_named(&name);

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");
    let installed = std::fs::read_to_string(staging_path(&name)).unwrap();

    spec.name = "another name".to_string();
    let script = Scripted::new(&[]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_driven("an install whose rendering is unchanged asks launchd nothing");
    assert_ne!(
        std::fs::read_to_string(staging_path(&name)).unwrap(),
        installed,
        "the metadata comment was refreshed"
    );
}

// A put-back, and a Create, that races ================================================================================

/// Installs a spec, then a changed one whose read after the write first runs `during` (as a shell
/// snippet, standing for something else on the host) and then answers "loaded". Returns that install's
/// error and the text of the changed plist it wrote.
fn install_loaded_after_write(mgr: &LaunchdManager, name: &str, during: &str) -> (Error, String) {
    let mut spec = spec_named(name);
    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");

    spec.command.push("updated".to_string());
    let changed = generate::plist(
        &spec,
        &Identity {
            user: "root".to_string(),
        },
    );
    let recheck = format!("{during}; {RUNNING}");
    let script = Scripted::new(&[("print", NOT_LOADED), ("print", &recheck)]);
    let e = mgr.install(&spec, false).expect_err("loaded after the write");
    script.assert_driven("install, job loaded after the write");
    (e, changed)
}

/// The failed put-back of an install that ended in doubt, or a panic.
fn restore_failure_of(e: &Error) -> &str {
    match e {
        Error::LoadedJobInDoubt {
            restore_failed: Some(detail),
            ..
        } => detail,
        other => panic!("expected an exit-4 error whose put-back failed: {other:?}"),
    }
}

/// The put-back writes only over what this install wrote: a concurrent install that wrote after it
/// owns the path, and a put-back over it would destroy that write.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_put_back_does_not_overwrite_a_plist_that_is_no_longer_ours() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-putback-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let target = staging_path(&name);

    let (e, _) = install_loaded_after_write(&mgr, &name, &format!("echo 'someone else' > '{}'", target.display()));

    assert!(restore_failure_of(&e).contains("no longer holds"), "{e}");
    assert!(e.to_string().contains(&target.display().to_string()), "{e}");
    assert!(!e.to_string().contains("plist is back as it was"), "{e}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else\n");
}

/// A concurrent `enable` moved the plist to `ENABLED_DIR` before the put-back: writing the previous
/// plist back to staging would leave one in both directories, which `locate` refuses for every verb.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_put_back_does_not_recreate_a_plist_that_was_enabled_meanwhile() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-putback-enabled-{:x}", std::process::id());
    let _plists = Plists(name.clone());

    let during = format!(
        "mv '{}' '{}'",
        staging_path(&name).display(),
        enabled_path(&name).display()
    );
    let (e, changed) = install_loaded_after_write(&mgr, &name, &during);

    assert!(restore_failure_of(&e).contains("no longer"), "{e}");
    assert!(!staging_path(&name).exists(), "the put-back recreated the staged plist");
    assert_eq!(std::fs::read_to_string(enabled_path(&name)).unwrap(), changed);
}

/// A concurrent `uninstall` removed the plist before the put-back: writing the previous one back would
/// resurrect a daemon that was just uninstalled.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_put_back_does_not_resurrect_a_plist_that_was_uninstalled_meanwhile() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-putback-removed-{:x}", std::process::id());
    let _plists = Plists(name.clone());

    let (e, _) = install_loaded_after_write(&mgr, &name, &format!("rm '{}'", staging_path(&name).display()));

    assert!(restore_failure_of(&e).contains("no longer"), "{e}");
    assert!(!staging_path(&name).exists(), "the put-back resurrected the plist");
}

/// A plist from an older release is `Stale`, and `Stale` is refused over a loaded job exactly when
/// its rendering differs from what would be written: a release that renders the same text only
/// refreshes the metadata, without asking launchd anything.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_stale_plist_is_refused_over_a_loaded_job_only_when_its_rendering_differs() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-stale-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let spec = spec_named(&name);

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");
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
    let script = Scripted::new(&[("print", RUNNING)]);
    let refused = mgr
        .install(&spec, false)
        .expect_err("a stale plist that renders differently");
    assert!(
        matches!(refused, Error::Other(_)) && refused.to_string().contains(&format!("goetia daemon stop {name}")),
        "{refused}"
    );
    script.assert_driven("the stale plist's rendering differs, so launchd is asked");
    assert_eq!(
        std::fs::read_to_string(staging_path(&name)).unwrap(),
        rendered_differently,
        "a refused install left the plist as it was"
    );

    std::fs::write(staging_path(&name), &older).unwrap();
    assert!(matches!(mgr.preview_install(&spec), Ok(Outcome::Stale { .. })));
    let script = Scripted::new(&[]);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Stale { .. })));
    script.assert_driven("a stale plist that renders the same asks launchd nothing");
    assert_eq!(std::fs::read_to_string(staging_path(&name)).unwrap(), installed);
}

/// A `Create` that fails before it has a plist to write has not stopped the job under its label: the
/// stale job is booted out only once the plist is staged, so a rerun that fails the same way leaves
/// the predecessor running.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_create_that_cannot_stage_its_plist_boots_nothing_out() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-unstaged-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, "").unwrap();
    let mut spec = spec_named(&name);
    spec.cwd = Some(blocker.join("cwd"));

    let script = Scripted::new(&[]);
    let e = mgr.install(&spec, false).expect_err("a cwd that cannot be created");
    assert!(matches!(e, Error::Io { .. }), "{e:?}");
    script.assert_driven("a create that cannot stage asks launchd nothing");
    assert!(!staging_path(&name).exists());
}

/// A job loaded between a `Create`'s check and its write is the job the plist was written under, which
/// `start` would find loaded and running: the install is in doubt (exit `4`), the plist it created is
/// removed, and the rerun is a `Create` again, not `UpToDate`. Reading the job back fails the same way.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_job_loaded_after_a_create_leaves_the_install_in_doubt_with_no_plist() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-create-indoubt-{:x}", std::process::id());
    let _plists = Plists(name.clone());
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
        let script = Scripted::new(&calls);
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
        script.assert_driven(what);
        assert!(!staging_path(&name).exists(), "{what}: the created plist was left");
    }

    let script = Scripted::new(&[("print", NOT_LOADED), ("print", UNREADABLE)]);
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
    script.assert_driven("create, unreadable recheck");
    assert!(!staging_path(&name).exists(), "the created plist was left");

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the rerun is a create again");
}

/// A plist appears at the target while the stale job's `bootout` runs. A request has been sent, so the
/// install does not go on to re-derive into further `launchctl` calls: it reports that the job was
/// booted out and what is at the path (exit `4`), and leaves what is there.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_plist_taken_after_the_bootout_is_reported_without_further_launchctl_calls() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-taken-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let spec = spec_named(&name);
    let target = staging_path(&name);

    let take = format!("echo 'someone else' > '{}'; exit 0", target.display());
    let script = Scripted::new(&[("print", DONE), ("bootout", &take)]);
    let e = mgr
        .install(&spec, false)
        .expect_err("the plist was taken after the bootout");
    assert!(matches!(e, Error::StaleJobBootedOut { .. }), "{e:?}");
    assert!(e.to_string().contains("booted out"), "{e}");
    assert!(e.to_string().contains(&target.display().to_string()), "{e}");
    script.assert_driven("nothing is sent after the race");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else\n");
}

/// The same race after a read only: nothing was sent and nothing written, but `install` does not
/// re-derive into a second pass, whose reads the verb did not reserve. It fails (exit `1`) and says to
/// run it again, and leaves what is at the path.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_plist_taken_after_a_read_fails_the_install_without_further_launchctl_calls() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-taken-early-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let spec = spec_named(&name);
    let target = staging_path(&name);

    let take = format!("echo 'someone else' > '{}'; exit 113", target.display());
    let script = Scripted::new(&[("print", &take)]);
    let e = mgr.install(&spec, false).expect_err("the plist was taken after a read");
    assert!(matches!(e, Error::Other(_)), "{e:?}");
    assert!(e.to_string().contains("re-run"), "{e}");
    assert!(e.to_string().contains(&target.display().to_string()), "{e}");
    script.assert_driven("nothing is sent after the race");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else\n");
}

/// The plist is removed (a concurrent `uninstall`) after the read that found no job loaded, while a
/// changed spec is being written over it. A call has been made, so `install` does not re-derive: a
/// second pass is a `Create` over what may be a job loaded meanwhile, which needs more calls than the
/// verb reserved. It fails (exit `1`) and writes nothing.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_plist_removed_after_a_read_fails_the_install_without_further_launchctl_calls() {
    let mgr = LaunchdManager::new();
    let name = format!("goetia-vanished-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let mut spec = spec_named(&name);
    let target = staging_path(&name);

    let script = Scripted::new(&CREATED);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Create)));
    script.assert_driven("the first install");

    spec.command.push("updated".to_string());
    let remove = format!("rm '{}'; exit 113", target.display());
    let script = Scripted::new(&[("print", &remove)]);
    let e = mgr
        .install(&spec, false)
        .expect_err("the plist was removed after a read");
    assert!(matches!(e, Error::Other(_)), "{e:?}");
    assert!(e.to_string().contains("re-run"), "{e}");
    script.assert_driven("nothing is sent after the vanish");
    assert!(!target.exists(), "the install wrote a plist over the vanished one");
}

/// `install --dry-run` and `diff` read no job state, and `install --start` over a loaded job with a
/// changed rendering is refused before it bootstraps anything: the stand-in answers one `print`, so
/// any other call fails the test.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn dry_run_and_diff_ask_launchd_nothing_and_install_start_is_refused_before_it_starts() {
    use crate::cli::wait::WaitArgs;
    use crate::cli::{diff, install};

    let name = format!("goetia-cli-{:x}", std::process::id());
    let _plists = Plists(name.clone());
    let dir = tempfile::tempdir().unwrap();
    let manifest = |seconds: &str| {
        std::fs::write(
            dir.path().join("goetia.yaml"),
            format!("daemons:\n  {name}:\n    command: [/bin/sleep, \"{seconds}\"]\n"),
        )
        .unwrap();
    };
    let get_manager = || -> Result<Box<dyn ServiceManager>> { Ok(Box::new(LaunchdManager::new())) };
    let run_install = |start: bool, dry_run: bool| {
        let args = install::Args {
            ids: vec![],
            file: dir.path().to_path_buf(),
            force: false,
            start,
            enable: false,
            dry_run,
            wait: WaitArgs::default(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = install::run(&args, &get_manager, &|| true, &mut out, &mut err);
        (code, String::from_utf8(err).unwrap())
    };

    manifest("300");
    let script = Scripted::new(&CREATED);
    assert_eq!(run_install(false, false).0, 0);
    script.assert_driven("the first install");

    manifest("299");
    let script = Scripted::new(&[]);
    assert_eq!(run_install(false, true).0, 0, "install --dry-run");
    let args = diff::Args {
        ids: vec![],
        file: dir.path().to_path_buf(),
    };
    let code = diff::run(&args, &get_manager, &mut Vec::new(), &mut Vec::new());
    assert_eq!(code, 3, "diff reports the drift");
    script.assert_driven("neither --dry-run nor diff asks launchd");

    let script = Scripted::new(&[("print", RUNNING)]);
    let (code, err) = run_install(true, false);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("goetia daemon stop {name}")), "{err}");
    script.assert_driven("install --start is refused after one read, before any bootstrap");
}
