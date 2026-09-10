//! Shared `DaemonSpec` construction for the SCM integration tests.

use std::collections::BTreeMap;
use std::path::PathBuf;

use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};

use crate::{fixture, support};

/// [`fixture_command`], parameterized on the executable path. Needed by
/// `a_local_service_daemon_actually_runs` (`managed.rs`), which runs the
/// fixture from a copy under `%ProgramData%` (see [`WorldReadableExe`])
/// rather than the ordinary `target/` one — `LocalService` cannot be
/// assumed to have execute access to the repo's own build tree.
pub fn fixture_command_with_exe(exe: &str, id: &str, start_port: u16, stop_port: u16, mode: &str) -> Vec<String> {
    vec![
        exe.to_string(),
        fixture::FIXTURE.to_string(),
        id.to_string(),
        start_port.to_string(),
        stop_port.to_string(),
        mode.to_string(),
    ]
}

/// The argv `type: managed` should run: this test binary itself, dispatched
/// into `fixture::service_main` — see `fixture.rs`'s module doc comment.
pub fn fixture_command(id: &str, start_port: u16, stop_port: u16, mode: &str) -> Vec<String> {
    fixture_command_with_exe(&support::current_exe_str(), id, start_port, stop_port, mode)
}

/// The live `account_name` SCM reports for `id`, exactly as
/// `windows-service`'s `ServiceConfig` returns it — the *literal* spelling
/// SCM stored, not what `generate::canonical_account` would fold it back
/// to. `local_service_account_installs_and_round_trips` needs this raw
/// value to prove SCM itself stores the canonical spelling `install` wrote,
/// rather than merely that this backend's own readback path canonicalises
/// whatever comes back.
pub fn query_account_name(id: &str) -> Option<String> {
    let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .unwrap_or_else(|e| panic!("open SCM to query `{id}`'s config: {e}"));
    let service = scm
        .open_service(id, ServiceAccess::QUERY_CONFIG)
        .unwrap_or_else(|e| panic!("open `{id}` to query its config: {e}"));
    let cfg = service
        .query_config()
        .unwrap_or_else(|e| panic!("query config for `{id}`: {e}"));
    cfg.account_name.map(|a| a.to_string_lossy().into_owned())
}

/// A copy of the current test executable, placed under `%ProgramData%` with
/// an explicit grant of read+execute to `Everyone` (by well-known SID
/// `S-1-1-0`, not by localized name), removed again on drop.
///
/// `LocalService` cannot be assumed to have execute access to the repo's
/// own `target/` tree — CI checks that out under an ACL scoped to the
/// runner's own (interactive/build) account, not to every built-in service
/// identity. A plain copy is not enough on its own either: it inherits
/// `%ProgramData%`'s own ACL, which does not grant `Everyone` execute
/// either, hence the explicit `icacls` grant.
pub struct WorldReadableExe {
    path: PathBuf,
}

impl WorldReadableExe {
    pub fn new(id: &str) -> Self {
        let program_data =
            std::env::var_os("ProgramData").unwrap_or_else(|| panic!("%ProgramData% is not set on this host"));
        let path = PathBuf::from(program_data).join(format!("{id}.exe"));
        let exe = std::env::current_exe().expect("locate the test binary");
        std::fs::copy(&exe, &path).unwrap_or_else(|e| panic!("copy {} to {}: {e}", exe.display(), path.display()));

        let path_str = path
            .to_str()
            .unwrap_or_else(|| panic!("{} is not UTF-8", path.display()));
        support::cmd::run("icacls", &[path_str, "/grant", "*S-1-1-0:(RX)"]).expect_ok();

        Self { path }
    }

    pub fn path_str(&self) -> String {
        self.path
            .to_str()
            .unwrap_or_else(|| panic!("{} is not UTF-8", self.path.display()))
            .to_string()
    }
}

impl Drop for WorldReadableExe {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("WorldReadableExe[{}]: cleanup failed: {e}", self.path.display());
            }
        }
    }
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
