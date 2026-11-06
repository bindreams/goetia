//! Per-platform backends.
//!
//! Each backend is two halves. The *pure* half turns a [`DaemonSpec`] plus a
//! pre-resolved [`Identity`] into artifact text; it does no I/O, needs no
//! privileges, and is deliberately not `#[cfg]`-gated, so the systemd
//! generator compiles and its tests run on Windows. The *effectful* half
//! writes that artifact and talks to the platform's service manager, and is
//! the only `cfg`-gated, elevation-requiring code in the crate.
//!
//! [`DaemonSpec`]: crate::spec::DaemonSpec

#[cfg(unix)]
pub(crate) mod bounded;
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod client;
mod contract;
pub mod launchd;
pub mod scm;
pub mod systemd;

/// The integration suites' removal of what a test created, shared with the library's unit tests;
/// they reach `tests/` only by path, and each module that included it itself would load it twice.
/// Only Linux and macOS compile it: its library users are the systemd and launchd tests, and on
/// other targets everything in it would be dead code.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "../tests/support/cleanup.rs"]
pub(crate) mod test_cleanup;

pub(crate) use contract::{
    Backend, Decoded, InstalledDefinition, InstalledService, NativeRestart, ReadOutcome, Scan, ScanEntry, Unnamed,
    WriteOutcome,
};

/// A platform account, already resolved from [`crate::spec::User`] by the
/// effectful install path.
///
/// Resolution is I/O — launchd has no numeric-UID key, and a Windows SID
/// needs `LookupAccountSid` — so it happens before generation rather than
/// inside it. That is what keeps the generators pure, and therefore keeps
/// `generate(extract(artifact)) == artifact` checkable on any host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The account as the target platform names it: `"0"` for root on
    /// systemd, `"root"` on launchd, an account name on SCM.
    pub user: String,
}
