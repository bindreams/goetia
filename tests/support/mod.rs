//! Shared support for the marker-inertness probe.
//!
//! Pulled in with `#[path = "support/mod.rs"] mod support;`. Nothing here
//! depends on the `goetia` crate: the probe runs before any backend exists.

#![allow(dead_code, unused_imports)]
// Each probe module is `#[cfg]`-gated to one platform, so most of this is
// unused in any single build.

pub mod cmd;
pub mod connect_back;
pub mod sentinel;
pub mod service_guard;

use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::Rng as _;

pub use connect_back::ConnectBack;
pub use service_guard::ServiceGuard;

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

/// Global constraint: test ids are `goetia-test-<random>`. Random rather than
/// derived from the pid so a straggler from a crashed run can never collide
/// with a live one.
pub fn random_test_id() -> String {
    format!("goetia-test-{:016x}", rand::rng().random::<u64>())
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
