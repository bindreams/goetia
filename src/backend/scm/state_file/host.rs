//! The Windows half of the state file: where it lives, the boot-relative clock, and the failure
//! actions the shim reads for its own service. Everything else about the file is in the parent
//! module, which has no platform dependence and is tested everywhere.

use std::path::PathBuf;

use cosca::identity::{ProcessId, Resolved};
use windows_service::service::{ServiceAccess, ServiceActionType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::{Clock, RestartDue, restart_due_from};

/// `%ProgramData%\Goetia\state`, beside the `logs` directory the shim already uses.
pub fn dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Goetia")
        .join("state")
}

/// The state file of service `id`.
pub fn path(id: &str) -> PathBuf {
    dir().join(id)
}

/// The system process: it starts at boot and its creation time is an absolute timestamp, so it
/// names the boot and is unaffected by a later change of the wall clock.
const SYSTEM_PID: u32 = 4;

/// The identity of the current boot, or `None` if the OS refuses to say.
fn boot_identity() -> Option<u64> {
    match ProcessId::of(SYSTEM_PID) {
        Resolved::Found(system) => system.to_record().ok().map(|record| record.token),
        Resolved::Gone | Resolved::Unknown => None,
    }
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
