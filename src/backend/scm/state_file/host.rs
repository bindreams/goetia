//! The Windows half of the state file: where it lives, the boot-relative clock, and the failure
//! actions the shim reads for its own service. Everything else about the file is in the parent
//! module, which has no platform dependence and is tested everywhere.

use std::path::PathBuf;

use windows_service::service::{ServiceAccess, ServiceActionType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Wdk::System::SystemServices::KUSER_SHARED_DATA;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::{Clock, RECORD, RestartDue, dir_name, restart_due_from};

/// `%ProgramData%\Goetia\state`, beside the `logs` directory the shim already uses.
pub fn dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Goetia")
        .join("state")
}

/// The directory of service `id`: its record, the temporaries a write goes through, and its lock.
pub fn service_dir(id: &str) -> PathBuf {
    dir().join(dir_name(id))
}

/// The record of service `id`.
pub fn path(id: &str) -> PathBuf {
    service_dir(id).join(RECORD)
}

/// Where the kernel maps `KUSER_SHARED_DATA` in every process (`MM_SHARED_USER_DATA_VA`), read-only.
const SHARED_USER_DATA: usize = 0x7FFE_0000;

/// The number of this boot, from the shared page every process has mapped: no access check
/// applies, unlike the System process, which only an administrator can open. It counts boots, so
/// a change of the wall clock does not move it. (On the measured host it equals the `BootId` the
/// prefetcher keeps in the registry.)
fn boot_identity() -> u64 {
    let shared = SHARED_USER_DATA as *const KUSER_SHARED_DATA;
    // SAFETY: the kernel maps this page at this address, readable, in every process for as long as
    // it lives, and the field is a plain `u32` the kernel wrote before any process started.
    u64::from(unsafe { std::ptr::read_volatile(&raw const (*shared).BootId) })
}

/// Now, on the clock every record is written against.
pub fn clock() -> Clock {
    Clock {
        boot: boot_identity(),
        // SAFETY: takes no arguments and cannot fail.
        ms: unsafe { GetTickCount64() },
    }
}

/// What a failure of service `id` reported now queues, from its failure actions.
///
/// `QUERY_CONFIG` is all this needs, which every account has on its own service by default.
pub fn restart_due(id: &str) -> Result<RestartDue, windows_service::Error> {
    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = scm.open_service(id, ServiceAccess::QUERY_CONFIG)?;
    let flag = service.get_failure_actions_on_non_crash_failures()?;
    let actions = service.get_failure_actions()?;
    let delays: Vec<_> = actions
        .actions
        .iter()
        .flatten()
        .filter(|action| action.action_type == ServiceActionType::Restart)
        .map(|action| action.delay)
        .collect();
    Ok(restart_due_from(flag, &delays))
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod host_tests;
