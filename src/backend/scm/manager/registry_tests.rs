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

#[skuld::test]
fn format_environment_lines_preserves_equals_signs_in_the_value() {
    let mut env = BTreeMap::new();
    env.insert("URL".to_string(), "http://x/a=b".to_string());
    assert_eq!(format_environment_lines(&env), vec!["URL=http://x/a=b".to_string()]);
}
