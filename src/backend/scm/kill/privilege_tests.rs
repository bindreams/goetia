//! A token that does not hold `SeDebugPrivilege` must come out as "not enabled", never as an error,
//! so the open that follows speaks for itself. The tests that need a token holding the privilege
//! (an elevated administrator's, usually) fail without one, naming it;
//! `GOETIA_TEST_DEBUG_PRIVILEGE=0` opts them out. The elevated `kill` tests in
//! `tests/scm_integration` pin the whole path.

use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NOT_ENOUGH_MEMORY, ERROR_SHARING_VIOLATION, LUID};
use windows_sys::Win32::Security::{
    GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, PRIVILEGE_SET, PrivilegeCheck, SE_DEBUG_NAME,
    TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
};

use super::{DebugPrivilege, enable_in, restore_notice};
use crate::backend::kill::kill_failed;
use crate::elevation::test_support::{self, ThreadTokenReset, process_token_copy, token_without_privileges};
use crate::error::{Error, os_error_in, within};
use crate::testing::groups::debug_privilege;

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

/// The failed step as the production steps build it: its call named by `within`.
fn failed_to_enable() -> DebugPrivilege {
    DebugPrivilege {
        restore: None,
        failure: Some(within(
            "DuplicateTokenEx",
            io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32),
        )),
    }
}

fn denied() -> io::Error {
    io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)
}

/// The open first, then the step that failed with its reason: each printed once.
#[skuld::test]
fn a_denied_open_names_the_privilege_step_that_failed() {
    let e = failed_to_enable().explain_denial(denied());

    assert_eq!(
        e.to_string(),
        format!(
            "{} (enabling SeDebugPrivilege for the open also failed: DuplicateTokenEx: {})",
            denied(),
            io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32),
        )
    );
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    let report = Error::os("kill", e).report().to_string();
    assert_eq!(report.matches(&denied().to_string()).count(), 1, "{report}");
    assert_eq!(report.matches("DuplicateTokenEx").count(), 1, "{report}");
}

/// The denial's code is the open's, with a failed step and without one.
#[skuld::test]
fn a_denied_open_keeps_its_os_code_whether_or_not_a_step_failed() {
    let no_step = DebugPrivilege {
        restore: None,
        failure: None,
    };
    for e in [
        failed_to_enable().explain_denial(denied()),
        no_step.explain_denial(denied()),
    ] {
        assert_eq!(Error::os("kill", e).raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
    }
}

#[skuld::test]
fn a_denied_open_with_the_failed_step_is_found_by_os_error_in() {
    let e = failed_to_enable().explain_denial(denied());

    assert_eq!(os_error_in(&e), Some(ERROR_ACCESS_DENIED as i32));
}

/// A kill whose open was denied says what was being killed: the denial with its step is not a goetia
/// error that comes back as itself.
#[skuld::test]
fn a_denied_open_with_the_failed_step_keeps_the_kills_context() {
    let e = failed_to_enable().explain_denial(denied());

    let failed = kill_failed("frpc", e);

    assert_eq!(failed.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
    assert_eq!(
        failed.report().to_string(),
        format!(
            "failed to kill `frpc`: {} (enabling SeDebugPrivilege for the open also failed: DuplicateTokenEx: {})",
            denied(),
            io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32),
        )
    );
}

/// The denial is read as the search reads any error's OS code, so a denial said within a context is
/// explained as a bare one is.
#[skuld::test]
fn a_denial_said_within_a_context_is_explained_as_a_bare_one_is() {
    let e = failed_to_enable().explain_denial(within("opening process 7", denied()));

    assert_eq!(os_error_in(&e), Some(ERROR_ACCESS_DENIED as i32));
    assert!(
        Error::os("kill", e)
            .report()
            .to_string()
            .contains("enabling SeDebugPrivilege for the open also failed"),
    );
}

/// A token that lacks the privilege is not a failure of the step, so the denial stands alone.
#[skuld::test]
fn a_denied_open_with_no_failed_step_is_unchanged() {
    let guard = DebugPrivilege {
        restore: None,
        failure: None,
    };

    let e = guard.explain_denial(denied());

    assert_eq!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
    assert_eq!(e.to_string(), denied().to_string());
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

#[skuld::test]
fn the_restore_notice_prints_the_cause_of_the_error() {
    let notice = restore_notice(&within(
        "the token was refused",
        io::Error::other("the handle is invalid"),
    ));

    assert!(
        notice.ends_with("the token was refused: the handle is invalid"),
        "{notice}"
    );
}
