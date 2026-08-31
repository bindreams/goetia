//! The `ServiceManager` seam: one trait implemented by each real backend
//! (Tasks 11-13) and by [`fake::Fake`], the in-memory test double every CLI
//! and policy test in this crate runs against instead of a real
//! systemd/launchd/SCM.
//!
//! The trait exists so the CLI is written once and tested without touching a
//! real machine — not for runtime backend swapping. Every implementation's
//! `install` must call [`crate::decide::decide`] rather than restate any row
//! of its policy table itself; see [`conformance::run`], which asserts that
//! contract against any `&dyn ServiceManager`.

pub mod conformance;
pub mod fake;

use crate::error::{Error, Result};
use crate::spec::{DaemonSpec, Id};

// ServiceManager ======================================================================================================

/// One platform's service manager, seen through the operations Goetia needs.
///
/// `install`/`uninstall`/`enable`/`disable`/`start`/`stop` are the mutating
/// verbs; `status`/`list` are read-only. The CLI checks elevation itself
/// before calling any mutating verb — implementations do not re-check it.
pub trait ServiceManager {
    /// Install (create or update) the service for `spec`. Never starts and
    /// never enables it at boot — see the crate-level design notes on
    /// install-off-by-default. Routes through [`crate::decide::decide`]; see
    /// that function's doc comment for what each [`crate::decide::Outcome`]
    /// means and when `force` is honored.
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<crate::decide::Outcome>;

    /// Stop the service if running, remove the artifact, and reload the
    /// manager. Operates by id alone — no manifest needed, so it still works
    /// on a machine where the original `goetia.yaml` clone is gone.
    fn uninstall(&self, id: &Id) -> Result<()>;

    /// Enable the service at boot. Does not start it. `Err(NotInstalled)` if
    /// `id` is not managed by Goetia.
    fn enable(&self, id: &Id) -> Result<()>;

    /// Disable the service at boot. Does not stop it if running.
    fn disable(&self, id: &Id) -> Result<()>;

    /// Start the service now. Does not change its boot-enablement.
    fn start(&self, id: &Id) -> Result<()>;

    /// Stop the service now. Does not change its boot-enablement.
    fn stop(&self, id: &Id) -> Result<()>;

    /// The live state of one installed service.
    fn status(&self, id: &Id) -> Result<Status>;

    /// Every Goetia-managed service currently installed. A foreign
    /// (unmarked) service at some id is never included; see [`Installed`]
    /// for what happens when a marked one exists but its blob will not
    /// decode.
    fn list(&self) -> Result<Vec<Installed>>;
}

// Installed / Status / State ==========================================================================================

/// One entry from [`ServiceManager::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// A Goetia-marked service whose blob decoded successfully.
    Ours {
        spec: DaemonSpec,
        state: State,
        enabled: bool,
    },
    /// Ours by marker, but the blob will not decode — a newer schema, or
    /// corruption. Kept distinct from a silent omission: a single artifact
    /// written by a newer Goetia must not take down `list`/`status` for
    /// every other daemon, which is exactly what dropping this entry
    /// silently would do.
    OursUnreadable { name: String, reason: String },
}

/// The live state of one installed service, as [`ServiceManager::status`]
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// For `type: simple` on Windows this is `goetia-shim.exe`'s pid, not
    /// the supervised child's — SCM knows no other process for that
    /// service. Documented here rather than left as a surprise once Task 14
    /// lands.
    pub pid: Option<u32>,
    /// Whether the service is enabled at boot: systemd's `.wants` symlink,
    /// the launchd plist's directory, or SCM's `SERVICE_AUTO_START`. Not
    /// spec data (see the crate-level design notes on boot-enablement) —
    /// this reports the *installation's* current state, queried fresh each
    /// call, never cached from `install`. Present specifically so
    /// `manager::conformance` can assert `install` never enables and that a
    /// re-install never changes it; every real backend must be able to
    /// answer this without rebooting (systemd: `systemctl is-enabled`;
    /// launchd: which directory the plist lives in; SCM: `dwStartType`).
    pub enabled: bool,
}

/// A service's run state, as the platform's manager reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Stopped,
    Failed,
    Unknown,
}

// native ==============================================================================================================

/// The platform's [`ServiceManager`] implementation.
///
/// Tasks 11-13 build systemd/launchd/SCM backends; until each lands, its
/// platform's arm here returns [`Error::UnsupportedPlatform`] rather than
/// panicking — a CLI user gets a diagnosable message ("no backend for linux
/// yet"), not a crash. Each of those tasks replaces only its own arm.
pub fn native() -> Result<Box<dyn ServiceManager>> {
    #[cfg(target_os = "linux")]
    {
        Err(Error::UnsupportedPlatform {
            platform: "linux".to_string(),
        })
    }
    #[cfg(target_os = "macos")]
    {
        Err(Error::UnsupportedPlatform {
            platform: "macos".to_string(),
        })
    }
    #[cfg(target_os = "windows")]
    {
        Err(Error::UnsupportedPlatform {
            platform: "windows".to_string(),
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(Error::UnsupportedPlatform {
            platform: std::env::consts::OS.to_string(),
        })
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
