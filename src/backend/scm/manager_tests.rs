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
