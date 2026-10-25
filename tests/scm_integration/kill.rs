//! `kill` for a caller whose `SeDebugPrivilege` is held but disabled, which is an elevated
//! administrator's token. CI's runner token has it enabled, so without this a `kill` that only works
//! with the privilege on would pass CI and fail for a real user.
//!
//! The privilege is disabled in a copy of the token on the calling thread alone (an impersonation of
//! the process itself), so no other test in the process sees it; the access check for `OpenProcess`
//! uses that thread token.

use std::collections::BTreeMap;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::Duration;

use goetia::backend::scm::manager::ScmManager;
use goetia::manager::{Budget, ServiceManager as _, State};
use goetia::spec::{Restart, User};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, GetLastError, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, ImpersonateSelf, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, RevertToSelf, SE_DEBUG_NAME,
    SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use crate::common::{self, fixture_command_with_exe};
use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard};

/// While alive, the calling thread has `SeDebugPrivilege` disabled (still held, as for an elevated
/// administrator), and reverts to the process token on drop.
struct DebugPrivilegeDisabled;

impl DebugPrivilegeDisabled {
    fn new() -> Self {
        // SAFETY: plain Win32 calls on this thread, with pointers to locals that outlive them.
        unsafe {
            assert_ne!(ImpersonateSelf(SecurityImpersonation), 0, "ImpersonateSelf");
            let guard = Self;

            let mut token = std::ptr::null_mut();
            assert_ne!(
                OpenThreadToken(GetCurrentThread(), TOKEN_ADJUST_PRIVILEGES, 1, &mut token),
                0,
                "OpenThreadToken: {}",
                std::io::Error::last_os_error()
            );
            let token = OwnedHandle::from_raw_handle(token);

            let mut luid = LUID {
                LowPart: 0,
                HighPart: 0,
            };
            assert_ne!(
                LookupPrivilegeValueW(std::ptr::null(), SE_DEBUG_NAME, &mut luid),
                0,
                "LookupPrivilegeValueW: {}",
                std::io::Error::last_os_error()
            );
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: 0,
                }],
            };
            assert_ne!(
                AdjustTokenPrivileges(
                    token.as_raw_handle(),
                    0,
                    &privileges,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                0,
                "AdjustTokenPrivileges: {}",
                std::io::Error::last_os_error()
            );
            // `AdjustTokenPrivileges` succeeds when the token does not hold the privilege at all and
            // says so only here; the test is about a held, disabled one.
            assert_eq!(
                GetLastError(),
                ERROR_SUCCESS,
                "the test token must hold SeDebugPrivilege (an elevated administrator's does)"
            );
            guard
        }
    }
}

impl Drop for DebugPrivilegeDisabled {
    fn drop(&mut self) {
        // SAFETY: no arguments; ends the impersonation begun in `new`.
        assert_ne!(unsafe { RevertToSelf() }, 0, "RevertToSelf");
    }
}

/// Install `user`'s daemon, start it, kill it with the privilege disabled, and see it end.
fn kill_with_the_privilege_disabled(user: User) {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    // LocalService cannot execute the repo's own build tree; see `WorldReadableExe`.
    let exe = common::WorldReadableExe::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    let spec = common::mk_spec_full(
        &id,
        fixture_command_with_exe(&exe.path_str(), &id, started.port(), stopped.port(), "plain"),
        BTreeMap::new(),
        user,
        Restart::Never,
        Some(Duration::from_secs(1)),
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::Unbounded).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    {
        let _disabled = DebugPrivilegeDisabled::new();
        mgr.kill(&spec.id)
            .expect("kill with SeDebugPrivilege held but disabled");
    }

    support::kill::until(|| {
        matches!(
            mgr.status(&spec.id).expect("status").state,
            State::Stopped | State::Failed
        )
    });
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn kill_works_for_a_local_system_service_without_the_debug_privilege_enabled() {
    kill_with_the_privilege_disabled(User::Root);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn kill_works_for_a_local_service_service_without_the_debug_privilege_enabled() {
    kill_with_the_privilege_disabled(User::Name("LocalService".to_string()));
}
