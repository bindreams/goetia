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
pub(crate) mod client;
mod contract;
pub mod launchd;
pub mod scm;
pub(crate) mod standby;
pub mod systemd;
#[cfg(test)]
pub(crate) mod test_allowance;
mod unreaped;

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

/// What a generator answers for a task until its backend's task support lands.
pub(crate) fn cannot_run_tasks(backend: &str, id: &crate::spec::Id) -> crate::error::Error {
    crate::spec::invalid(id, &format!("{backend} cannot run tasks yet"))
}
