//! Unit tests for the reads `discover.rs` performs and for the error class each of them earns.
//!
//! Deliberately not exercised here: `EACCES`, the case that motivated [`Error::Undetermined`]. It
//! needs a uid that is not root, and CI runs every test binary elevated (see `.github/workflows/
//! ci.yaml`), so a permission test in this binary would either fail there or have to be gated into
//! a silent skip. `tests/systemd_integration/linux.rs` seeds those as root and reads them back
//! through `runuser -u nobody`, which is correct under both uids:
//! `status_reports_undetermined_for_a_dropin_directory_it_cannot_read` for the absence path,
//! `status_reports_undetermined_for_a_fragment_it_cannot_read` for the fragment itself, and
//! `diff_refuses_our_own_fragment_with_an_unreadable_dropin_without_denying_ownership` for the
//! presence path.

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

/// The masked-unit case, confirmed by `is_symlink` rather than inferred from "the open failed and
/// the `lstat` did not". The target deliberately does not exist: `systemctl mask` points the link at
/// `/dev/null`, and either way the link itself is never followed.
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

/// The three kinds of drop-in directory `systemd.unit(5)` reads for one unit name. The truncations
/// go longest-first, matching the man page's "further down the prefix hierarchy overrides further
/// up".
#[skuld::test]
fn every_drop_in_directory_name_systemd_reads_is_generated() {
    assert_eq!(
        dropin_dir_names("foo-bar-baz"),
        [
            "foo-bar-baz.service.d",
            "foo-bar-.service.d",
            "foo-.service.d",
            "service.d"
        ]
    );
    assert_eq!(dropin_dir_names("frpc"), ["frpc.service.d", "service.d"]);
}

/// An id may both start and end with a dash (`^[A-Za-z0-9._-]{1,80}$`), and truncating after the
/// final one reproduces the id itself — which must name one directory, not two.
#[skuld::test]
fn a_trailing_dash_does_not_name_the_same_directory_twice() {
    let names = dropin_dir_names("foo-");
    assert_eq!(names, ["foo-.service.d", "service.d"]);

    let names = dropin_dir_names("-foo-");
    assert_eq!(names, ["-foo-.service.d", "-.service.d", "service.d"]);
}

/// [`residue`] asks for `<id>.service.d` alone: the other two kinds are named for a family of units
/// rather than for this id, so they are drift on an installed unit but never occupancy of an empty
/// one — and [`residue_recovery`] tells a human to delete what it names.
#[skuld::test]
fn only_the_id_specific_directory_counts_as_this_ids_occupancy() {
    let names = dropin_dir_names("goetia-daemon");
    assert_eq!(names[0], "goetia-daemon.service.d");
    assert!(
        names[1..].iter().all(|name| !name.starts_with("goetia-daemon.")),
        "everything past the first is a family name, not this id's: {names:?}"
    );
}
