//! Shared `DaemonSpec` construction for the SCM integration tests.

use std::collections::BTreeMap;

use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};

use crate::{fixture, support};

/// The argv `type: managed` should run: this test binary itself, dispatched
/// into `fixture::service_main` — see `fixture.rs`'s module doc comment.
pub fn fixture_command(id: &str, start_port: u16, stop_port: u16, mode: &str) -> Vec<String> {
    vec![
        support::current_exe_str(),
        fixture::FIXTURE.to_string(),
        id.to_string(),
        start_port.to_string(),
        stop_port.to_string(),
        mode.to_string(),
    ]
}

/// A minimal, valid `type: managed` `DaemonSpec` running as `user`. Built
/// literally (never via `spec::resolve`) since these tests run only on
/// Windows and construct `command`/paths for this host directly.
pub fn mk_spec_as(id: &str, command: Vec<String>, env: BTreeMap<String, String>, user: User) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).expect("random_test_id produces a valid Id"),
        name: id.to_string(),
        command,
        cwd: None,
        env,
        user,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Managed,
    }
}

pub fn mk_spec(id: &str, command: Vec<String>, env: BTreeMap<String, String>) -> DaemonSpec {
    mk_spec_as(id, command, env, User::Root)
}

/// The `mk` [`goetia::manager::conformance::run`] needs: a fresh, valid
/// `type: managed` spec for any `id`. Ports are dummy — the fixture reports
/// in on a best-effort basis regardless of whether anything listens (see
/// `fixture.rs`'s `connect`), and no conformance scenario reads the report.
pub fn conformance_mk(id: &str) -> DaemonSpec {
    mk_spec(id, fixture_command(id, 1, 1, "plain"), BTreeMap::new())
}
