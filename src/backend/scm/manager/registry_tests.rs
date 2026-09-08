use std::collections::BTreeMap;

use super::*;

#[skuld::test]
fn service_key_path_is_under_current_control_set_services() {
    assert_eq!(
        service_key_path("goetia-test"),
        r"SYSTEM\CurrentControlSet\Services\goetia-test"
    );
}

#[skuld::test]
fn parameters_key_path_is_the_service_key_plus_parameters() {
    assert_eq!(
        parameters_key_path("goetia-test"),
        r"SYSTEM\CurrentControlSet\Services\goetia-test\Parameters"
    );
}

#[skuld::test]
fn format_environment_lines_is_key_equals_value_per_entry() {
    let mut env = BTreeMap::new();
    env.insert("B".to_string(), "2".to_string());
    env.insert("A".to_string(), "1".to_string());
    // `BTreeMap` iterates key-sorted, so the lines come out deterministically
    // ordered regardless of insertion order.
    assert_eq!(
        format_environment_lines(&env),
        vec!["A=1".to_string(), "B=2".to_string()]
    );
}

#[skuld::test]
fn format_environment_lines_is_empty_for_empty_env() {
    assert!(format_environment_lines(&BTreeMap::new()).is_empty());
}

/// The `Parameters` half of this backend's [`Error::Undetermined`] keying. `winreg` surfaces
/// `ERROR_ACCESS_DENIED` as `ErrorKind::PermissionDenied`, and that chooses the *recovery* alone —
/// the class is the same either way, because a corrupt hive leaves goetia exactly as ignorant of
/// the id as a denial does.
#[skuld::test]
fn a_parameters_read_that_did_not_complete_is_undetermined() {
    let path = parameters_key_path("x");
    for (source, wants_elevation) in [
        (std::io::Error::from(std::io::ErrorKind::PermissionDenied), true),
        (std::io::Error::other("the hive is corrupt"), false),
    ] {
        let rendered = format!("{source:?}");
        let Error::Undetermined { id, reason, recovery } = registry_undetermined("x", "open", &path, &source) else {
            panic!("{rendered}: a read that did not complete establishes nothing about the id");
        };

        assert_eq!(id, "x");
        assert!(
            reason.contains(&path),
            "{rendered}: the reason must name the key: {reason}"
        );
        assert_eq!(
            recovery.contains("re-run as Administrator"),
            wants_elevation,
            "{rendered}: {recovery}"
        );
    }
}

#[skuld::test]
fn format_environment_lines_preserves_equals_signs_in_the_value() {
    let mut env = BTreeMap::new();
    env.insert("URL".to_string(), "http://x/a=b".to_string());
    assert_eq!(format_environment_lines(&env), vec!["URL=http://x/a=b".to_string()]);
}
