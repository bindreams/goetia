//! Shared `DaemonSpec` construction for the SCM integration tests.

use goetia::spec::{DaemonSpec, ServiceSpec};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};

use crate::{fixture, support};

/// [`fixture_command`], parameterized on the executable path. Needed by
/// `a_local_service_daemon_actually_runs` (`managed.rs`), which runs the
/// fixture from a copy under `%ProgramData%` (see [`support::WorldReadableExe`])
/// rather than the ordinary `target/` one — `LocalService` cannot be
/// assumed to have execute access to the repo's own build tree.
pub fn fixture_command_with_exe(exe: &str, id: &str, start_port: u16, stop_port: u16, mode: &str) -> Vec<String> {
    vec![
        exe.to_string(),
        fixture::FIXTURE.to_string(),
        id.to_string(),
        start_port.to_string(),
        stop_port.to_string(),
        mode.to_string(),
    ]
}

/// The argv `type: managed` should run: this test binary itself, dispatched
/// into `fixture::service_main` — see `fixture.rs`'s module doc comment.
pub fn fixture_command(id: &str, start_port: u16, stop_port: u16, mode: &str) -> Vec<String> {
    fixture_command_with_exe(&support::current_exe_str(), id, start_port, stop_port, mode)
}

/// The live `account_name` SCM reports for `id`, exactly as
/// `windows-service`'s `ServiceConfig` returns it — the *literal* spelling
/// SCM stored, not what `generate::canonical_account` would fold it back
/// to. `local_service_account_installs_and_round_trips` needs this raw
/// value to prove SCM itself stores the canonical spelling `install` wrote,
/// rather than merely that this backend's own readback path canonicalises
/// whatever comes back.
pub fn query_account_name(id: &str) -> Option<String> {
    let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .unwrap_or_else(|e| panic!("open SCM to query `{id}`'s config: {e}"));
    let service = scm
        .open_service(id, ServiceAccess::QUERY_CONFIG)
        .unwrap_or_else(|e| panic!("open `{id}` to query its config: {e}"));
    let cfg = service
        .query_config()
        .unwrap_or_else(|e| panic!("query config for `{id}`: {e}"));
    cfg.account_name.map(|a| a.to_string_lossy().into_owned())
}

/// The `mk` [`goetia::manager::conformance::run`] needs. Ports are dummy — the
/// fixture reports in on a best-effort basis regardless of whether anything
/// listens (see `fixture.rs`'s `connect`), and no conformance scenario reads
/// the report.
pub fn conformance_mk(id: &str) -> DaemonSpec {
    ServiceSpec::new(support::native_fields(id, fixture_command(id, 1, 1, "plain")))
        .map(DaemonSpec::from)
        .unwrap()
}

// Account SID lookup ==================================================================================================

/// The string SID for a local account name (e.g. one just created with `net user`), by the
/// library's own lookup, which drops a leading `.\`; the names tests pass have none.
pub fn sid_string_for_account(account_name: &str) -> String {
    let sid = goetia::testing::sid_for_account_name(account_name)
        .unwrap_or_else(|e| panic!("resolve the SID of `{account_name}`: {}", e.report()));
    goetia::testing::shown_sid(&sid)
}
