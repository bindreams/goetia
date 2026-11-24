//! Shared support for the integration suites.
//!
//! Pulled in with `#[path = "support/mod.rs"] mod support;`. Helpers the library's unit tests also
//! use live in `goetia::testing` (the `testing` feature) and are re-exported here.

#![allow(dead_code, unused_imports)]
// Each probe module is `#[cfg]`-gated to one platform, so most of this is
// unused in any single build.

pub mod cmd;
pub mod connect_back;
#[cfg(unix)]
pub mod cron_oracle;
// Runs `CARGO_BIN_EXE_goetia`, which only a build with the `cli` feature has.
#[cfg(feature = "cli")]
pub mod demonstration;
pub mod failed_install;
pub mod kill;
pub mod list_json;
#[cfg(windows)]
pub mod local_user;
#[cfg(windows)]
pub mod registry;
#[cfg(windows)]
pub mod scm_wait;
pub mod sentinel;
#[cfg(windows)]
pub mod service_ctrl;
pub mod service_guard;
#[cfg(unix)]
pub mod tasks;
pub mod wait;
#[cfg(windows)]
pub mod windows_tasks;
#[cfg(windows)]
pub mod world_readable_exe;

use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
// rand 0.10 moved `random` off `Rng` onto `RngExt`.
use rand::RngExt as _;

pub use connect_back::ConnectBack;
pub use failed_install::assert_failed_on_a_path_under_a_file;
#[cfg(unix)]
use goetia::spec::{DaemonSpec, ServiceSpec, StopAction};
pub use goetia::testing::groups::{Group, launchd_group, scm_group, systemd_group};
#[cfg(unix)]
use goetia::testing::service_fields;
#[cfg(windows)]
pub use goetia::testing::wide_null;
pub use goetia::testing::{base_fields, cleanup};
#[cfg(windows)]
pub use goetia::testing::{
    raw_registry,
    scratch::{self, ScratchKey},
};
pub use list_json::{ListJson, assert_own, list_json, may_account_for};
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
/// Where systemd reads runtime unit files, which a stand-in for a left run is written to.
pub const SYSTEMD_RUNTIME_UNIT_DIR: &str = "/run/systemd/system";
/// Where systemd keeps a `Persistent=yes` timer's stamp, `stamp-<id>.timer`.
pub const SYSTEMD_STAMP_DIR: &str = "/var/lib/systemd/timers";
pub const LAUNCHD_DAEMON_DIR: &str = "/Library/LaunchDaemons";
pub const SCM_SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";

/// A manager whose `protocol: plain` services run the stock shim this package builds, wherever cargo put
/// it, through a `StandaloneShim`. The folder check is off: the build directory is the user's, not a
/// protected one. `Scm::new()` has no provider, so a test that installs a `protocol: plain` service
/// without caring where its host comes from uses this.
#[cfg(windows)]
pub fn stock_shim_manager() -> goetia::backend::scm::manager::Scm {
    shim_manager_at(PathBuf::from(env!("CARGO_BIN_EXE_goetia-shim")))
}

/// [`stock_shim_manager`] for the shim at `shim`: a copy that an account which cannot run the build tree
/// can run (see [`WorldReadableExe`]).
#[cfg(windows)]
pub fn shim_manager_at(shim: PathBuf) -> goetia::backend::scm::manager::Scm {
    goetia::backend::scm::manager::Scm::with_shim(
        goetia::shim::StandaloneShim::new(shim, vec![]),
        goetia::shim::CheckPermissions::Disable,
    )
}

/// Fails the test unless `dir`, a directory the system provides (`multi-user.target.wants`,
/// `/Library/LaunchDaemons`), exists. A test never creates one: every test running concurrently
/// shares it.
#[cfg(unix)]
pub fn require_system_dir(dir: &std::path::Path) {
    assert!(dir.is_dir(), "the system directory {} is missing", dir.display());
}

/// `base`, a service, run as `command` and stopping by `stop_action`: the daemon the stop-action tests of
/// the launchd and systemd suites start.
#[cfg(unix)]
pub fn stopping_by(base: DaemonSpec, command: Vec<String>, stop_action: StopAction) -> DaemonSpec {
    let mut fields = service_fields(base);
    fields.common.command = command;
    fields.stop_action = stop_action;
    ServiceSpec::new(fields).map(DaemonSpec::from).unwrap()
}

/// Test ids are `goetia-test-<random>`. Random rather than derived from the
/// pid so a straggler from a crashed run can never collide with a live one.
pub fn random_test_id() -> String {
    format!("goetia-test-{:016x}", rand::rng().random::<u64>())
}

/// The host's `%ProgramData%`, as the library locates it.
#[cfg(windows)]
pub fn program_data() -> PathBuf {
    goetia::testing::program_data()
}

/// A new folder under `%ProgramData%` that the check of a daemon's own `logs:` passes: owned by Administrators
/// (this process is elevated), and only SYSTEM, Administrators and the accounts `modify` (SIDs as `S-1-5-...`)
/// hold rights in it, inherited by what is made in it. The folder is removed when the guard drops; the guard is
/// made before the folder, so a failed creation still leaves nothing. A temporary directory of the user's cannot
/// serve: the user's profile is a folder that user can change, which the check refuses on the way to the log.
#[cfg(windows)]
pub fn protected_dir(modify: &[String]) -> cleanup::DirGuard {
    let path = program_data().join(random_test_id());
    let guard = cleanup::DirGuard::new(path.clone());
    std::fs::create_dir(&path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    let path_str = path
        .to_str()
        .unwrap_or_else(|| panic!("{} is not UTF-8", path.display()));
    let mut grants = vec!["*S-1-5-18:(OI)(CI)F".to_string(), "*S-1-5-32-544:(OI)(CI)F".to_string()];
    grants.extend(modify.iter().map(|sid| format!("*{sid}:(OI)(CI)M")));
    let mut args = vec![path_str, "/inheritance:r", "/grant:r"];
    args.extend(grants.iter().map(String::as_str));
    cmd::run("icacls", &args).expect_ok();
    guard
}

/// Where the shim of task `id` writes its run records: `<program_data>\Goetia\runs\<id>`.
#[cfg(windows)]
pub fn task_records(id: &str) -> PathBuf {
    program_data().join("Goetia").join("runs").join(id)
}

/// Where the shim of service `id` writes a failure it reports before it has read the daemon's
/// config: `<program_data>\Goetia\logs\<id>.log`, with the machine's `%ProgramData%` ([`program_data`]).
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
