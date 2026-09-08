//! Pure unit tests: no real SCM/registry access, so these run unelevated
//! alongside every other `#[skuld::test]` in the library. Elevated behavior
//! (everything that actually talks to SCM) is covered by
//! `tests/scm_integration.rs` instead.

use std::path::PathBuf;

use super::*;

fn dummy_reg(account: Option<&str>) -> ScmRegistration {
    ScmRegistration {
        name: "goetia-test".to_string(),
        display_name: "Goetia Test".to_string(),
        executable: PathBuf::from(r"C:\bin\test.exe"),
        arguments: vec![],
        account: account.map(str::to_string),
        failure_actions: None,
        parameters: BTreeMap::new(),
    }
}

#[skuld::test]
fn map_state_maps_running_and_stopped_directly() {
    assert_eq!(map_state(WinState::Running), State::Running);
    assert_eq!(map_state(WinState::Stopped), State::Stopped);
}

#[skuld::test]
fn map_state_maps_pending_and_paused_to_unknown() {
    for s in [
        WinState::StartPending,
        WinState::StopPending,
        WinState::ContinuePending,
        WinState::PausePending,
        WinState::Paused,
    ] {
        assert_eq!(map_state(s), State::Unknown, "{s:?} should map to Unknown");
    }
}

#[skuld::test]
fn create_maps_no_account_to_null_for_local_system() {
    // `CreateServiceW(NULL)` means LocalSystem -- `windows-service` passes
    // `account_name: None` through as null, which is exactly what's wanted
    // here. `current_start_type: None` signals "create".
    let info = service_info(&dummy_reg(None), None, None);
    assert_eq!(info.account_name, None);
    assert_eq!(info.start_type, ServiceStartType::OnDemand);
}

#[skuld::test]
fn update_spells_out_local_system_explicitly() {
    // `ChangeServiceConfigW(NULL)` means "leave the account unchanged", not
    // LocalSystem -- see `service_info`'s doc comment. A desired
    // `LocalSystem` on the update path must therefore be the literal string.
    let info = service_info(&dummy_reg(None), None, Some(ServiceStartType::OnDemand));
    assert_eq!(info.account_name.as_deref(), Some(std::ffi::OsStr::new("LocalSystem")));
}

#[skuld::test]
fn a_real_account_is_passed_through_on_both_paths() {
    for current_start_type in [None, Some(ServiceStartType::OnDemand)] {
        let info = service_info(&dummy_reg(Some(r".\svc-account")), None, current_start_type);
        assert_eq!(
            info.account_name.as_deref(),
            Some(std::ffi::OsStr::new(r".\svc-account")),
            "current_start_type={current_start_type:?}"
        );
    }
}

#[skuld::test]
fn password_is_only_attached_when_given() {
    let with_password = service_info(&dummy_reg(Some("svc")), Some("hunter2".to_string()), None);
    assert_eq!(
        with_password.account_password.as_deref(),
        Some(std::ffi::OsStr::new("hunter2"))
    );

    let without_password = service_info(&dummy_reg(Some("svc")), None, None);
    assert_eq!(without_password.account_password, None);
}

#[skuld::test]
fn create_always_uses_demand_start() {
    let info = service_info(&dummy_reg(None), None, None);
    assert_eq!(info.start_type, ServiceStartType::OnDemand);
}

#[skuld::test]
fn update_preserves_whatever_start_type_is_already_live() {
    // A routine spec-driven update must not silently undo a prior `enable`
    // (SERVICE_AUTO_START) or `disable` (SERVICE_DEMAND_START) -- `windows-service`'s
    // `change_config` always sends a real `dwStartType`, never
    // `SERVICE_NO_CHANGE`, so `service_info` must restate whatever
    // `current_start_type` already says.
    for start_type in [ServiceStartType::OnDemand, ServiceStartType::AutoStart] {
        let info = service_info(&dummy_reg(None), None, Some(start_type));
        assert_eq!(info.start_type, start_type);
    }
}

#[skuld::test]
fn is_not_found_matches_only_error_service_does_not_exist() {
    let not_found = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1060));
    assert!(is_not_found(&not_found));

    let access_denied = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(5));
    assert!(!is_not_found(&access_denied));
}

// undetermined ========================================================================================================

/// The substance `manager::fake`'s own `Error::Undetermined` carries, asserted of this backend's
/// constructor — as `the_launchd_undetermined_recovery_names_both_causes_and_not_uninstall` asserts
/// it of launchd's. The three describe one condition and must agree about it without sharing a
/// function. Split across the cause rather than crammed into one sentence — elevation is advice only
/// a permission boundary earns, and offering it for a failing disk sends the reader somewhere
/// useless — so "both causes" means the pair covers both, one each. Never `uninstall`, in any of
/// them: that is `Outcome::RefuseUnreadable`'s remedy, and it certifies the ownership this read
/// never established.
#[skuld::test]
fn the_scm_undetermined_recovery_names_both_causes_and_not_uninstall() {
    // 1117 is ERROR_IO_DEVICE: a read that did not complete for a reason no amount of privilege
    // fixes, so the class must not be keyed on the denial even though the advice is.
    for (raw, wants_elevation) in [(ERROR_ACCESS_DENIED as i32, true), (1117, false)] {
        let e = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(raw));
        let Error::Undetermined { id, reason, recovery } = service_undetermined("x", "open service `x`", &e) else {
            panic!("{raw}: every non-`is_not_found` failure is undetermined, not just a denial");
        };

        assert_eq!(id, "x");
        assert!(reason.contains("open service `x`"), "{raw}: {reason}");
        assert!(recovery.contains("re-run"), "{raw}: {recovery}");
        assert_eq!(
            recovery.contains("re-run as Administrator"),
            wants_elevation,
            "{raw}: {recovery}"
        );
        assert!(
            !recovery.contains("uninstall"),
            "{raw}: uninstall certifies ownership this read never established: {recovery}"
        );
    }
}

#[skuld::test]
fn is_access_denied_matches_only_error_access_denied() {
    let denied = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
    assert!(is_access_denied(&denied));

    let not_found = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1060));
    assert!(!is_access_denied(&not_found));

    assert!(!is_access_denied(&windows_service::Error::LaunchArgumentsNotSupported));
}

// list's aggregate ====================================================================================================

/// The count is data, not a diagnostic: it reaches the caller as an entry, whose null name is what
/// obliges every consumer to stop concluding absence (see `Installed::Undetermined`).
#[skuld::test]
fn an_aggregate_undetermined_entry_has_a_null_name_and_names_its_count() {
    match unreadable_aggregate(3) {
        Some(Installed::Undetermined { name, reason }) => {
            assert_eq!(
                name, None,
                "one entry stands for 3 ids, so there is no single id to name"
            );
            assert_eq!(reason, unreadable_notice(3), "the entry carries the notice verbatim");
        }
        other => panic!("services goetia could not classify establish neither presence nor absence: {other:?}"),
    }
}

/// The other half, and the one an over-eager fix would break: a host every one of whose services
/// goetia read leaves nothing undetermined, so `list` must add no entry and `daemon list` must not
/// exit `4` forever.
#[skuld::test]
fn no_aggregate_entry_is_emitted_for_a_zero_count() {
    assert!(unreadable_aggregate(0).is_none());
}

#[skuld::test]
fn unreadable_notice_is_one_line_and_agrees_in_number() {
    // The wording carries the honest part of this diagnostic — that ownership
    // is *unknown*, not that the services are foreign — so pin it rather than
    // let a later reword quietly turn it into a reassuring lie.
    let one = unreadable_notice(1);
    assert!(one.contains("1 service "), "singular noun: {one}");
    assert!(one.contains("unknown for it"), "singular pronoun: {one}");

    let many = unreadable_notice(3);
    assert!(many.contains("3 services "), "plural noun: {many}");
    assert!(many.contains("unknown for them"), "plural pronoun: {many}");

    for text in [&one, &many] {
        assert!(!text.contains('\n'), "must stay one line: {text}");
        assert!(
            text.contains("may be missing"),
            "must not claim the list is complete: {text}"
        );
        assert!(text.contains("re-run elevated"), "must name the remedy: {text}");
    }
}
