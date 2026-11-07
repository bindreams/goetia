//! The real elevation check, one per platform.
//!
//! `main.rs` wires this into `dispatch`'s `is_elevated`; tests inject their
//! own closure, since a unit test cannot construct an elevated or unelevated
//! process.

/// Whether the current process has root/Administrator privileges.
#[cfg(unix)]
pub fn is_elevated() -> bool {
    // SAFETY: `geteuid` takes no arguments, reads no memory, and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Whether the current process has root/Administrator privileges.
///
/// Probes the capability rather than the token: opening SCM with
/// `ALL_ACCESS` is exactly what a mutating subcommand needs and exactly
/// what a filtered (non-elevated) admin token refuses. The same technique
/// `tests/support/mod.rs`'s `elevated()` precondition already uses.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS).is_ok()
}

#[cfg(not(any(unix, windows)))]
pub fn is_elevated() -> bool {
    false
}
