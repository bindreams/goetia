use windows_sys::Win32::Security::{TOKEN_IMPERSONATE, TOKEN_QUERY};
use windows_sys::Win32::System::SystemServices::ACCESS_SYSTEM_SECURITY;

use super::*;
use crate::backend::scm::sid::administrators;
use crate::elevation::test_support::{
    ThreadTokenReset, impersonation_copy, open_process_token, process_token_copy, restricted_token_copy,
};

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
    let copy = process_token_copy(TOKEN_QUERY | TOKEN_IMPERSONATE).unwrap();
    let _reset = ThreadTokenReset::impersonate(&copy);

    assert_eq!(token_has_administrators().unwrap(), process);
}

/// A token that cannot be opened or copied is an error that names the call, for a caller that must
/// report it rather than panic.
#[skuld::test]
fn a_token_that_cannot_be_opened_or_copied_is_an_error_naming_the_call() {
    let why = open_process_token(ACCESS_SYSTEM_SECURITY).unwrap_err().to_string();
    assert!(why.contains("OpenProcessToken"), "{why}");

    let query_only = open_process_token(TOKEN_QUERY).unwrap();
    let why = impersonation_copy(&query_only, TOKEN_QUERY).unwrap_err().to_string();
    assert!(why.contains("DuplicateTokenEx"), "{why}");
}
