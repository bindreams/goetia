use super::Backend;

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
