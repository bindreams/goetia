//! The `backend-specific:` manifest key's namespace: which service-manager
//! backend a set of overrides applies to.
//!
//! Lives under `spec` rather than `crate::backend` because its job is
//! parsing a manifest key, not artifact generation — `crate::backend` is
//! the tree that turns a resolved [`crate::spec::DaemonSpec`] into a real
//! systemd unit, launchd plist, or SCM registration.

use serde::Deserialize;

/// One of the three service-manager backends a `backend-specific:` entry
/// can target.
///
/// Declared in alphabetical order: serde's `unknown_variant` error lists
/// variants in declaration order, and this order also fixes the order
/// [`Backend::ALL`] iterates in, which in turn fixes the order any warning
/// keyed off it is emitted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Launchd,
    Scm,
    Systemd,
}

impl Backend {
    /// Every variant, in the same alphabetical order they are declared in.
    pub const ALL: [Backend; 3] = [Backend::Launchd, Backend::Scm, Backend::Systemd];

    /// The backend for the platform this binary is running on, or `None` on
    /// a platform with no service manager Goetia supports.
    ///
    /// Must agree with [`crate::manager::native`]'s supported set — see
    /// `native_backend_agrees_with_the_platforms_that_have_a_service_manager`
    /// in `backend_tests.rs`.
    pub const fn native() -> Option<Backend> {
        #[cfg(target_os = "linux")]
        {
            Some(Backend::Systemd)
        }
        #[cfg(target_os = "macos")]
        {
            Some(Backend::Launchd)
        }
        #[cfg(target_os = "windows")]
        {
            Some(Backend::Scm)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            None
        }
    }

    /// The manifest key's spelling for this variant: `systemd`, `launchd`,
    /// `scm`. Round-trips through [`Deserialize`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Launchd => "launchd",
            Backend::Scm => "scm",
            Backend::Systemd => "systemd",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;
