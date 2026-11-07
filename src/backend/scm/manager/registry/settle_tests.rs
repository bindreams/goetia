use std::io;

use super::*;

const NOT_FOUND: i32 = 2;
const ACCESS_DENIED: i32 = 5;
const IO_DEVICE: i32 = 1117;
const KEY_DELETED: i32 = 1018;

fn failure(code: i32) -> Failure {
    Failure {
        op: "open".to_string(),
        path: r"SYSTEM\CurrentControlSet\Services\x\Parameters".to_string(),
        source: io::Error::from_raw_os_error(code),
    }
}

fn code_of(failure: &Failure) -> Option<i32> {
    failure.source.raw_os_error()
}

/// A `stands` answer for a test that must not reach it.
fn never<T>() -> Result<T, Failure> {
    panic!("the existence of the service must not be asked here")
}

#[skuld::test]
fn an_open_that_finds_no_key_is_absent_when_the_key_is_optional() {
    assert!(matches!(Stop::at_open(failure(NOT_FOUND), Some(7)), Stop::Absent(7)));
}

#[skuld::test]
fn an_open_that_finds_no_service_key_is_a_vanish() {
    assert!(matches!(
        Stop::<i32>::at_open(failure(NOT_FOUND), None),
        Stop::Vanished(f) if code_of(&f) == Some(NOT_FOUND)
    ));
}

#[skuld::test]
fn an_open_of_a_deleted_key_is_a_vanish_whether_or_not_the_key_is_optional() {
    for absent in [Some(7), None] {
        assert!(
            matches!(Stop::at_open(failure(KEY_DELETED), absent), Stop::Vanished(f) if code_of(&f) == Some(KEY_DELETED))
        );
    }
}

#[skuld::test]
fn any_other_open_failure_is_other() {
    for code in [ACCESS_DENIED, IO_DEVICE] {
        match Stop::<i32>::at_open(failure(code), Some(7)) {
            Stop::Other(f) => assert_eq!(code_of(&f), Some(code)),
            other => panic!("{code}: expected Other, got {other:?}"),
        }
    }
}

#[skuld::test]
fn a_key_deleted_or_not_found_mid_read_is_a_vanish() {
    for code in [KEY_DELETED, NOT_FOUND] {
        assert!(
            matches!(Stop::<i32>::mid_read(failure(code)), Stop::Vanished(f) if code_of(&f) == Some(code)),
            "{code}"
        );
    }
}

#[skuld::test]
fn any_other_mid_read_failure_is_other() {
    for code in [ACCESS_DENIED, IO_DEVICE] {
        match Stop::<i32>::mid_read(failure(code)) {
            Stop::Other(f) => assert_eq!(code_of(&f), Some(code)),
            other => panic!("{code}: expected Other, got {other:?}"),
        }
    }
}

#[skuld::test]
fn a_service_key_that_opens_stands() {
    assert!(stands(Ok(())).unwrap());
}

#[skuld::test]
fn a_service_key_not_found_or_deleted_does_not_stand() {
    for code in [NOT_FOUND, KEY_DELETED] {
        assert!(!stands(Err(io::Error::from_raw_os_error(code))).unwrap(), "{code}");
    }
}

/// Resolving the path comes before the access check, so a denial proves the key is there.
#[skuld::test]
fn a_service_key_that_denies_the_open_stands() {
    assert!(stands(Err(io::Error::from_raw_os_error(ACCESS_DENIED))).unwrap());
}

#[skuld::test]
fn a_service_key_that_fails_otherwise_says_nothing() {
    let e = stands(Err(io::Error::from_raw_os_error(IO_DEVICE))).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(IO_DEVICE));
}

#[skuld::test]
fn absent_with_the_service_standing_is_its_value() {
    assert!(matches!(
        settle(Stop::Absent(7), Origin::Opened, || Ok(true)),
        Settled::Value(7)
    ));
}

/// The name came from the enumeration of `Services`: a gone service leaves at most a stale row that
/// the user's own concurrent uninstall explains, so no second open is spent on it.
#[skuld::test]
fn absent_under_a_listed_name_is_its_value_without_asking_whether_the_service_stands() {
    assert!(matches!(
        settle(Stop::Absent(7), Origin::Listed, never),
        Settled::Value(7)
    ));
}

#[skuld::test]
fn a_vanish_under_a_listed_name_still_asks_whether_the_service_stands() {
    assert!(matches!(
        settle(Stop::<i32>::Vanished(failure(KEY_DELETED)), Origin::Listed, || Ok(
            false
        )),
        Settled::Gone
    ));
    match settle(Stop::<i32>::Vanished(failure(KEY_DELETED)), Origin::Listed, || Ok(true)) {
        Settled::Failed(f) => assert_eq!(code_of(&f), Some(KEY_DELETED)),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[skuld::test]
fn absent_with_the_service_gone_is_gone() {
    assert!(matches!(
        settle(Stop::Absent(7), Origin::Opened, || Ok(false)),
        Settled::Gone
    ));
}

/// Not read again and not waited on: a key another process took away is undetermined, with what
/// the read met.
#[skuld::test]
fn a_vanish_with_the_service_standing_fails_with_its_cause() {
    for code in [NOT_FOUND, KEY_DELETED] {
        match settle(Stop::<i32>::Vanished(failure(code)), Origin::Opened, || Ok(true)) {
            Settled::Failed(f) => assert_eq!(code_of(&f), Some(code)),
            other => panic!("{code}: expected Failed, got {other:?}"),
        }
    }
}

#[skuld::test]
fn a_vanish_with_the_service_gone_is_gone() {
    assert!(matches!(
        settle(Stop::<i32>::Vanished(failure(KEY_DELETED)), Origin::Opened, || Ok(
            false
        )),
        Settled::Gone
    ));
}

#[skuld::test]
fn a_failed_existence_check_fails_every_vanish_with_its_own_cause() {
    let stops: [Stop<i32>; 2] = [Stop::Absent(7), Stop::Vanished(failure(NOT_FOUND))];
    for stop in stops {
        match settle(stop, Origin::Opened, || Err(failure(IO_DEVICE))) {
            Settled::Failed(f) => assert_eq!(code_of(&f), Some(IO_DEVICE)),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}

#[skuld::test]
fn another_failure_fails_without_asking_whether_the_service_stands() {
    match settle(Stop::<i32>::Other(failure(ACCESS_DENIED)), Origin::Opened, never) {
        Settled::Failed(f) => assert_eq!(code_of(&f), Some(ACCESS_DENIED)),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[skuld::test]
fn a_key_that_is_not_found_or_deleted_is_gone_and_a_denied_one_is_not() {
    assert!(is_gone(&io::Error::from_raw_os_error(NOT_FOUND)));
    assert!(is_gone(&io::Error::from_raw_os_error(KEY_DELETED)));
    assert!(!is_gone(&io::Error::from_raw_os_error(ACCESS_DENIED)));
}
