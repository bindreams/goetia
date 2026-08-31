//! Shared `DaemonSpec` construction for the `type: simple` integration tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};

use crate::fixture;
use crate::support;

pub fn fixture_command(mode: &str, args: &[&str]) -> Vec<String> {
    let mut v = vec![
        support::current_exe_str(),
        fixture::FIXTURE.to_string(),
        mode.to_string(),
    ];
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

#[allow(clippy::too_many_arguments)]
pub fn mk_spec_full(
    id: &str,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    restart: Restart,
    restart_delay: Option<Duration>,
    logs: Option<PathBuf>,
) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).expect("random_test_id/short local ids are valid Ids"),
        name: id.to_string(),
        command,
        cwd,
        env,
        user: User::Root,
        restart,
        restart_delay,
        logs,
        kind: Kind::Simple,
    }
}

pub fn mk_spec(id: &str, command: Vec<String>) -> DaemonSpec {
    mk_spec_full(id, command, None, BTreeMap::new(), Restart::Never, None, None)
}

/// The `mk` [`goetia::manager::conformance::run`] needs: a fresh, valid
/// `type: simple` spec for any `id`. The port is dummy — the fixture
/// reports in on a best-effort basis regardless of whether anything
/// listens, and no conformance scenario reads the report.
pub fn conformance_mk(id: &str) -> DaemonSpec {
    mk_spec(id, fixture_command("report", &["1"]))
}
