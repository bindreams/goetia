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
/// Compiled on every target so its tests run on Windows too; only the unix backends use it.
#[cfg(any(unix, test))]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) mod created_dirs;
pub mod launchd;
pub mod scm;
pub mod systemd;

/// The integration suites' removal of what a test created, shared with the library's unit tests;
/// they reach `tests/` only by path, and each module that included it itself would load it twice.
/// Its library users are the systemd and launchd tests, and the scratch registry key on Windows.
#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
#[path = "../tests/support/cleanup.rs"]
pub(crate) mod test_cleanup;

// `scratch.rs` reaches `test_cleanup` as `super::cleanup`, which is where `support` keeps it.
#[cfg(all(test, windows))]
use test_cleanup as cleanup;

/// The suites' registry calls that `winreg` cannot make.
#[cfg(all(test, windows))]
#[path = "../tests/support/raw_registry.rs"]
pub(crate) mod test_raw_registry;

/// The suites' scratch key under `HKLM\SOFTWARE`.
#[cfg(all(test, windows))]
#[path = "../tests/support/scratch.rs"]
pub(crate) mod test_scratch;

/// The `ELEVATED` label and `elevated` precondition of the integration suites, for the library's
/// unit tests that write real system state. On every target: each has such tests.
#[cfg(test)]
#[path = "../tests/support/elevation.rs"]
pub(crate) mod test_elevation;

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
