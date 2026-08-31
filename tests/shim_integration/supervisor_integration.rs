//! Step 2 of Task 14: the shim's own behavior, exercised through a real,
//! elevated SCM install/start/stop — the four failure paths the plan calls
//! out are `stop_kills_the_whole_process_tree` (a commanded stop must not
//! respawn, and must reach the whole tree) and
//! `unreadable_blob_logs_to_fallback_path_and_event_log` (version skew);
//! `shim_runs_child_with_cwd_and_env`/`shim_captures_stdout_to_logs_path`
//! cover the ordinary spawn path these two failure tests assume works.

use std::collections::BTreeMap;

use goetia::backend::scm::manager::ScmManager;
use goetia::manager::ServiceManager as _;

use crate::common::{fixture_command, mk_spec_full};
use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard};

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn shim_runs_child_with_cwd_and_env() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();
    let tmp = tempfile::tempdir().expect("tempdir for cwd");

    let mut env = BTreeMap::new();
    env.insert("GOETIA_SHIM_ENV_PROBE".to_string(), "probe-value".to_string());
    let spec = mk_spec_full(
        &id,
        fixture_command("cwd-env", &[&reported.port().to_string(), "GOETIA_SHIM_ENV_PROBE"]),
        Some(tmp.path().to_path_buf()),
        env,
        goetia::spec::Restart::Never,
        None,
        None,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");

    let line = reported.accept_line("the fixture to report its cwd and env");
    mgr.stop(&spec.id).expect("stop");

    let (cwd_part, env_part) = line
        .split_once(';')
        .unwrap_or_else(|| panic!("malformed report: {line}"));
    let reported_cwd = cwd_part.strip_prefix("cwd=").expect("cwd= prefix");
    let reported_env = env_part.strip_prefix("env=").expect("env= prefix");

    // Canonicalize both sides: Windows may report a `\\?\`-prefixed or
    // short/long path differently than the one handed to `cwd:`.
    let expected = std::fs::canonicalize(tmp.path()).expect("canonicalize expected cwd");
    let actual =
        std::fs::canonicalize(reported_cwd).unwrap_or_else(|e| panic!("canonicalize reported cwd {reported_cwd}: {e}"));
    assert_eq!(actual, expected, "child did not run with the configured cwd");
    assert_eq!(reported_env, "probe-value", "child did not see the configured env var");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn shim_captures_stdout_to_logs_path() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();
    let tmp = tempfile::tempdir().expect("tempdir for logs");
    let log_path = tmp.path().join("daemon.log");
    const MARKER: &str = "goetia-shim-log-capture-probe";

    let spec = mk_spec_full(
        &id,
        fixture_command("write-log", &[&reported.port().to_string(), MARKER]),
        None,
        BTreeMap::new(),
        goetia::spec::Restart::Never,
        None,
        Some(log_path.clone()),
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");
    reported.accept("the fixture to finish writing its log lines");
    mgr.stop(&spec.id).expect("stop");

    let contents =
        std::fs::read_to_string(&log_path).unwrap_or_else(|e| panic!("read captured log {}: {e}", log_path.display()));
    let occurrences = contents.matches(MARKER).count();
    assert!(
        occurrences >= 2,
        "expected the marker written to both stdout and stderr (merged) to appear at least twice, got \
         {occurrences} in:\n{contents}"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn stop_kills_the_whole_process_tree() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let child_reported = ConnectBack::listen();
    let grandchild_reported = ConnectBack::listen();

    let spec = mk_spec_full(
        &id,
        fixture_command(
            "spawn-grandchild",
            &[
                &child_reported.port().to_string(),
                &grandchild_reported.port().to_string(),
            ],
        ),
        None,
        BTreeMap::new(),
        goetia::spec::Restart::Never,
        None,
        None,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");
    child_reported.accept("the direct child to start");
    grandchild_reported.accept("the grandchild to start");

    // The synchronous barrier `ConnectBack::connected_yet`'s own doc
    // comment requires: `stop` blocks (via a real `NotifyServiceStatusChangeW`
    // wait, `backend::scm::manager::scm_wait`) until SCM confirms
    // `SERVICE_STOPPED`, and the shim does not report that until AFTER it
    // has already decided not to respawn (see `service::supervisor_loop`'s
    // `Stopping` arm) — so nothing after this call can still be racing a
    // decision made before it returned.
    mgr.stop(&spec.id).expect("stop");

    assert!(
        !child_reported.connected_yet(),
        "a replacement direct child connected after the stop was confirmed — the naive respawn-outside-\
         the-terminated-job bug"
    );
    assert!(
        !grandchild_reported.connected_yet(),
        "a replacement grandchild connected after the stop was confirmed"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn unreadable_blob_logs_to_fallback_path_and_event_log() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let spec = mk_spec_full(
        &id,
        fixture_command("report", &["1"]),
        None,
        BTreeMap::new(),
        goetia::spec::Restart::Never,
        None,
        None,
    );
    mgr.install(&spec, false).expect("install");

    corrupt_spec_value(&id);

    let err = mgr
        .start(&spec.id)
        .expect_err("a shim that cannot decode its own metadata blob must fail to start, not hang");
    let _ = err;

    let fallback_path = programdata_dir().join("Goetia").join("logs").join(format!("{id}.log"));
    let logged = std::fs::read_to_string(&fallback_path)
        .unwrap_or_else(|e| panic!("read fallback log {}: {e}", fallback_path.display()));
    assert!(
        logged.contains(&id) && logged.to_lowercase().contains("decode"),
        "fallback log at {} does not look like a decode-failure report naming `{id}`:\n{logged}",
        fallback_path.display()
    );

    let events = support::cmd::run("wevtutil.exe", &["qe", "Application", "/rd:true", "/c:200", "/f:text"]);
    assert!(
        events.ok() && events.stdout.contains(&id),
        "Windows Event Log (Application) has no entry naming `{id}`:\n{events}"
    );
}

fn programdata_dir() -> std::path::PathBuf {
    std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"))
}

/// Overwrite `Services\<id>\Parameters\Spec` with undecodable text, leaving
/// `Marker` intact — the version-skew shape this test targets: something
/// that is unambiguously ours (so `install`/`start` do not refuse it as
/// foreign) but whose `Spec` an old shim (or any shim) cannot decode.
fn corrupt_spec_value(id: &str) {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_WRITE};

    let path = format!(r"SYSTEM\CurrentControlSet\Services\{id}\Parameters");
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_WRITE)
        .unwrap_or_else(|e| panic!("open {path} to corrupt Spec: {e}"));
    key.set_value("Spec", &"not valid base64 or JSON at all".to_string())
        .unwrap_or_else(|e| panic!("overwrite Spec under {path}: {e}"));
}
