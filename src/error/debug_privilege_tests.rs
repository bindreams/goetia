use std::io;

use super::*;
use crate::error::{Error, within};

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "denied")
}

/// A pair is two OS failures: a goetia error is not one, whether a failure carries it directly or under a
/// context.
#[cfg(debug_assertions)]
#[skuld::test]
#[should_panic(expected = "a pair is two OS failures")]
fn a_goetia_error_carried_under_a_context_is_not_an_os_failure_of_a_pair() {
    let _ = debug_privilege_also_failed(denied(), within("step", io::Error::other(Error::blob("carried"))));
}

#[cfg(debug_assertions)]
#[skuld::test]
#[should_panic(expected = "a pair is two OS failures")]
fn a_goetia_error_carried_directly_is_not_an_os_failure_of_a_pair() {
    let _ = debug_privilege_also_failed(io::Error::other(Error::blob("carried")), denied());
}
