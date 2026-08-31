//! Shared `DaemonSpec` construction for the SCM integration tests.

use std::collections::BTreeMap;

use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};

use crate::{fixture, support};

/// The argv `type: managed` should run: this test binary itself, dispatched
/// into `fixture::service_main` — see `fixture.rs`'s module doc comment.
pub fn fixture_command(id: &str, start_port: u16, stop_port: u16, mode: &str) -> Vec<String> {
    vec![
        support::current_exe_str(),
        fixture::FIXTURE.to_string(),
        id.to_string(),
        start_port.to_string(),
        stop_port.to_string(),
        mode.to_string(),
    ]
}

/// A `type: managed` `DaemonSpec`, every field spelled out. Built literally
/// (never via `spec::resolve`) since these tests run only on Windows and
/// construct `command`/paths for this host directly.
#[allow(clippy::too_many_arguments)]
pub fn mk_spec_full(
    id: &str,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    user: User,
    restart: Restart,
    restart_delay: Option<std::time::Duration>,
) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).expect("random_test_id produces a valid Id"),
        name: id.to_string(),
        command,
        cwd: None,
        env,
        user,
        restart,
        restart_delay,
        logs: None,
        kind: Kind::Managed,
    }
}

/// A minimal, valid `type: managed` `DaemonSpec` running as `user`, with no
/// SCM recovery actions (`restart: never`).
pub fn mk_spec_as(id: &str, command: Vec<String>, env: BTreeMap<String, String>, user: User) -> DaemonSpec {
    mk_spec_full(id, command, env, user, Restart::Never, None)
}

pub fn mk_spec(id: &str, command: Vec<String>, env: BTreeMap<String, String>) -> DaemonSpec {
    mk_spec_as(id, command, env, User::Root)
}

/// The `mk` [`goetia::manager::conformance::run`] needs: a fresh, valid
/// `type: managed` spec for any `id`. Ports are dummy — the fixture reports
/// in on a best-effort basis regardless of whether anything listens (see
/// `fixture.rs`'s `connect`), and no conformance scenario reads the report.
pub fn conformance_mk(id: &str) -> DaemonSpec {
    mk_spec(id, fixture_command(id, 1, 1, "plain"), BTreeMap::new())
}

// Account SID lookup ==================================================================================================
//
// `deleted_account_makes_the_service_oursunreadable` needs the SID string of
// a real (temporary) local account, so it can install `user: {id: <sid>}`,
// delete the account, and observe `Ownership::OursUnreadable`. Raw
// `windows-sys` FFI, mirroring `identity::sid_for_account_name` +
// `identity::account_name_from_sid_string`'s `ConvertStringSidToSidW`
// counterpart — sanctioned here the same way `fixture.rs` already uses raw
// FFI for SCM dispatch.

use std::os::windows::ffi::OsStrExt as _;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{LookupAccountNameW, PSID};

fn wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `LookupAccountNameW` + `ConvertSidToStringSidW`: the string SID for a
/// local account name (e.g. one this test just created with `net user`).
pub fn sid_string_for_account(account_name: &str) -> String {
    let wide_name = wide_null(account_name);
    let mut sid_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_: i32 = 0;
    // SAFETY: sizing call — every buffer pointer is null, every length
    // out-param a valid `u32` lvalue. Expected to fail with
    // `ERROR_INSUFFICIENT_BUFFER`.
    let sizing_ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide_name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut use_,
        )
    };
    assert_eq!(
        sizing_ok, 0,
        "LookupAccountNameW unexpectedly succeeded on a zero-sized buffer"
    );
    let err = std::io::Error::last_os_error();
    assert_eq!(
        err.raw_os_error(),
        Some(ERROR_INSUFFICIENT_BUFFER as i32),
        "sizing call for `{account_name}` failed: {err}"
    );

    let mut sid_buf = vec![0u8; sid_len as usize];
    let mut domain_buf = vec![0u16; domain_len as usize];
    // SAFETY: `sid_buf`/`domain_buf` are sized exactly to what the sizing
    // call reported.
    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide_name.as_ptr(),
            sid_buf.as_mut_ptr() as PSID,
            &mut sid_len,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    assert!(
        ok != 0,
        "resolve SID for `{account_name}`: {}",
        std::io::Error::last_os_error()
    );

    let mut sid_string_ptr: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid_buf` is a valid `PSID` buffer populated above;
    // `sid_string_ptr` is a valid out-param. On success it must be freed
    // with `LocalFree`.
    let ok = unsafe { ConvertSidToStringSidW(sid_buf.as_mut_ptr() as PSID, &mut sid_string_ptr) };
    assert!(
        ok != 0,
        "ConvertSidToStringSidW for `{account_name}`: {}",
        std::io::Error::last_os_error()
    );

    let mut len = 0isize;
    // SAFETY: `sid_string_ptr` is a valid null-terminated wide string per
    // `ConvertSidToStringSidW`'s contract.
    while unsafe { *sid_string_ptr.offset(len) } != 0 {
        len += 1;
    }
    // SAFETY: `sid_string_ptr[0..len)` are exactly the UTF-16 units just counted.
    let slice = unsafe { std::slice::from_raw_parts(sid_string_ptr, len as usize) };
    let result = String::from_utf16_lossy(slice);
    // SAFETY: `sid_string_ptr` was allocated by `ConvertSidToStringSidW` and
    // is freed exactly once, after being copied out above.
    unsafe {
        LocalFree(sid_string_ptr as *mut core::ffi::c_void);
    }
    result
}
