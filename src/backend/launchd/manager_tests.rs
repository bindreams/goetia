//! Unit coverage for the pieces of the launchd backend that neither need
//! root nor the real `/Library/...` system paths: `write_new`/
//! `write_existing` take an arbitrary target path, so the non-clobbering
//! race and the `plutil -lint` gate are both fully testable against a
//! `tempfile::tempdir()`. Everything that genuinely needs elevation and the
//! real filesystem locations (`locate`/`discover`, `enable`/`disable`,
//! `start`/`stop` against real launchd) lives in
//! `tests/launchd_integration.rs` instead.

use super::*;
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

// write_new / write_existing ==========================================================================================

#[skuld::test]
fn write_new_creates_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("fresh.plist");
    let content = sample_plist("fresh");

    let result = write_new(&target, &content).expect("write_new over an absent target");
    assert!(matches!(result, WriteNew::Written));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
}

/// The mechanism `install`'s "create must not clobber" obligation rests on:
/// by the time `write_new` runs, discovery already classified the target as
/// absent, so anything present now (a foreign plist, or a concurrent
/// installer's) arrived after that classification — exactly the shape of
/// the TOCTOU race a plain unconditional `rename` would lose. Simulating
/// "something is already there" directly (rather than trying to force a
/// genuine two-thread race, which no amount of `Barrier` synchronization
/// can *guarantee* lands inside a window of a few instructions) reproduces
/// the one fact that matters: `write_new` must never overwrite it.
#[skuld::test]
fn write_new_does_not_clobber_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("raced.plist");
    std::fs::write(&target, "not a goetia artifact\n").unwrap();

    let result = write_new(&target, &sample_plist("raced")).expect("write_new over a raced target");
    assert!(matches!(result, WriteNew::Raced));
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

/// The same TOCTOU class `write_new` guards against for `install`'s create
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

// find_field ==========================================================================================================

#[skuld::test]
fn find_field_extracts_a_launchctl_print_style_line() {
    let text = "system/foo = {\n\tstate = running\n\tpid = 4242\n}\n";
    assert_eq!(find_field(text, "state"), Some("running"));
    assert_eq!(find_field(text, "pid"), Some("4242"));
}

#[skuld::test]
fn find_field_returns_none_for_a_missing_key() {
    let text = "system/foo = {\n\tstate = running\n}\n";
    assert_eq!(find_field(text, "pid"), None);
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

/// A symlink to a regular plist still resolves and is still read — unchanged from before this
/// classification step existed, and deliberately unlike the systemd backend, where `systemctl mask`
/// makes the symlink itself the meaningful artifact. See [`obtain`]'s doc comment.
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

/// `ENABLED_DIR` is `/Library/LaunchDaemons` — every vendor's daemons, not goetia's — and
/// `plutil -convert binary1` and `defaults write` produce a binary plist by default. Treating those
/// bytes as undetermined would give a macOS host a permanent, unclearable `undetermined` entry, and
/// a permanent exit `4`, for a service goetia has no business reporting on at all.
///
/// `list` reaches the same verdict through the same [`classify`], which is what keeps the two from
/// describing one file differently; its own end is
/// `tests/launchd_integration/launchd.rs::list_stays_clean_on_a_host_carrying_binary_plists`.
#[skuld::test]
fn a_binary_plist_is_treated_as_foreign_not_undetermined() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vendor.plist");
    let mut bytes = b"bplist00".to_vec();
    bytes.extend_from_slice(&[0xd1, 0x01, 0x02, 0x5f, 0x10, 0x00, 0xff]);
    fs::write(&path, &bytes).expect("write the binary plist");

    assert!(matches!(classify(bytes), Classified::BinaryPlist));

    let text = read_artifact(&path, "vendor").expect("a binary plist is identified, not a read that failed");
    assert!(
        generate::extract(&text).expect("empty text decodes cleanly").is_none(),
        "a format goetia never emits carries no goetia marker, so every caller must refuse it as foreign"
    );
}

#[skuld::test]
fn non_utf8_bytes_that_are_not_a_binary_plist_are_undetermined() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("corrupt.plist");
    fs::write(&path, [b'<', 0xff, 0xfe, b'>']).expect("write the undecodable plist");

    let err = read_artifact(&path, "corrupt").expect_err("nothing was identified in these bytes");

    let Error::Undetermined { reason, .. } = &err else {
        panic!("corruption of one of goetia's own cannot be ruled out, so nothing may be claimed: {err:?}");
    };
    assert!(
        reason.contains(&path.display().to_string()),
        "the reason must name the path: {reason}"
    );
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
