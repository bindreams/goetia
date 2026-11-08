use windows_sys::Win32::Security::{TOKEN_IMPERSONATE, TOKEN_QUERY};

use super::*;
use crate::backend::scm::shim::acl::administrators;
use crate::elevation::test_support::{ThreadTokenReset, process_token_copy, restricted_token_copy};

/// A token whose Administrators group is disabled has no Administrators, whatever it belongs to:
/// that is what an unelevated administrator's token is, and what an elevated one is not.
#[skuld::test]
fn a_token_with_administrators_disabled_does_not_have_them() {
    let token = restricted_token_copy(0, &mut [administrators()], TOKEN_QUERY | TOKEN_IMPERSONATE);
    let _reset = ThreadTokenReset::impersonate(&token);

    assert!(!token_has_administrators().unwrap());
}

/// The thread's token is asked when it impersonates, and the process's when it does not: a copy of
/// the process's token answers as the process does.
#[skuld::test]
fn the_process_token_answers_as_its_copy_does() {
    let process = token_has_administrators().unwrap();
    let copy = process_token_copy(TOKEN_QUERY | TOKEN_IMPERSONATE);
    let _reset = ThreadTokenReset::impersonate(&copy);

    assert_eq!(token_has_administrators().unwrap(), process);
}
