//! Shared support for the integration suites.
//!
//! Pulled in with `#[path = "support/mod.rs"] mod support;`. Only
//! `daemon_spec` depends on the `goetia` crate; the marker-inertness probe uses
//! the rest, which runs before any backend exists.

#![allow(dead_code, unused_imports)]
// Each probe module is `#[cfg]`-gated to one platform, so most of this is
// unused in any single build.

pub mod cleanup;
pub mod cmd;
pub mod connect_back;
pub mod daemon_spec;
pub mod kill;
#[cfg(windows)]
pub mod local_user;
#[cfg(windows)]
pub mod registry;
pub mod sentinel;
#[cfg(windows)]
pub mod service_ctrl;
pub mod service_guard;
#[cfg(windows)]
pub mod world_readable_exe;

use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
// rand 0.10 moved `random` off `Rng` onto `RngExt`.
use rand::RngExt as _;

pub use connect_back::ConnectBack;
pub use daemon_spec::spec;
#[cfg(windows)]
pub use local_user::LocalUserGuard;
pub use service_guard::ServiceGuard;
#[cfg(windows)]
pub use world_readable_exe::WorldReadableExe;

// Artifact locations --------------------------------------------------------------------------------------------------
// Shared with `service_guard`, whose cleanup has to reach the very paths
// the probes wrote to. A second copy desyncs on a one-sided edit, and the
// symptom is a straggler left behind rather than an error.

pub const SYSTEMD_UNIT_DIR: &str = "/etc/systemd/system";
pub const LAUNCHD_DAEMON_DIR: &str = "/Library/LaunchDaemons";
pub const SCM_SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";

/// Opt-out marker for the probes, all of which register real system services:
/// `SKULD_LABELS='!elevated'`.
#[skuld::label]
pub const ELEVATED: skuld::Label;

/// Skuld precondition. CI runs the test binaries under elevation precisely so
/// this never reports unavailable there — an unmet precondition in CI is a
/// silent skip of the tests that gate the metadata design.
pub fn elevated() -> Result<(), String> {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } == 0 {
            Ok(())
        } else {
            Err("registers real system services; re-run the test binary under sudo".to_string())
        }
    }
    #[cfg(windows)]
    {
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        // The capability, not the token: creating a service is exactly what
        // these probes need and exactly what a filtered admin token refuses.
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS)
            .map(drop)
            .map_err(|e| format!("registers real system services; re-run elevated (SCM open failed: {e})"))
    }
}

/// Fails the test unless `dir`, a directory the system provides (`multi-user.target.wants`,
/// `/Library/LaunchDaemons`), exists. A test never creates one: every test running concurrently
/// shares it.
#[cfg(unix)]
pub fn require_system_dir(dir: &std::path::Path) {
    assert!(dir.is_dir(), "the system directory {} is missing", dir.display());
}

/// Test ids are `goetia-test-<random>`. Random rather than derived from the
/// pid so a straggler from a crashed run can never collide with a live one.
pub fn random_test_id() -> String {
    format!("goetia-test-{:016x}", rand::rng().random::<u64>())
}

/// `s` as NUL-terminated UTF-16, for the wide Win32 entry points.
#[cfg(windows)]
pub fn wide_null(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The host's `%ProgramData%`.
#[cfg(windows)]
pub fn program_data() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("%ProgramData% is not set on this host"))
}

/// Where the shim of service `id` writes a failure it reports before it has read the daemon's
/// config: `<program_data>\Goetia\logs\<id>.log`. `program_data` is the real `%ProgramData%`
/// ([`program_data`]), or the one a test gave the service through its `Environment` value.
#[cfg(windows)]
pub fn shim_default_log(program_data: &std::path::Path, id: &str) -> PathBuf {
    program_data.join("Goetia").join("logs").join(format!("{id}.log"))
}

/// Base64 the size of a worst-case spec blob. systemd historically applied a
/// 2048-byte `LINE_MAX` to unit-file lines and truncated past it, and `Spec=`
/// is a single line whose length grows with `env` — so a token-sized value
/// would probe the wrong thing entirely.
pub fn probe_blob() -> String {
    let bytes: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
    let blob = BASE64.encode(&bytes);
    assert!(
        blob.len() >= 8 * 1024,
        "probe blob is {} bytes, want >= 8 KiB",
        blob.len()
    );
    blob
}

pub fn current_exe_str() -> String {
    let exe = std::env::current_exe().expect("locate the test binary");
    exe.to_str()
        .unwrap_or_else(|| panic!("test binary path is not UTF-8: {}", exe.display()))
        .to_string()
}

/// Where measured (as opposed to asserted) outcomes are written. CI uploads
/// this directory, so a finding that no green build depends on still cannot
/// decay into one nobody ever looks at again.
pub fn probe_results_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("probe-results")
}

pub fn record_probe(name: &str, body: &str) {
    let dir = probe_results_dir();
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    let path = dir.join(format!("{name}.txt"));
    fs::write(&path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    eprintln!("--- probe result: {name} ---\n{body}");
}
