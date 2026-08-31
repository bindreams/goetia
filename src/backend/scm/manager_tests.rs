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
    // here.
    let info = service_info(&dummy_reg(None), None, false);
    assert_eq!(info.account_name, None);
}

#[skuld::test]
fn update_spells_out_local_system_explicitly() {
    // `ChangeServiceConfigW(NULL)` means "leave the account unchanged", not
    // LocalSystem -- see `service_info`'s doc comment. A desired
    // `LocalSystem` on the update path must therefore be the literal string.
    let info = service_info(&dummy_reg(None), None, true);
    assert_eq!(info.account_name.as_deref(), Some(std::ffi::OsStr::new("LocalSystem")));
}

#[skuld::test]
fn a_real_account_is_passed_through_on_both_paths() {
    for for_update in [false, true] {
        let info = service_info(&dummy_reg(Some(r".\svc-account")), None, for_update);
        assert_eq!(
            info.account_name.as_deref(),
            Some(std::ffi::OsStr::new(r".\svc-account")),
            "for_update={for_update}"
        );
    }
}

#[skuld::test]
fn password_is_only_attached_when_given() {
    let with_password = service_info(&dummy_reg(Some("svc")), Some("hunter2".to_string()), false);
    assert_eq!(
        with_password.account_password.as_deref(),
        Some(std::ffi::OsStr::new("hunter2"))
    );

    let without_password = service_info(&dummy_reg(Some("svc")), None, false);
    assert_eq!(without_password.account_password, None);
}

#[skuld::test]
fn is_not_found_matches_only_error_service_does_not_exist() {
    let not_found = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1060));
    assert!(is_not_found(&not_found));

    let access_denied = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(5));
    assert!(!is_not_found(&access_denied));
}
