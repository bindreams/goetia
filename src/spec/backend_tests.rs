use std::time::Duration;

use super::{BackendKind, Builtin, MAX_SC_ACTION_DELAY, Shaped, ShapedSpec, windows_builtin, windows_only_account};
use crate::error::Error;
use crate::spec::field::Field;
use crate::spec::interpolate::Substitution;
use crate::spec::overrides::Supplied;
use crate::spec::test_support::{abs, assert_no_run_of_spaces};
use crate::spec::{AccountId, DaemonKind, Id, Protocol, Restart, StopSignal, User};

fn parse(yaml: &str) -> Result<BackendKind, yaml_serde::Error> {
    yaml_serde::from_str(yaml)
}

#[skuld::test]
fn backend_keys_are_the_lowercase_backend_names() {
    assert_eq!(parse("systemd").unwrap(), BackendKind::Systemd);
    assert_eq!(parse("launchd").unwrap(), BackendKind::Launchd);
    assert_eq!(parse("scm").unwrap(), BackendKind::Scm);
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
    assert_eq!(BackendKind::ALL.len(), 3, "ALL should have one entry per variant");

    for backend in BackendKind::ALL {
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
        BackendKind::native().is_some(),
        crate::manager::native().is_ok(),
        "BackendKind::native() and manager::native() must agree on which platforms have a service manager"
    );
}

// Fixtures ============================================================================================================

/// A `ShapedSpec` with every field at an unremarkable default: `Protocol` and
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
        daemon_type: Shaped::Absent,
        protocol: Shaped::Absent,
        schedule: Shaped::Absent,
        stop_signal: Shaped::Absent,
        stop_timeout: Shaped::Absent,
    }
}

// `BackendKind::warn` =================================================================================================

#[skuld::test]
fn each_backend_emits_only_its_own_warnings() {
    let mut spec = shaped_spec();
    spec.protocol = Shaped::Parsed(Protocol::Native);
    spec.cwd = Some(abs("data"));
    spec.restart_delay = Shaped::Parsed(Duration::from_millis(500));

    let mut launchd_warnings = Vec::new();
    BackendKind::Launchd.warn(&spec, &mut launchd_warnings);
    assert_eq!(launchd_warnings.len(), 1, "launchd: {launchd_warnings:?}");
    assert!(launchd_warnings[0].message.contains("not a whole number of seconds"));

    let mut scm_warnings = Vec::new();
    BackendKind::Scm.warn(&spec, &mut scm_warnings);
    assert_eq!(scm_warnings.len(), 1, "scm: {scm_warnings:?}");
    assert!(scm_warnings[0].message.contains("System32"));

    let mut systemd_warnings = Vec::new();
    BackendKind::Systemd.warn(&spec, &mut systemd_warnings);
    assert!(systemd_warnings.is_empty(), "systemd: {systemd_warnings:?}");
}

#[skuld::test]
fn an_over_long_delay_warns_from_the_scm_arm() {
    let mut spec = shaped_spec();
    spec.protocol = Shaped::Parsed(Protocol::Native);
    spec.restart = Shaped::Parsed(Restart::OnFailure);
    spec.restart_delay = Shaped::Parsed(MAX_SC_ACTION_DELAY + Duration::from_millis(1));

    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&spec, &mut warnings);

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
fn the_scm_delay_advisory_is_gated_on_restart_alone() {
    let over_long = MAX_SC_ACTION_DELAY + Duration::from_millis(1);

    let mut simple = shaped_spec();
    simple.protocol = Shaped::Parsed(Protocol::Plain);
    simple.restart = Shaped::Parsed(Restart::OnFailure);
    simple.restart_delay = Shaped::Parsed(over_long);
    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&simple, &mut warnings);
    assert_eq!(
        warnings.len(),
        1,
        "Protocol::Plain clamps like managed, so it warns: {warnings:?}"
    );

    let mut never_restarts = shaped_spec();
    never_restarts.protocol = Shaped::Parsed(Protocol::Native);
    never_restarts.restart = Shaped::Parsed(Restart::Never);
    never_restarts.restart_delay = Shaped::Parsed(over_long);
    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&never_restarts, &mut warnings);
    assert!(warnings.is_empty(), "Restart::Never must not warn: {warnings:?}");
}

#[skuld::test]
fn a_sub_millisecond_delay_warns_from_the_scm_arm_with_the_rounded_value() {
    let mut spec = shaped_spec();
    spec.protocol = Shaped::Parsed(Protocol::Native);
    spec.restart = Shaped::Parsed(Restart::OnFailure);
    spec.restart_delay = Shaped::Parsed(Duration::from_micros(1500));

    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&spec, &mut warnings);

    let [warning] = warnings.as_slice() else {
        panic!("expected exactly one warning, got {warnings:?}");
    };
    assert!(
        warning.message.contains("restart-delay") && warning.message.contains("2ms"),
        "warning should name the field and the rounded value: {}",
        warning.message
    );
}

#[skuld::test]
fn the_sub_millisecond_advisory_is_silent_when_nothing_is_rounded_or_nothing_restarts() {
    let mut whole = shaped_spec();
    whole.restart = Shaped::Parsed(Restart::OnFailure);
    whole.restart_delay = Shaped::Parsed(Duration::from_millis(7));
    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&whole, &mut warnings);
    assert!(warnings.is_empty(), "a whole-ms delay must not warn: {warnings:?}");

    let mut never_restarts = shaped_spec();
    never_restarts.restart = Shaped::Parsed(Restart::Never);
    never_restarts.restart_delay = Shaped::Parsed(Duration::from_micros(1500));
    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&never_restarts, &mut warnings);
    assert!(warnings.is_empty(), "Restart::Never must not warn: {warnings:?}");

    // Clamped, not rounded: the over-long advisory is the only one.
    let mut over_long = shaped_spec();
    over_long.restart = Shaped::Parsed(Restart::OnFailure);
    over_long.restart_delay = Shaped::Parsed(MAX_SC_ACTION_DELAY + Duration::from_nanos(1));
    let mut warnings = Vec::new();
    BackendKind::Scm.warn(&over_long, &mut warnings);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].message.contains("clamped"), "{warnings:?}");
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

/// Guards the carve-out's keying: a future refactor that makes
/// `windows_only_account` filter the *folded* form (instead of the
/// original `name`) would make `NT AUTHORITY\SYSTEM` fold toward `system`
/// and get carved out too, silently stopping it from being rejected under
/// `systemd`/`launchd`. Written as direct literal assertions rather than
/// transitively through `windows_builtin`, so it cannot pass by both sides
/// drifting together.
#[skuld::test]
fn windows_only_account_keys_the_system_carveout_on_the_original_string() {
    assert_eq!(windows_only_account(r"NT AUTHORITY\SYSTEM"), Some(Builtin::LocalSystem));
    assert_eq!(windows_only_account("system"), None);
}

// `windows_builtin` ===================================================================================================

/// Every qualified spelling, enumerated. The row this test exists for —
/// `NT AUTHORITY\LocalSystem` — went missing precisely because nothing
/// enumerated them: `origin/main`'s `identity::account_needs_password`
/// stripped the prefix before matching, so it had that row, and the table
/// that replaced it silently did not.
#[skuld::test]
fn every_nt_authority_spelling_is_recognised() {
    for (name, builtin) in [
        (r"NT AUTHORITY\SYSTEM", Builtin::LocalSystem),
        (r"NT AUTHORITY\LocalSystem", Builtin::LocalSystem),
        (r"NT AUTHORITY\LocalService", Builtin::LocalService),
        (r"NT AUTHORITY\NetworkService", Builtin::NetworkService),
        // The spellings `LookupAccountSidW` returns for the well-known SIDs.
        (r"NT AUTHORITY\LOCAL SERVICE", Builtin::LocalService),
        (r"NT AUTHORITY\NETWORK SERVICE", Builtin::NetworkService),
    ] {
        assert_eq!(windows_builtin(name), Some(builtin), "`{name}`");
        // Case-insensitive, and qualified: unlike the bare `system`
        // carve-out, every one of these can only ever name a Windows
        // account.
        assert_eq!(
            windows_builtin(&name.to_ascii_lowercase()),
            Some(builtin),
            "`{name}` lowercased"
        );
        assert_eq!(windows_only_account(name), Some(builtin), "`{name}` is Windows-only");
    }
}

#[skuld::test]
fn a_spelling_that_is_not_in_the_table_is_an_ordinary_account() {
    // The table is enumerated, not folded: interior spacing is not a way to
    // spell a built-in, so a typo fails as a nonexistent account instead of
    // silently becoming LocalSystem. `Local Service` unqualified is the
    // case the enumeration exists to protect — a real local account with
    // that name — and it is why the two spaced rows above are qualified.
    for name in [
        r"NT AUTHORITY\S Y S T E M",
        r"NT AUTHORITY\LO CAL SERVICE",
        r"NT AUTHORITY\ localservice",
        "nt authority\\localservice ",
        r"NT AUTHORITY\LOCAL SYSTEM",
        "Local Service",
        "Network Service",
    ] {
        assert_eq!(windows_builtin(name), None, "`{name}`");
    }
}

// `BackendKind::error` ================================================================================================

#[skuld::test]
fn a_templated_user_name_is_not_checked_against_the_builtin_table() {
    let mut spec = shaped_spec();
    let supplied = Supplied {
        user: true,
        ..Supplied::NONE
    };

    spec.user = User::Name("${ACCT}".to_string());
    assert!(
        BackendKind::Systemd
            .error(&spec, &supplied, Substitution::Pending)
            .is_ok(),
        "a `$`-bearing name cannot be classified from this host and must not be guessed at"
    );

    spec.user = User::Name("LocalService".to_string());
    assert!(
        BackendKind::Systemd
            .error(&spec, &supplied, Substitution::Pending)
            .is_err(),
        "the rule still fires for a value that needs no resolution"
    );

    assert!(
        BackendKind::Systemd
            .error(&spec, &Supplied::NONE, Substitution::Pending)
            .is_ok(),
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
    assert!(BackendKind::Scm.error(&spec, &supplied, Substitution::Pending).is_err());
    assert!(
        BackendKind::Scm
            .error(&spec, &Supplied::NONE, Substitution::Pending)
            .is_ok()
    );
}

#[skuld::test]
fn a_sid_is_rejected_under_a_posix_backend() {
    let mut spec = shaped_spec();
    spec.user = User::Id(AccountId::Sid("S-1-5-19".to_string()));
    let supplied = Supplied {
        user: true,
        ..Supplied::NONE
    };
    assert!(
        BackendKind::Systemd
            .error(&spec, &supplied, Substitution::Pending)
            .is_err()
    );
    assert!(
        BackendKind::Launchd
            .error(&spec, &supplied, Substitution::Pending)
            .is_err()
    );
    assert!(
        BackendKind::Scm.error(&spec, &supplied, Substitution::Pending).is_ok(),
        "a SID is a Windows account"
    );
}

// systemd's `Environment=` names ======================================================================================

/// A spec whose `env` holds `key`. Its `command[0]` has no backslash, which a Windows `abs` would write
/// and a systemd host refuses.
fn shaped_with_env_key(key: &str) -> ShapedSpec {
    let mut spec = shaped_spec();
    spec.command = Some(vec!["/bin/frpc".to_string()]);
    spec.env.insert(key.to_string(), "v".to_string());
    spec
}

#[skuld::test]
fn systemd_refuses_an_env_key_it_would_drop_and_the_others_do_not() {
    for key in ["MY-VAR", "1ST", "a.b", "é", "a b"] {
        let spec = shaped_with_env_key(key);
        let refused = BackendKind::Systemd
            .refuse(&spec)
            .expect_err(&format!("{key:?} must be refused"));
        assert_eq!(refused.field, Some(Field::EnvKey(key.into())));
        let message = Error::from(refused).to_string();
        assert!(message.contains(&format!("{key:?}")), "{message}");
        assert!(message.contains("Environment="), "{message}");
        assert_no_run_of_spaces(&message);
        assert!(BackendKind::Launchd.refuse(&spec).is_ok(), "{key:?}");
        assert!(BackendKind::Scm.refuse(&spec).is_ok(), "{key:?}");

        let mut warnings = Vec::new();
        BackendKind::Systemd.warn(&spec, &mut warnings);
        assert!(
            warnings.is_empty(),
            "the refusal is systemd's whole answer: {warnings:?}"
        );
    }
    for key in ["PATH", "_x", "a1_B"] {
        assert!(
            BackendKind::Systemd.refuse(&shaped_with_env_key(key)).is_ok(),
            "{key:?}"
        );
    }
}

// `stop-action` =======================================================================================================

fn with_stop(signal: Shaped<StopSignal>, timeout: Shaped<Option<Duration>>) -> ShapedSpec {
    ShapedSpec {
        stop_signal: signal,
        stop_timeout: timeout,
        ..shaped_spec()
    }
}

fn supplies_signal() -> Supplied {
    Supplied {
        stop_signal: true,
        ..Supplied::NONE
    }
}

fn supplies_timeout() -> Supplied {
    Supplied {
        stop_timeout: true,
        ..Supplied::NONE
    }
}

#[skuld::test]
fn the_native_backend_refuses_a_signal_it_cannot_send_wherever_the_manifest_wrote_it() {
    let quit = with_stop(Shaped::Parsed(StopSignal::Quit), Shaped::Absent);
    let refused = BackendKind::Launchd.refuse(&quit).unwrap_err();
    assert_eq!(refused.field, Some(Field::StopSignal));
    assert!(
        Error::from(refused)
            .to_string()
            .contains("`send-signal: quit` is refused on launchd")
    );
    assert!(BackendKind::Systemd.refuse(&quit).is_ok());
    assert!(BackendKind::Scm.refuse(&quit).is_ok());

    let ctrl_c = with_stop(Shaped::Parsed(StopSignal::CtrlC), Shaped::Absent);
    assert!(BackendKind::Systemd.refuse(&ctrl_c).is_err());
    assert!(BackendKind::Launchd.refuse(&ctrl_c).is_err());
    assert!(BackendKind::Scm.refuse(&ctrl_c).is_ok());

    let native = with_stop(Shaped::Parsed("sigusr1".parse::<StopSignal>().unwrap()), Shaped::Absent);
    assert!(BackendKind::Systemd.refuse(&native).is_ok());
    assert!(BackendKind::Launchd.refuse(&native).is_err());
    assert!(BackendKind::Scm.refuse(&native).is_err());
}

#[skuld::test]
fn an_override_is_judged_only_for_the_leaves_it_wrote() {
    let quit = with_stop(Shaped::Parsed(StopSignal::Quit), Shaped::Absent);
    assert!(
        BackendKind::Launchd
            .error(&quit, &supplies_signal(), Substitution::Pending)
            .is_err()
    );
    assert!(
        BackendKind::Launchd
            .error(&quit, &Supplied::NONE, Substitution::Pending)
            .is_ok(),
        "a base value is nobody's override"
    );
}

#[skuld::test]
fn a_signal_still_holding_a_substitution_is_not_judged() {
    for kind in [Shaped::Deferred, Shaped::Absent] {
        let spec = with_stop(kind, Shaped::Absent);
        for backend in BackendKind::ALL {
            assert!(backend.refuse(&spec).is_ok(), "{backend}");
        }
    }
}

#[skuld::test]
fn windows_refuses_a_stop_action_on_a_task() {
    let task = |signal, timeout| ShapedSpec {
        daemon_type: Shaped::Parsed(DaemonKind::Task),
        ..with_stop(signal, timeout)
    };
    let timeout = Shaped::Parsed(Some(Duration::from_secs(5)));

    let refused = BackendKind::Scm
        .refuse(&task(Shaped::Parsed(StopSignal::Quit), Shaped::Absent))
        .unwrap_err();
    assert_eq!(refused.field, Some(Field::StopSignal));
    let message = Error::from(refused).to_string();
    assert!(
        message.contains("Task Scheduler") && message.contains("backend-specific"),
        "{message}"
    );

    let refused = BackendKind::Scm.refuse(&task(Shaped::Absent, timeout)).unwrap_err();
    assert_eq!(refused.field, Some(Field::StopTimeout));

    assert!(BackendKind::Scm.refuse(&task(Shaped::Absent, Shaped::Absent)).is_ok());
    // A leaf written as the default is not a request.
    let thirty = || Shaped::Parsed(Some(Duration::from_secs(30)));
    assert!(
        BackendKind::Scm
            .refuse(&task(Shaped::Parsed(StopSignal::Terminate), thirty()))
            .is_ok()
    );
    assert!(BackendKind::Scm.refuse(&task(Shaped::Absent, thirty())).is_ok());
    // The default timeout of `kill` is none, so `null` beside it asks for nothing more.
    let refused = BackendKind::Scm.refuse(&task(Shaped::Parsed(StopSignal::Kill), Shaped::Parsed(None)));
    assert_eq!(refused.unwrap_err().field, Some(Field::StopSignal));
    // systemd and launchd stop a task as they stop a service.
    for backend in [BackendKind::Systemd, BackendKind::Launchd] {
        assert!(
            backend
                .refuse(&task(Shaped::Parsed(StopSignal::Terminate), thirty()))
                .is_ok()
        );
    }
}

#[skuld::test]
fn launchd_refuses_a_wait_over_its_cap_and_a_wait_without_a_limit() {
    let seconds = |n| Shaped::Parsed(Some(Duration::from_secs(n)));
    for timeout in [seconds(61), seconds(86_400), Shaped::Parsed(None)] {
        let refused = BackendKind::Launchd
            .refuse(&with_stop(Shaped::Parsed(StopSignal::Terminate), timeout))
            .unwrap_err();
        assert_eq!(refused.field, Some(Field::StopTimeout));
        assert!(Error::from(refused).to_string().contains("60 s"));
    }
    // An absent signal is `terminate`, so `null` beside it is a wait without a limit too.
    assert!(
        BackendKind::Launchd
            .refuse(&with_stop(Shaped::Absent, Shaped::Parsed(None)))
            .is_err()
    );
    for (signal, timeout) in [
        (Shaped::Parsed(StopSignal::Terminate), seconds(60)),
        (Shaped::Parsed(StopSignal::Terminate), seconds(1)),
        (Shaped::Absent, Shaped::Absent),
        // `kill` waits for nothing: no timeout is its default.
        (Shaped::Parsed(StopSignal::Kill), Shaped::Parsed(None)),
        (Shaped::Parsed(StopSignal::Kill), Shaped::Absent),
        // A signal a `$` still holds may be `kill`: a later pass judges the `null`.
        (Shaped::Deferred, Shaped::Parsed(None)),
        (Shaped::Parsed(StopSignal::Terminate), Shaped::Deferred),
    ] {
        assert!(BackendKind::Launchd.refuse(&with_stop(signal, timeout)).is_ok());
    }
    // The other backends have no such cap.
    for backend in [BackendKind::Systemd, BackendKind::Scm] {
        assert!(backend.refuse(&with_stop(Shaped::Absent, seconds(86_400))).is_ok());
    }
}

#[skuld::test]
fn launchd_judges_only_the_timeout_an_override_wrote() {
    let long = with_stop(Shaped::Absent, Shaped::Parsed(Some(Duration::from_secs(90))));
    assert!(
        BackendKind::Launchd
            .error(&long, &Supplied::NONE, Substitution::Pending)
            .is_ok()
    );
    let refused = BackendKind::Launchd
        .error(&long, &supplies_timeout(), Substitution::Pending)
        .unwrap_err();
    assert_eq!(refused.field, Some(Field::StopTimeout));
}

#[skuld::test]
fn windows_refuses_a_signal_for_a_native_service_and_keeps_its_timeout() {
    let native = |signal, timeout| ShapedSpec {
        protocol: Shaped::Parsed(Protocol::Native),
        ..with_stop(signal, timeout)
    };
    assert!(
        BackendKind::Scm
            .refuse(&native(Shaped::Parsed(StopSignal::Quit), Shaped::Absent))
            .is_err()
    );
    assert!(
        BackendKind::Scm
            .refuse(&native(Shaped::Parsed(StopSignal::Terminate), Shaped::Absent))
            .is_ok()
    );
    assert!(
        BackendKind::Scm
            .refuse(&native(Shaped::Absent, Shaped::Parsed(Some(Duration::from_secs(5)))))
            .is_ok()
    );
    let plain = ShapedSpec {
        protocol: Shaped::Parsed(Protocol::Plain),
        ..with_stop(Shaped::Parsed(StopSignal::CtrlC), Shaped::Parsed(None))
    };
    assert!(BackendKind::Scm.refuse(&plain).is_ok());
}

#[skuld::test]
fn windows_waits_for_the_type_and_protocol_before_judging_them() {
    let deferred_type = ShapedSpec {
        daemon_type: Shaped::Deferred,
        ..with_stop(Shaped::Parsed(StopSignal::Terminate), Shaped::Absent)
    };
    assert!(BackendKind::Scm.refuse(&deferred_type).is_ok());
    let deferred_protocol = ShapedSpec {
        protocol: Shaped::Deferred,
        ..with_stop(Shaped::Parsed(StopSignal::Terminate), Shaped::Absent)
    };
    assert!(BackendKind::Scm.refuse(&deferred_protocol).is_ok());
}
