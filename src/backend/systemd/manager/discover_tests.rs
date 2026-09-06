//! Unit tests for the errors `discover.rs`'s read paths produce when a read that was supposed to
//! say whether anything is at an id fails.
//!
//! Deliberately not exercised here: `EACCES`, the case that motivated [`Error::Undetermined`]. It
//! needs a uid that is not root, and CI runs every test binary elevated (see `.github/workflows/
//! ci.yaml`), so a permission test in this binary would either fail there or have to be gated into
//! a silent skip. `tests/systemd_integration/linux.rs`'s
//! `status_reports_undetermined_for_an_unreadable_dropin_directory` seeds it as root and reads it
//! back through `runuser -u nobody`, which is correct under both uids.

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

#[skuld::test]
fn an_unreadable_dropin_directory_is_undetermined_not_other() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = unreadable_dir(tmp.path());

    let err = dropin_marker_in("x", &dir).expect_err("ENOTDIR is not a readable drop-in directory");

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

    let err = dropin_marker_in("some-daemon", &dir).expect_err("ENOTDIR");

    assert!(
        err.to_string().contains("some-daemon"),
        "a report that names no id sends the reader looking through four search directories: {err}"
    );
}
