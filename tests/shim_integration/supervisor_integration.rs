//! The shim's own behavior, exercised through a real, elevated SCM
//! install/start/stop — the two central failure paths this file covers are
//! `stop_kills_the_whole_process_tree` (a commanded stop must not respawn,
//! and must reach the whole tree) and
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

    // `restart: always`, not `never`: with `never`, `decide_restart` would
    // return `Stop` for every input regardless of the `stopping` flag, so
    // "no replacement child appears" would hold trivially even if the
    // `stopping`-checked-first logic this test exists to guard were deleted
    // outright. `always` is the policy that would actually respawn a
    // replacement absent that check.
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
        goetia::spec::Restart::Always,
        None,
        None,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");
    let child_pid = child_reported.accept_line("the direct child to start and report its pid");
    let grandchild_pid = grandchild_reported.accept_line("the grandchild to start and report its pid");

    // The synchronous barrier `ConnectBack::connected_yet`'s own doc
    // comment requires: `stop` blocks (via a real `NotifyServiceStatusChangeW`
    // wait, `backend::scm::manager::scm_wait`) until SCM confirms
    // `SERVICE_STOPPED`, and the shim does not report that until AFTER it
    // has already decided not to respawn (see `service::supervisor_loop`'s
    // `Stopping` arm) — so nothing after this call can still be racing a
    // decision made before it returned.
    mgr.stop(&spec.id).expect("stop");

    // The namesake claim: the stop reached the *whole* tree, not merely
    // "no new connection arrived" (which a surviving-but-silent process
    // would also satisfy, since both fixture modes only ever connect once,
    // at startup).
    assert!(
        !pid_is_alive(&child_pid),
        "direct child pid {child_pid} is still alive after the stop was confirmed"
    );
    assert!(
        !pid_is_alive(&grandchild_pid),
        "grandchild pid {grandchild_pid} is still alive after the stop was confirmed"
    );

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

/// `tasklist /FI "PID eq <pid>"` prints an informational "no tasks found"
/// line (not a CSV row) when nothing matches, so a matching row is
/// distinguished by the output actually containing `pid` as a field —
/// `/FO CSV` quotes each field, so a literal `"<pid>"` substring is
/// unambiguous for the range of pids a test process ever reports (never
/// contained inside another field like an image name).
fn pid_is_alive(pid: &str) -> bool {
    let out = support::cmd::run("tasklist.exe", &["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
    out.ok() && out.stdout.contains(&format!("\"{pid}\""))
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn on_failure_respawns_after_a_nonzero_exit() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();

    // No `restart-delay` — exercises the real end-to-end wiring for
    // `supervisor::DEFAULT_RESTART_DELAY`, not only the pure
    // `missing_restart_delay_uses_the_documented_default` unit test.
    let spec = mk_spec_full(
        &id,
        fixture_command("exit", &[&reported.port().to_string(), "1"]),
        None,
        BTreeMap::new(),
        goetia::spec::Restart::OnFailure,
        None,
        None,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");

    reported.accept("the first spawn to report in before exiting 1");
    let before_respawn = std::time::Instant::now();
    reported.accept("a respawned child to report in after the first one exited nonzero");
    let elapsed = before_respawn.elapsed();

    mgr.stop(&spec.id).expect("stop");

    // A real, if generous (scheduling jitter, not a chosen synchronization
    // bound), lower bound on `DEFAULT_RESTART_DELAY` (1s): proves the
    // supervisor actually paced the respawn rather than looping
    // immediately, without pinning it to an exact duration.
    assert!(
        elapsed >= std::time::Duration::from_millis(700),
        "respawn happened after only {elapsed:?}, faster than the documented ~1s restart-delay default allows for"
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

    // Filtered server-side by provider, not by an arbitrary "most recent N"
    // window: an unrelated process's own Application-log entries (of which
    // there can be arbitrarily many between the shim's `ReportEventW` call
    // and this query) can never push a `Goetia`-provider entry out of a
    // fixed-size recent slice, because nothing else writes under that
    // provider name. The `/c:` cap is still present as a sanity bound, not
    // as the filter itself.
    let events = support::cmd::run(
        "wevtutil.exe",
        &[
            "qe",
            "Application",
            "/q:*[System[Provider[@Name='Goetia']]]",
            "/rd:true",
            "/c:1000",
            "/f:text",
        ],
    );
    assert!(
        events.ok() && events.stdout.contains(&id),
        "Windows Event Log (Application, provider `Goetia`) has no entry naming `{id}`:\n{events}"
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
