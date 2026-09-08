//! Unit tests for the reads `discover.rs` performs and for the error class each of them earns.
//!
//! Deliberately not exercised here: `EACCES`, the case that motivated [`Error::Undetermined`]. It
//! needs a uid that is not root, and CI runs every test binary elevated (see `.github/workflows/
//! ci.yaml`), so a permission test in this binary would either fail there or have to be gated into
//! a silent skip. `tests/systemd_integration/linux.rs` seeds those as root and reads them back
//! through `runuser -u nobody`, which is correct under both uids:
//! `status_reports_undetermined_for_a_dropin_directory_it_cannot_read` for the absence path,
//! `status_reports_undetermined_for_a_fragment_it_cannot_read` and
//! `unelevated_list_reports_a_root_only_unit_as_undetermined` for the fragment itself, and
//! `diff_refuses_our_own_fragment_with_an_unreadable_dropin` for the presence path.

use std::io;

use super::*;

/// `ENOTDIR`: a regular file where a directory component has to be. Not `NotFound`, not
/// `PermissionDenied`, and identical for root and everyone else — which is what makes it the right
/// probe for the "every non-`NotFound` failure" half of [`undetermined`]'s rule.
fn unreadable_dir(tmp: &Path) -> PathBuf {
    let file = tmp.join("not-a-directory");
    fs::write(&file, "").expect("write the blocking file");
    file.join("x.service.d")
}

fn mkfifo(path: &Path) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a temp path with no NUL");
    // SAFETY: `c_path` is a NUL-terminated pointer valid for the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), io::Error::last_os_error());
}

/// Run `f` on its own thread and report — rather than hang — if it never returns.
///
/// The bound is a failure bound on a kernel wait that may genuinely never end: `open(2)` on a FIFO
/// with `O_RDONLY` blocks until a writer arrives, and the tests below never create one. It
/// synchronizes nothing. A correct classification is one `openat` and a send, so no passing run's
/// outcome depends on the bound's value — only how long a regression takes to be *reported* instead
/// of wedging the whole suite, which is what an unguarded call would do.
fn without_blocking<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || drop(tx.send(f())));
    rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap_or_else(|_| {
        panic!("{what} never returned: it opened the FIFO for reading, which blocks until a writer arrives")
    })
}

// The error class a failed read earns ---------------------------------------------------------------------------------

#[skuld::test]
fn an_unreadable_dropin_directory_is_undetermined_not_other() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = unreadable_dir(tmp.path());

    let failure = dropin_marker_in(&dir).expect_err("ENOTDIR is not a readable drop-in directory");
    let err = failure.undetermined("x");

    match &err {
        Error::Undetermined { id, reason, .. } => {
            assert_eq!(id, "x");
            assert!(
                reason.contains(&dir.display().to_string()),
                "the reason must name the path that could not be read: {reason}"
            );
        }
        other => panic!("`Error::Other` here reaches `status` as `unreadable`, which claims ownership: {other:?}"),
    }
    assert!(
        !err.to_string().contains("uninstall"),
        "goetia has established nothing about this id, so it cannot advise destroying it: {err}"
    );
}

/// The reader itself commits to neither class: the absence path and the fragment-present path both
/// meet this exact failure and answer it differently, so a [`ReadFailure`] that already said
/// "cannot determine whether `x` is installed" would have made one of the two wrong.
#[skuld::test]
fn a_read_failure_names_the_path_without_claiming_anything_about_the_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = unreadable_dir(tmp.path());

    let detail = dropin_marker_in(&dir).expect_err("ENOTDIR").detail();

    assert!(
        detail.contains(&dir.display().to_string()),
        "the detail must name the path that could not be read: {detail}"
    );
    assert!(
        !detail.contains("determine") && !detail.contains("installed"),
        "the reader states the failure; the caller states what it means: {detail}"
    );
}

/// The one thing the errno decides. Both branches are still
/// [`Error::Undetermined`] — the ownership claim is wrong for either.
#[skuld::test]
fn only_a_permission_denial_recommends_elevation() {
    let path = Path::new("/etc/systemd/system/x.service.d");

    for (source, wants_elevation) in [
        (io::Error::from(io::ErrorKind::PermissionDenied), true),
        (io::Error::from(io::ErrorKind::Other), false),
    ] {
        let rendered = format!("{source:?}");
        let Error::Undetermined { reason, recovery, .. } = undetermined("x", "read", path, &source) else {
            panic!("every non-`NotFound` failure is undetermined, not just a permission denial");
        };
        assert!(reason.contains("x.service.d"), "{rendered}: {reason}");
        assert_eq!(
            recovery.contains("re-run as root"),
            wants_elevation,
            "{rendered}: {recovery}"
        );
        assert!(!recovery.contains("uninstall"), "{rendered}: {recovery}");
    }
}

/// The `.wants` link is the other artifact `residue` stats, and it sweeps four unit-load roots, so
/// the id it names has to survive the trip out of the loop.
#[skuld::test]
fn the_error_names_the_id_that_could_not_be_determined() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = unreadable_dir(tmp.path());

    let err = dropin_marker_in(&dir).expect_err("ENOTDIR").undetermined("some-daemon");

    assert!(
        err.to_string().contains("some-daemon"),
        "a report that names no id sends the reader looking through every search directory: {err}"
    );
}

// classify_and_read ---------------------------------------------------------------------------------------------------

/// The masked-unit case. `O_PATH | O_NOFOLLOW` is the documented combination that returns a
/// descriptor for the *symlink itself* rather than failing, so the link is classified without being
/// followed. One target exists and one does not: `systemctl mask` points the link at `/dev/null`,
/// and either way what the link points at is never opened.
#[skuld::test]
fn a_symlink_is_non_regular_whether_or_not_its_target_exists() {
    let tmp = tempfile::tempdir().expect("tempdir");

    for (name, target) in [("masked", "/dev/null"), ("dangling", "/nonexistent/goetia-target")] {
        let link = tmp.path().join(name);
        std::os::unix::fs::symlink(target, &link).expect("plant the symlink");

        let state = classify_and_read(&link).unwrap_or_else(|e| panic!("{name}: {}", e.detail()));
        assert!(
            matches!(state, RawState::NonRegular),
            "{name}: a symlink is never read through"
        );
    }
}

#[skuld::test]
fn a_missing_path_is_absent_and_a_regular_file_is_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("x.service");

    let absent = classify_and_read(&path).expect("a missing path is not a failed read");
    assert!(matches!(absent, RawState::Absent));

    fs::write(&path, "[Unit]\n").expect("write the fragment");
    let present = classify_and_read(&path).unwrap_or_else(|e| panic!("{}", e.detail()));
    match present {
        RawState::Regular(text) => assert_eq!(text, "[Unit]\n"),
        _ => panic!("a regular file is read, not classified away"),
    }
}

#[skuld::test]
fn a_directory_is_non_regular() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("x.service.d");
    fs::create_dir(&dir).expect("seed the drop-in directory");

    let state = classify_and_read(&dir).unwrap_or_else(|e| panic!("{}", e.detail()));
    assert!(
        matches!(state, RawState::NonRegular),
        "a directory is classified, never opened for reading"
    );
}

/// A FIFO is the case that makes classifying before opening for reading mandatory rather than
/// tidy: `open(2)` on one with `O_RDONLY` blocks until a writer arrives, and none ever will here.
/// `list` sweeps every `*.service` name in `/etc/systemd/system`, so a single `mkfifo x.service`
/// would otherwise wedge the listing for the whole host.
#[skuld::test]
fn a_fifo_is_non_regular_without_blocking() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fifo = tmp.path().join("x.service");
    mkfifo(&fifo);

    let state = without_blocking("classify_and_read", move || classify_and_read(&fifo))
        .unwrap_or_else(|e| panic!("{}", e.detail()));
    assert!(
        matches!(state, RawState::NonRegular),
        "a FIFO is classified, never opened for reading"
    );
}

/// The bytes of a regular file could not be obtained — the only shape of failure a *readable-type*
/// artifact can produce, and the one arm of it that is identical for root and everyone else. The
/// permission-denied shape needs a second uid and is exercised end to end in
/// `tests/systemd_integration/linux.rs`; see this module's own doc comment.
#[skuld::test]
fn invalid_utf8_in_a_regular_file_is_a_read_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("x.service");
    fs::write(&path, [b'[', 0xff, 0xfe, b']']).expect("write the undecodable fragment");

    let Err(failure) = classify_and_read(&path) else {
        panic!("bytes that are not UTF-8 are not text goetia read, so this settles nothing about the id");
    };
    let detail = failure.detail();
    assert!(
        detail.contains(&path.display().to_string()),
        "the detail must name the path that could not be read: {detail}"
    );
}

/// `ENOTDIR` on a parent component: presence itself was never established, so neither
/// [`RawState::Absent`] ("nothing is here") nor [`RawState::NonRegular`] ("something is, and it is
/// not a fragment") is a claim this arm may make.
#[skuld::test]
fn a_path_that_cannot_be_resolved_is_a_read_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = unreadable_dir(tmp.path()).join("x.service");

    let Err(failure) = classify_and_read(&path) else {
        panic!("`ENOTDIR` establishes neither that something is at the path nor that nothing is");
    };
    assert!(
        failure.detail().contains(&path.display().to_string()),
        "{}",
        failure.detail()
    );
}

// The drop-in search path ---------------------------------------------------------------------------------------------

/// `systemd.unit(5)`'s "System Unit Search Path", verbatim and in its order. Pinned as a whole
/// rather than spot-checked: a root left out is invisible drift — goetia reports an id clean while
/// systemd applies configuration to it — and no other test in the suite can notice a missing entry.
#[skuld::test]
fn the_search_path_is_the_man_pages_own_list_in_its_own_order() {
    assert_eq!(
        DROPIN_SEARCH_DIRS,
        [
            "/etc/systemd/system.control",
            "/run/systemd/system.control",
            "/run/systemd/transient",
            "/run/systemd/generator.early",
            "/etc/systemd/system",
            "/etc/systemd/system.attached",
            "/run/systemd/system",
            "/run/systemd/system.attached",
            "/run/systemd/generator",
            "/usr/local/lib/systemd/system",
            "/usr/lib/systemd/system",
            "/run/systemd/generator.late",
        ]
    );
    let control = DROPIN_SEARCH_DIRS
        .iter()
        .position(|d| *d == "/etc/systemd/system.control")
        .expect("the root `systemctl set-property` writes into");
    let etc = DROPIN_SEARCH_DIRS
        .iter()
        .position(|d| *d == UNIT_DIR)
        .expect("goetia's own root");
    assert!(
        control < etc,
        "`system.control` overrides `{UNIT_DIR}`, so it is searched first"
    );
}

/// An enablement link can only be written where a unit *directory* is; the generated, transient and
/// dbus-created roots hold no `*.wants` a `systemctl enable` or a package preset could have put
/// there.
#[skuld::test]
fn the_wants_sweep_skips_the_roots_that_cannot_hold_a_link() {
    for dir in WANTS_SEARCH_DIRS {
        assert!(
            DROPIN_SEARCH_DIRS.contains(&dir),
            "{dir} is not even a unit search directory"
        );
    }
    for excluded in [
        "/etc/systemd/system.control",
        "/run/systemd/transient",
        "/run/systemd/generator",
    ] {
        assert!(
            !WANTS_SEARCH_DIRS.contains(&excluded),
            "{excluded} cannot hold an enablement link"
        );
    }
}

/// The one directory name both the drift scan and the occupancy scan ask for. Systemd reads more —
/// `my-.service.d` for `my-daemon.service`, and the top-level `service.d` for every service unit —
/// and the module doc comment says why goetia deliberately does not. Pinned here because the cheap
/// mistake is to "complete" that list without noticing that `Outcome::Conflict` is what the extra
/// directories would produce, and that `--force` cannot clear one.
#[skuld::test]
fn the_scan_asks_for_this_ids_own_directory_and_no_family_wide_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["my-.service.d", "service.d"] {
        let dir = tmp.path().join(name);
        fs::create_dir(&dir).expect("seed a family-wide drop-in directory");
        fs::write(dir.join("50-x.conf"), "[Service]\nMemoryMax=8G\n").expect("write drop-in");
    }
    let own = tmp.path().join("my-daemon.service.d");
    fs::create_dir(&own).expect("seed this id's own drop-in directory");
    fs::write(own.join("override.conf"), "[Service]\nMemoryMax=1G\n").expect("write drop-in");

    // `dropin_dirs` sweeps absolute roots, so the name-building rule is exercised through the one
    // reader both it and `residue` share.
    let marker = dropin_marker_in(&own).expect("read this id's drop-in directory");
    assert!(marker.contains("MemoryMax=1G"), "{marker}");
    assert!(
        !marker.contains("MemoryMax=8G"),
        "a family-wide directory is a sibling of this id's, never part of it: {marker}"
    );
}
