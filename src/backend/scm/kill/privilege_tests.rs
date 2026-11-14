//! A token that does not hold `SeDebugPrivilege` must come out as "not enabled", never as an error,
//! so the open that follows speaks for itself. The tests that need a token holding the privilege
//! (an elevated administrator's) fail without one, naming it; `GOETIA_TEST_DEBUG_PRIVILEGE=0` opts
//! them out. The elevated `kill` tests in `tests/scm_integration` pin the whole path.

use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, LUID};
use windows_sys::Win32::Security::{
    GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, PRIVILEGE_SET, PrivilegeCheck, SE_DEBUG_NAME,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
};

use super::{DebugPrivilege, enable_in, restore_notice};
use crate::elevation::test_support::{self, ThreadTokenReset, process_token_copy, token_without_privileges};
use crate::testing::groups::{DEBUG_PRIVILEGE, need, process_var, require_enabled};

fn debug_privilege_tests_on() -> Result<(), String> {
    require_enabled(DEBUG_PRIVILEGE, process_var)
}

/// Fixture of the tests that need a token holding `SeDebugPrivilege` (an elevated administrator's):
/// they fail without one, and `GOETIA_TEST_DEBUG_PRIVILEGE=0` ignores them.
#[skuld::fixture(requires = [debug_privilege_tests_on])]
fn debug_privilege() -> Result<(), String> {
    need(
        DEBUG_PRIVILEGE,
        process_var,
        "this token does not hold SeDebugPrivilege; run with a token that does (an elevated one usually does)",
        holds_debug_privilege,
    )
}

/// Whether the process token holds `SeDebugPrivilege`, enabled or not; `Err` if the probe fails.
fn holds_debug_privilege() -> Result<bool, String> {
    let probe = || {
        let copy = process_token_copy(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)?;
        enable_in(copy.as_raw_handle())
    };
    probe().map_err(|e| {
        format!(
            "could not probe the token for SeDebugPrivilege: {}",
            crate::error::Chain(&e)
        )
    })
}

/// The calling thread's own token, if it has one, opened to be queried.
fn open_thread_token() -> Option<OwnedHandle> {
    test_support::open_thread_token(TOKEN_QUERY)
}

fn thread_has_token() -> bool {
    open_thread_token().is_some()
}

/// Which token object `token` is: a copy of a token has an id of its own.
fn token_id(token: RawHandle) -> (u32, i32) {
    // SAFETY: an all-zero `TOKEN_STATISTICS` is valid; the call fills it, and is given its size.
    let mut stats: TOKEN_STATISTICS = unsafe { std::mem::zeroed() };
    let mut needed = 0;
    // SAFETY: `token` is live with `TOKEN_QUERY`; `stats` is the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenStatistics,
            (&raw mut stats).cast(),
            std::mem::size_of::<TOKEN_STATISTICS>() as u32,
            &mut needed,
        )
    };
    assert_ne!(ok, 0, "GetTokenInformation: {}", io::Error::last_os_error());
    (stats.TokenId.LowPart, stats.TokenId.HighPart)
}

fn thread_token_id() -> Option<(u32, i32)> {
    open_thread_token().map(|token| token_id(token.as_raw_handle()))
}

/// Whether `SeDebugPrivilege` is enabled in the calling thread's token.
fn debug_privilege_enabled_on_thread() -> bool {
    let token = open_thread_token().expect("the thread has a token");
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: a pointer to a local, and the constant name of a privilege.
    assert_ne!(
        unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_DEBUG_NAME, &mut luid) },
        0
    );
    let mut required = PRIVILEGE_SET {
        PrivilegeCount: 1,
        Control: 1,
        Privilege: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: 0,
        }],
    };
    let mut result = 0;
    // SAFETY: `token` is live with `TOKEN_QUERY`; the rest are locals.
    let ok = unsafe { PrivilegeCheck(token.as_raw_handle(), &mut required, &mut result) };
    assert_ne!(ok, 0, "PrivilegeCheck: {}", io::Error::last_os_error());
    result != 0
}

#[skuld::test]
fn a_token_without_the_privilege_is_not_enabled_and_not_an_error() {
    let token = token_without_privileges();

    let enabled = enable_in(token.as_raw_handle());

    assert!(!enabled.expect("a missing privilege is not an error"));
}

#[skuld::test]
fn enabling_and_letting_go_leaves_the_thread_as_it_was() {
    assert!(
        !thread_has_token(),
        "a test thread starts without an impersonation token"
    );

    drop(DebugPrivilege::enable());

    assert!(!thread_has_token());
}

#[skuld::test]
fn the_thread_has_the_privilege_enabled_exactly_while_the_guard_lives(#[fixture(debug_privilege)] _held: &()) {
    let guard = DebugPrivilege::enable();

    assert!(guard.failure.is_none(), "{:?}", guard.failure);
    assert!(thread_has_token(), "the open runs on a privileged copy of the token");
    assert!(debug_privilege_enabled_on_thread());

    drop(guard);

    assert!(!thread_has_token());
}

/// The thread already impersonates: the guard replaces that token for the open, and puts the same
/// token back, not the process's and not none.
#[skuld::test]
fn dropping_puts_back_the_thread_token_that_was_there(#[fixture(debug_privilege)] _held: &()) {
    let prior =
        process_token_copy(TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE).expect("a copy of the process token");
    let _reset = ThreadTokenReset::impersonate(&prior);
    let prior_id = token_id(prior.as_raw_handle());
    assert_eq!(thread_token_id(), Some(prior_id));

    let guard = DebugPrivilege::enable();

    assert!(guard.failure.is_none(), "{:?}", guard.failure);
    assert_ne!(thread_token_id(), Some(prior_id), "the open runs on a copy");
    assert!(debug_privilege_enabled_on_thread());

    drop(guard);

    assert_eq!(thread_token_id(), Some(prior_id));
}

// Failures are reported -----------------------------------------------------------------------------------------------

fn failed_to_enable() -> DebugPrivilege {
    DebugPrivilege {
        restore: None,
        failure: Some(io::Error::other("DuplicateTokenEx failed")),
    }
}

#[skuld::test]
fn a_denied_open_names_the_privilege_step_that_failed() {
    let e = failed_to_enable().explain_denial(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));

    let msg = e.to_string();
    assert!(msg.contains("SeDebugPrivilege"), "{msg}");
    assert!(msg.contains("DuplicateTokenEx failed"), "{msg}");
    assert!(msg.contains("Access is denied"), "the open's own error is kept: {msg}");
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
}

/// A token that lacks the privilege is not a failure of the step, so the denial stands alone.
#[skuld::test]
fn a_denied_open_with_no_failed_step_is_unchanged() {
    let guard = DebugPrivilege {
        restore: None,
        failure: None,
    };

    let e = guard.explain_denial(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));

    assert_eq!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
}

#[skuld::test]
fn a_refusal_that_is_not_a_denial_is_unchanged_whatever_the_step_did() {
    let e = failed_to_enable().explain_denial(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION as i32));

    assert_eq!(e.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
}

#[skuld::test]
fn the_restore_notice_names_the_call_and_the_error() {
    let notice = restore_notice(&io::Error::other("the token handle is invalid"));

    assert!(notice.starts_with("warning: "), "{notice}");
    assert!(notice.contains("SetThreadToken"), "{notice}");
    assert!(notice.contains("the token handle is invalid"), "{notice}");
}
