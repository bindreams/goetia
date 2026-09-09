use std::path::PathBuf;
use std::time::Duration;

use super::{Backend, Builtin, MAX_SC_ACTION_DELAY, Shaped, ShapedSpec, windows_builtin, windows_only_account};
use crate::spec::overrides::Supplied;
use crate::spec::{AccountId, Id, Kind, Restart, User};

fn parse(yaml: &str) -> Result<Backend, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(yaml)
}

#[skuld::test]
fn backend_keys_are_the_lowercase_backend_names() {
    assert_eq!(parse("systemd").unwrap(), Backend::Systemd);
    assert_eq!(parse("launchd").unwrap(), Backend::Launchd);
    assert_eq!(parse("scm").unwrap(), Backend::Scm);
}

#[skuld::test]
fn unknown_backend_key_names_the_valid_set() {
    let err = parse("windwos").unwrap_err();
    let msg = err.to_string();

    assert!(msg.contains("windwos"), "should name the bad value: {msg}");
    assert!(
        msg.contains("launchd"),
        "should name `launchd` as a valid option: {msg}"
    );
    assert!(msg.contains("scm"), "should name `scm` as a valid option: {msg}");
    assert!(
        msg.contains("systemd"),
        "should name `systemd` as a valid option: {msg}"
    );
}

#[skuld::test]
fn backend_keys_are_case_sensitive() {
    assert!(parse("Scm").is_err(), "`Scm` is not the lowercase key `scm`");
}

#[skuld::test]
fn all_round_trips_through_as_str() {
    assert_eq!(Backend::ALL.len(), 3, "ALL should have one entry per variant");

    for backend in Backend::ALL {
        assert_eq!(
            parse(backend.as_str()).unwrap(),
            backend,
            "as_str() of {backend:?} should deserialize back to itself"
        );
    }
}

#[skuld::test]
fn native_backend_agrees_with_the_platforms_that_have_a_service_manager() {
    assert_eq!(
        Backend::native().is_some(),
        crate::manager::native().is_ok(),
        "Backend::native() and manager::native() must agree on which platforms have a service manager"
    );
}

// Fixtures ============================================================================================================

#[cfg(windows)]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\{p}"))
}
#[cfg(not(windows))]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!("/{p}"))
}

/// A `ShapedSpec` with every field at an unremarkable default: `Kind` and
/// `Restart` absent, no `cwd`/`logs`, no `restart_delay`. Tests override the
/// one or two fields they care about.
fn shaped_spec() -> ShapedSpec {
    ShapedSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "frpc".to_string(),
        command: Some(vec![abs("bin/frpc").to_string_lossy().into_owned()]),
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Shaped::Absent,
        restart_delay: Shaped::Absent,
        logs: None,
        kind: Shaped::Absent,
    }
}

// `Backend::warn` =====================================================================================================

#[skuld::test]
fn each_backend_emits_only_its_own_warnings() {
    let mut spec = shaped_spec();
    spec.kind = Shaped::Parsed(Kind::Managed);
    spec.cwd = Some(abs("data"));
    spec.restart_delay = Shaped::Parsed(Duration::from_millis(500));

    let mut launchd_warnings = Vec::new();
    Backend::Launchd.warn(&spec, &mut launchd_warnings);
    assert_eq!(launchd_warnings.len(), 1, "launchd: {launchd_warnings:?}");
    assert!(launchd_warnings[0].message.contains("not a whole number of seconds"));

    let mut scm_warnings = Vec::new();
    Backend::Scm.warn(&spec, &mut scm_warnings);
    assert_eq!(scm_warnings.len(), 1, "scm: {scm_warnings:?}");
    assert!(scm_warnings[0].message.contains("System32"));

    let mut systemd_warnings = Vec::new();
    Backend::Systemd.warn(&spec, &mut systemd_warnings);
    assert!(systemd_warnings.is_empty(), "systemd: {systemd_warnings:?}");
}

#[skuld::test]
fn an_over_long_delay_warns_from_the_scm_arm() {
    let mut spec = shaped_spec();
    spec.kind = Shaped::Parsed(Kind::Managed);
    spec.restart = Shaped::Parsed(Restart::OnFailure);
    spec.restart_delay = Shaped::Parsed(MAX_SC_ACTION_DELAY + Duration::from_millis(1));

    let mut warnings = Vec::new();
    Backend::Scm.warn(&spec, &mut warnings);

    let [warning] = warnings.as_slice() else {
        panic!("expected exactly one warning, got {warnings:?}");
    };
    assert!(
        warning.message.contains("restart-delay"),
        "warning should name the field: {}",
        warning.message
    );
}

#[skuld::test]
fn the_scm_delay_advisory_is_gated_on_kind_and_restart() {
    let over_long = MAX_SC_ACTION_DELAY + Duration::from_millis(1);

    let mut simple = shaped_spec();
    simple.kind = Shaped::Parsed(Kind::Simple);
    simple.restart = Shaped::Parsed(Restart::OnFailure);
    simple.restart_delay = Shaped::Parsed(over_long);
    let mut warnings = Vec::new();
    Backend::Scm.warn(&simple, &mut warnings);
    assert!(warnings.is_empty(), "Kind::Simple must not warn: {warnings:?}");

    let mut never_restarts = shaped_spec();
    never_restarts.kind = Shaped::Parsed(Kind::Managed);
    never_restarts.restart = Shaped::Parsed(Restart::Never);
    never_restarts.restart_delay = Shaped::Parsed(over_long);
    let mut warnings = Vec::new();
    Backend::Scm.warn(&never_restarts, &mut warnings);
    assert!(warnings.is_empty(), "Restart::Never must not warn: {warnings:?}");
}

// `windows_only_account` ==============================================================================================

#[skuld::test]
fn a_bare_system_name_is_not_a_windows_only_account() {
    for name in ["system", "System", "SYSTEM"] {
        assert_eq!(
            windows_only_account(name),
            None,
            "`{name}` is a legal POSIX username and must not be windows-only"
        );
        assert_eq!(
            windows_builtin(name),
            Some(Builtin::LocalSystem),
            "`{name}` is still a recognised built-in spelling"
        );
    }

    assert_eq!(
        windows_only_account(r"NT AUTHORITY\SYSTEM"),
        windows_builtin(r"NT AUTHORITY\SYSTEM"),
        "the qualified spelling is unambiguous and stays windows-only"
    );
    assert_eq!(windows_builtin(r"NT AUTHORITY\SYSTEM"), Some(Builtin::LocalSystem));

    assert_eq!(windows_only_account("LocalService"), windows_builtin("LocalService"),);
    assert_eq!(windows_builtin("LocalService"), Some(Builtin::LocalService));
}

// `Backend::error` ====================================================================================================

#[skuld::test]
fn a_templated_user_name_is_not_checked_against_the_builtin_table() {
    let mut spec = shaped_spec();
    let supplied = Supplied {
        user: true,
        ..Supplied::NONE
    };

    spec.user = User::Name("${ACCT}".to_string());
    assert!(
        Backend::Systemd.error(&spec, supplied).is_ok(),
        "a `$`-bearing name cannot be classified from this host and must not be guessed at"
    );

    spec.user = User::Name("LocalService".to_string());
    assert!(
        Backend::Systemd.error(&spec, supplied).is_err(),
        "the rule still fires for a value that needs no resolution"
    );

    assert!(
        Backend::Systemd.error(&spec, Supplied::NONE).is_ok(),
        "`Supplied::NONE` means this value came from the base spec, not the override, and must not be rejected"
    );
}

#[skuld::test]
fn a_numeric_uid_is_rejected_under_scm() {
    let mut spec = shaped_spec();
    spec.user = User::Id(AccountId::Uid(1000));
    let supplied = Supplied {
        user: true,
        ..Supplied::NONE
    };
    assert!(Backend::Scm.error(&spec, supplied).is_err());
    assert!(Backend::Scm.error(&spec, Supplied::NONE).is_ok());
}

#[skuld::test]
fn a_sid_is_rejected_under_a_posix_backend() {
    let mut spec = shaped_spec();
    spec.user = User::Id(AccountId::Sid("S-1-5-19".to_string()));
    let supplied = Supplied {
        user: true,
        ..Supplied::NONE
    };
    assert!(Backend::Systemd.error(&spec, supplied).is_err());
    assert!(Backend::Launchd.error(&spec, supplied).is_err());
    assert!(
        Backend::Scm.error(&spec, supplied).is_ok(),
        "a SID is a Windows account"
    );
}
