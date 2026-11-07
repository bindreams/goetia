//! The Windows elevation check, which the library needs as well as the CLI: it is not behind the
//! `cli` feature.

/// Whether the current process has Administrator privileges.
///
/// Probes the capability rather than the token: opening SCM with
/// `ALL_ACCESS` is exactly what a mutating subcommand needs and exactly
/// what a filtered (non-elevated) admin token refuses. The same technique
/// `tests/support/mod.rs`'s `elevated()` precondition already uses.
pub fn is_elevated() -> bool {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS).is_ok()
}
