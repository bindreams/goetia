use std::io;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_KEY_DELETED};

use super::*;

const NOT_FOUND: i32 = 2;
const ACCESS_DENIED: i32 = ERROR_ACCESS_DENIED as i32;
const IO_DEVICE: i32 = 1117;
const KEY_DELETED: i32 = ERROR_KEY_DELETED as i32;

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
fn a_value_not_found_is_a_delete_that_is_done_and_any_other_failure_is_not() {
    assert!(tolerate_not_found(Ok(())).is_ok());
    assert!(tolerate_not_found(Err(io::Error::from(io::ErrorKind::NotFound))).is_ok());
    // The key holding the value was marked deleted: the write was lost, which is not done.
    assert!(tolerate_not_found(Err(io::Error::from_raw_os_error(KEY_DELETED))).is_err());
}

#[skuld::test]
fn a_key_gone_or_deleted_is_a_delete_that_is_done_and_any_other_failure_is_not() {
    assert!(gone_is_done(Ok(())).is_ok());
    for code in [NOT_FOUND, KEY_DELETED] {
        assert!(gone_is_done(Err(io::Error::from_raw_os_error(code))).is_ok(), "{code}");
    }
    let denied = gone_is_done(Err(io::Error::from_raw_os_error(ACCESS_DENIED))).unwrap_err();
    assert_eq!(denied.raw_os_error(), Some(ACCESS_DENIED));
}

fn os(code: i32) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(code))
}

/// The open of the key as itself, for a test that must not reach it.
fn itself_never_asked() -> io::Result<()> {
    panic!("the key as itself must not be opened here")
}

#[skuld::test]
fn a_service_key_that_opens_stands() {
    assert!(stands(Ok(()), itself_never_asked).unwrap());
}

#[skuld::test]
fn a_service_key_not_found_or_deleted_as_itself_too_does_not_stand() {
    for code in [NOT_FOUND, KEY_DELETED] {
        assert!(!stands(os(code), || os(code)).unwrap(), "{code}");
    }
}

/// Resolving the path comes before the access check, so a denial proves the key is there.
#[skuld::test]
fn a_service_key_that_denies_the_open_stands() {
    assert!(stands(os(ACCESS_DENIED), itself_never_asked).unwrap());
}

#[skuld::test]
fn a_service_key_that_fails_otherwise_says_nothing() {
    let e = stands(os(IO_DEVICE), itself_never_asked).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(IO_DEVICE));
}

/// A registry link whose target is gone opens as itself and not through: SCM has a service at that
/// key, and the read that cannot see through it is a failure with the open's own error, not a
/// service that is gone. The same holds for a target marked deleted, and a link that denies the
/// open as itself.
#[skuld::test]
fn a_service_key_that_is_there_but_does_not_resolve_is_the_failure_of_the_open_through_it() {
    for (through, itself) in [(NOT_FOUND, None), (KEY_DELETED, None), (NOT_FOUND, Some(ACCESS_DENIED))] {
        let e = stands(os(through), || itself.map_or(Ok(()), os)).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(through), "{through} with {itself:?}");
    }
}

#[skuld::test]
fn a_service_key_gone_through_whose_own_open_fails_otherwise_says_nothing() {
    let e = stands(os(NOT_FOUND), || os(IO_DEVICE)).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(IO_DEVICE));
}

#[skuld::test]
fn absent_with_the_service_standing_is_its_value() {
    assert!(matches!(settle(Stop::Absent(7), || Ok(true)), Settled::Value(7)));
}

/// A missing optional key always asks whether the service stands.
#[skuld::test]
fn absent_asks_whether_the_service_stands() {
    let asked = std::cell::Cell::new(false);
    let settled = settle(Stop::Absent(7), || {
        asked.set(true);
        Ok(true)
    });

    assert!(matches!(settled, Settled::Value(7)));
    assert!(asked.get());
}

#[skuld::test]
fn absent_with_a_service_key_that_does_not_resolve_fails_with_that_cause() {
    match settle(Stop::Absent(7), || Err(failure(NOT_FOUND))) {
        Settled::Failed(f) => assert_eq!(code_of(&f), Some(NOT_FOUND)),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[skuld::test]
fn absent_with_the_service_gone_is_gone() {
    assert!(matches!(settle(Stop::Absent(7), || Ok(false)), Settled::Gone));
}

/// Not read again and not waited on: a key another process took away is undetermined, with what
/// the read met.
#[skuld::test]
fn a_vanish_with_the_service_standing_fails_with_its_cause() {
    for code in [NOT_FOUND, KEY_DELETED] {
        match settle(Stop::<i32>::Vanished(failure(code)), || Ok(true)) {
            Settled::Failed(f) => assert_eq!(code_of(&f), Some(code)),
            other => panic!("{code}: expected Failed, got {other:?}"),
        }
    }
}

#[skuld::test]
fn a_vanish_with_the_service_gone_is_gone() {
    assert!(matches!(
        settle(Stop::<i32>::Vanished(failure(KEY_DELETED)), || Ok(false)),
        Settled::Gone
    ));
}

#[skuld::test]
fn a_failed_existence_check_fails_every_vanish_with_its_own_cause() {
    let stops: [Stop<i32>; 2] = [Stop::Absent(7), Stop::Vanished(failure(NOT_FOUND))];
    for stop in stops {
        match settle(stop, || Err(failure(IO_DEVICE))) {
            Settled::Failed(f) => assert_eq!(code_of(&f), Some(IO_DEVICE)),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}

#[skuld::test]
fn another_failure_fails_without_asking_whether_the_service_stands() {
    match settle(Stop::<i32>::Other(failure(ACCESS_DENIED)), never) {
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
