use std::io;

use super::*;
use crate::error::{Error, within};

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "denied")
}

/// A pair is two OS failures: a goetia error is not one, whether a failure carries it directly or under a
/// context.
#[skuld::test]
#[should_panic(expected = "a pair is two OS failures")]
fn a_goetia_error_carried_under_a_context_is_not_an_os_failure_of_a_pair() {
    let _ = paired(denied(), within("step", io::Error::other(Error::blob("carried"))));
}

#[skuld::test]
#[should_panic(expected = "a pair is two OS failures")]
fn a_goetia_error_carried_directly_is_not_an_os_failure_of_a_pair() {
    let _ = paired(io::Error::other(Error::blob("carried")), denied());
}
