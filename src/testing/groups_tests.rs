use super::{check_group, enter, require_enabled};

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

/// Mutant: an unset group variable disables the group, or an invalid one is read as on or off.
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

/// Mutant: consent defaults to given, or any set value counts.
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

/// Mutant: consent is asked of a group that is off, or an invalid group value is excused by consent.
#[skuld::test]
fn a_group_that_is_off_needs_no_consent_and_an_invalid_one_is_an_error_whatever_the_consent() {
    for consent in [None, Some("0"), Some("1"), Some("yes")] {
        assert_eq!(checked(Some("0"), consent), Ok(false), "consent {consent:?}");
        assert!(checked(Some("yes"), consent).is_err(), "consent {consent:?}");
    }
}

/// The `requires` of a group: only `=0` makes skuld ignore a test. Mutant: an invalid value or missing
/// consent is ignored too, which would skip the test silently instead of failing it.
#[skuld::test]
fn only_a_group_set_to_0_is_ignored() {
    assert!(require_enabled(GROUP, env(&[])).is_ok());
    assert!(require_enabled(GROUP, env(&[(GROUP, "1")])).is_ok());
    assert!(require_enabled(GROUP, env(&[(GROUP, "yes")])).is_ok());
    let why = require_enabled(GROUP, env(&[(GROUP, "0")])).unwrap_err();
    assert!(why.contains(&format!("{GROUP}=0")), "{why}");
}

/// A consented group on a host that lacks what the group needs fails, naming the lack; it does not
/// skip. Mutant: the precondition is checked before consent, or not at all.
#[skuld::test]
fn a_consented_group_fails_when_the_host_lacks_what_it_needs() {
    let consent = env(&[(CONSENT, "1")]);

    assert!(enter(GROUP, "does a thing", &consent, || Ok(())).is_ok());
    let why = enter(GROUP, "does a thing", &consent, || {
        Err("the host has no flux".to_string())
    })
    .err()
    .expect("a host without flux");
    assert!(why.contains("the host has no flux") && why.contains(GROUP), "{why}");

    let why = enter(GROUP, "does a thing", env(&[]), || {
        Err("the host has no flux".to_string())
    })
    .err()
    .expect("no consent");
    assert!(why.contains(CONSENT) && why.contains("does a thing"), "{why}");
}

/// A group that is off refuses at setup too: skuld runs the body of a test whose `requires` failed
/// when ignored tests are included.
#[skuld::test]
fn a_group_that_is_off_refuses_its_setup() {
    let why = enter(GROUP, "does a thing", env(&[(GROUP, "0")]), || Ok(()))
        .err()
        .expect("a group that is off");
    assert!(why.contains(&format!("{GROUP}=0")), "{why}");
}
