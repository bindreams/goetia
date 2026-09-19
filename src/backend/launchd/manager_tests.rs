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
            matches!(classify(bytes), Classified::NotOurs),
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

/// The mid-pass fault, and the reason `list` no longer propagates one: a dirent that could not be
/// read must not take down the ids the very same pass already named — nor, since the two
/// directories are scanned in one call, a directory that was read to the end before it. Injected
/// through `collect_plists`' iterator because no real `readdir` fails on request.
#[skuld::test]
fn a_dirent_that_cannot_be_read_keeps_what_the_scan_already_named() {
    let dir = Path::new(ENABLED_DIR);
    let entries = vec![
        Ok(dir.join("named-before-the-fault.plist")),
        Err(io::Error::other("injected mid-scan failure")),
        Ok(dir.join("never-reached.plist")),
    ];

    let scan = collect_plists(ENABLED_DIR, entries.into_iter());

    assert_eq!(
        scan.plists.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["named-before-the-fault"],
        "what the pass named before the fault survives it, and the pass stops there: {scan:?}"
    );
    match &scan.incomplete {
        Some(Installed::Undetermined { name, reason }) => {
            assert_eq!(*name, None, "the pass cannot name what it never reached");
            assert!(reason.contains("injected mid-scan failure"), "{reason}");
            assert!(
                reason.contains(ENABLED_DIR),
                "the reason must name what was being scanned: {reason}"
            );
        }
        other => panic!("a pass that stopped early must say so: {other:?}"),
    }
}

/// The same fault one syscall earlier. An `Err` here reaches the CLI as `Kind::Unavailable` — exit
/// `1` over an empty document, which is `list` reporting a populated host as having nothing on it.
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
    assert!(
        matches!(&scan.incomplete, Some(Installed::Undetermined { name: None, .. })),
        "{scan:?}"
    );
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
/// or the wait failing — may have reached launchd: [`Error::RequestInDoubt`], exit `4`. A read
/// changes nothing, so the same failure there is a plain one. A failure placed before the spawn is a
/// plain one for both. Injected, so no `launchctl` runs.
#[skuld::test]
fn a_launchctl_request_that_may_have_run_is_in_doubt() {
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
        assert!(matches!(e, Error::RequestInDoubt { .. }), "{e:?}");

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

/// `install`'s longest path, [`INSTALL_CALLS`] calls: its read, and the `bootout` of the job it
/// finds loaded.
const INSTALL_LONGEST: [(&str, &str); INSTALL_CALLS] = [("print", DONE), ("bootout", DONE)];

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
    let script = Scripted::new(&INSTALL_LONGEST);
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    script.assert_driven("install, updating a loaded job");

    // `restart`: the stop and the start under one reservation, spent exactly.
    let script = Scripted::new(&then(&[&BOOTOUT, &START_LONGEST]));
    let prepared = mgr.prepare(&[Step::Stop, Step::Start], Budget::DEFAULT).unwrap();
    mgr.stop(&id, Budget::DEFAULT).expect("restart's stop");
    mgr.start(&id, Budget::DEFAULT).expect("restart's start");
    exhausted();
    drop(prepared);
    script.assert_driven("restart");

    // `install --start`: the install and the start under one reservation, spent exactly.
    spec.command.push("again".to_string());
    let script = Scripted::new(&then(&[&INSTALL_LONGEST, &START_LONGEST]));
    let prepared = mgr.prepare(&[Step::Install, Step::Start], Budget::DEFAULT).unwrap();
    assert!(matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })));
    mgr.start(&id, Budget::DEFAULT).expect("install --start's start");
    exhausted();
    drop(prepared);
    script.assert_driven("install --start");

    let script = Scripted::new(&BOOTOUT);
    mgr.uninstall(&id).expect("uninstall");
    script.assert_driven("uninstall");
    assert!(!staging_path(&name).exists(), "uninstall left the plist");
}
