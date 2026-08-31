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
