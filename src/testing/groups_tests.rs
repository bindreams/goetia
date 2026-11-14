use super::{DEBUG_PRIVILEGE, check_group, enter, need, require_enabled, validate_all};

const GROUP: &str = "GOETIA_TEST_X";
const CONSENT: &str = "GOETIA_TEST_X_CONSENT";

fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
}

/// What `check_group` says for the given values of the group and consent variables, `None` for unset.
fn checked(group: Option<&str>, consent: Option<&str>) -> Result<bool, String> {
    let vars: Vec<(&str, &str)> = [(GROUP, group), (CONSENT, consent)]
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| (name, v)))
        .collect();
    check_group(GROUP, env(&vars))
}

#[skuld::test]
fn the_group_variable_is_on_unset_or_1_off_for_0_and_an_error_otherwise() {
    for group in [None, Some("1")] {
        assert_eq!(checked(group, Some("1")), Ok(true), "group {group:?}");
    }
    assert_eq!(checked(Some("0"), Some("1")), Ok(false));
    for value in ["2", "yes", "true", " 1", "", "01"] {
        let why = checked(Some(value), Some("1")).unwrap_err();
        assert!(
            why.contains(GROUP) && why.contains(&format!("{value:?}")),
            "{value:?}: {why}"
        );
    }
}

#[skuld::test]
fn consent_is_given_only_by_exactly_1() {
    for consent in [None, Some("0")] {
        let why = checked(None, consent).unwrap_err();
        assert!(
            why.contains(&format!("{CONSENT}=1")) && why.contains("sandbox") && why.contains(&format!("{GROUP}=0")),
            "{consent:?}: {why}"
        );
    }
    for value in ["2", "yes", "true", " 1", "", "01"] {
        let why = checked(None, Some(value)).unwrap_err();
        assert!(
            why.contains(CONSENT) && why.contains(&format!("{value:?}")),
            "{value:?}: {why}"
        );
    }
}

#[skuld::test]
fn a_group_that_is_off_needs_no_consent_but_an_invalid_value_of_either_is_an_error_whatever_the_other() {
    for consent in [None, Some("0"), Some("1")] {
        assert_eq!(checked(Some("0"), consent), Ok(false), "consent {consent:?}");
    }
    for other in [None, Some("0"), Some("1"), Some("yes")] {
        assert!(checked(Some("yes"), other).is_err(), "consent {other:?}");
        assert!(checked(other, Some("yes")).is_err(), "group {other:?}");
    }
}

/// Every known group's variables are checked at harness start, on any OS and whatever the group's
/// state.
#[skuld::test]
fn every_known_groups_variables_are_validated_at_start() {
    assert!(validate_all(env(&[])).is_ok());
    assert!(validate_all(env(&[("GOETIA_TEST_SCM", "0"), ("GOETIA_TEST_SYSTEMD_CONSENT", "1")])).is_ok());
    for group in ["SCM", "SYSTEMD", "LAUNCHD"] {
        for suffix in ["", "_CONSENT"] {
            let name = format!("GOETIA_TEST_{group}{suffix}");
            let why = validate_all(env(&[(&name, "maybe")])).unwrap_err();
            assert!(why.contains(&name) && why.contains("maybe"), "{name}: {why}");
        }
        let off = format!("GOETIA_TEST_{group}");
        let consent = format!("{off}_CONSENT");
        assert!(
            validate_all(env(&[(&off, "0"), (&consent, "maybe")])).is_err(),
            "{group}"
        );
    }
}

/// The `requires` of a group: only `=0` makes skuld ignore a test.
#[skuld::test]
fn only_a_group_set_to_0_is_ignored() {
    assert!(require_enabled(GROUP, env(&[])).is_ok());
    assert!(require_enabled(GROUP, env(&[(GROUP, "1")])).is_ok());
    assert!(require_enabled(GROUP, env(&[(GROUP, "yes")])).is_ok());
    let why = require_enabled(GROUP, env(&[(GROUP, "0")])).unwrap_err();
    assert!(why.contains(&format!("{GROUP}=0")), "{why}");
}

/// A consented group on a host that lacks what the group needs fails; it does not
/// skip.
#[skuld::test]
fn a_consented_group_fails_when_the_host_lacks_what_it_needs() {
    let consent = env(&[(CONSENT, "1")]);

    assert!(enter(GROUP, "does a thing", &consent, || true).is_ok());
    let why = enter(GROUP, "does a thing", &consent, || false)
        .err()
        .expect("an unelevated host");
    assert!(why.contains("elevated") && why.contains(GROUP), "{why}");

    let why = enter(GROUP, "does a thing", env(&[]), || false)
        .err()
        .expect("no consent");
    assert!(why.contains(CONSENT) && why.contains("does a thing"), "{why}");
}

/// A group that is off refuses at setup too.
#[skuld::test]
fn a_group_that_is_off_refuses_its_setup() {
    let why = enter(GROUP, "does a thing", env(&[(GROUP, "0")]), || true)
        .err()
        .expect("a group that is off");
    assert!(why.contains(&format!("{GROUP}=0")), "{why}");
}

/// A test that needs something the host may lack, and changes no system state, has a switch and no
/// consent: it runs by default and fails, naming the missing thing, where the host lacks it.
#[skuld::test]
fn a_test_that_needs_a_dependency_fails_where_the_host_lacks_it() {
    let missing = || Err("this token does not hold SeDebugPrivilege".to_string());

    for vars in [env(&[]), env(&[(GROUP, "1")])] {
        assert!(need(GROUP, &vars, || Ok(())).is_ok());
    }
    let why = need(GROUP, env(&[]), missing).unwrap_err();
    assert!(
        why.contains("SeDebugPrivilege") && why.contains(&format!("{GROUP}=0")),
        "{why}"
    );
}

/// The opt-out refuses at setup, so `--run-ignored only` does not run what the switch turned off.
#[skuld::test]
fn a_dependency_test_that_is_switched_off_refuses_without_probing_the_host() {
    let why = need(GROUP, env(&[(GROUP, "0")]), || {
        panic!("the host is probed though the switch is off")
    })
    .unwrap_err();

    assert!(why.contains(&format!("{GROUP}=0")), "{why}");
}

#[skuld::test]
fn a_dependency_switch_with_an_invalid_value_fails_the_test() {
    let why = need(GROUP, env(&[(GROUP, "yes")]), || Ok(())).unwrap_err();

    assert!(why.contains(GROUP) && why.contains("\"yes\""), "{why}");
}

#[skuld::test]
fn the_debug_privilege_switch_is_validated_at_start() {
    assert!(validate_all(env(&[(DEBUG_PRIVILEGE, "0")])).is_ok());
    assert!(validate_all(env(&[(DEBUG_PRIVILEGE, "1")])).is_ok());
    let why = validate_all(env(&[(DEBUG_PRIVILEGE, "maybe")])).unwrap_err();
    assert!(why.contains(DEBUG_PRIVILEGE) && why.contains("maybe"), "{why}");
}
