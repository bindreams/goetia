//! `uninstall --no-stop` on a real systemd: a run that uninstalls its own id is not ended by it, and
//! exits by itself when the test releases it. While it is held the tests assert what systemd reports,
//! never that some time has passed; a run is released by a FIFO and waited for by its pid.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use goetia::Error;
use goetia::backend::systemd::manager::Systemd;
use goetia::decide::Outcome;
use goetia::manager::{Budget, RunInFlight, ServiceManager};
use goetia::spec::{DaemonSpec, ServiceSpec};

use crate::linux::{show_properties, unit_of};
use crate::support::tasks::{Held, id_of, sh};
use crate::support::{self, Group, SYSTEMD_RUNTIME_UNIT_DIR, SYSTEMD_UNIT_DIR, ServiceGuard, cmd, systemd_group, wait};

/// A daily schedule that does not come due while a test runs.
const DAILY: &str = "0 3 * * *";

/// A run that writes its pid and then uninstalls its own `id` with `--no-stop`, through the `goetia`
/// binary, before it announces itself and waits to be released.
struct SelfUninstalling {
    held: Held,
    pid: PathBuf,
    _dir: tempfile::TempDir,
}

impl SelfUninstalling {
    fn new(id: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid = dir.path().join("pid");
        let held = Held::after(&format!(
            "echo $$ > \"{}\"; \"{}\" daemon uninstall --no-stop {id} 2>&1; ",
            pid.display(),
            env!("CARGO_BIN_EXE_goetia"),
        ));
        SelfUninstalling { held, pid, _dir: dir }
    }

    /// The run's pid, once it has announced itself.
    fn pid(&self) -> String {
        fs::read_to_string(&self.pid)
            .expect("the run wrote its pid before it announced itself")
            .trim()
            .to_string()
    }
}

fn service(id: &str, command: Vec<String>) -> DaemonSpec {
    ServiceSpec::new(goetia::testing::base_fields(id, command))
        .map(DaemonSpec::from)
        .expect("a service")
}

fn standin(id: &str) -> PathBuf {
    Path::new(SYSTEMD_RUNTIME_UNIT_DIR).join(unit_of(id))
}

fn unit_files(id: &str) -> [PathBuf; 2] {
    [unit_of(id), format!("{id}.timer")].map(|name| Path::new(SYSTEMD_UNIT_DIR).join(name))
}

/// Blocks until `pid` is gone, and then until the unit has settled: the process ending is the event, and
/// the unit's own state follows it.
fn wait_for_exit(id: &str, pid: &str) {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    wait::until_paced(Duration::from_millis(50), || !proc.exists());
    wait::until_paced(Duration::from_millis(50), || {
        let [state] = show_properties(&unit_of(id), ["ActiveState"]);
        !matches!(state.as_str(), "active" | "activating" | "deactivating")
    });
}

fn assert_not_installed(mgr: &Systemd, id: &str) {
    match mgr.status(&id_of(id)) {
        Err(Error::NotInstalled { .. }) => {}
        other => panic!("`{id}` should read as not installed, got {other:?}"),
    }
    assert!(unit_files(id).iter().all(|path| !path.exists()), "{:?}", unit_files(id));
}

// A task ==============================================================================================================

/// The run removes its own task and goes on: systemd still has it in its start job with its own pid and
/// no start timeout, the id is not installed, and the stand-in is the only file left. When the run exits
/// by itself the stand-in stays, a hand start of it fails, and the next `install` of the id removes it.
#[skuld::test]
fn a_run_that_uninstalls_its_own_task_with_no_stop_goes_on_until_it_exits(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let mut run = SelfUninstalling::new(&id);
    let spec = goetia::testing::task(&id, run.held.command.clone(), Some(DAILY));
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&id_of(&id)).expect("enable");
    mgr.start(&id_of(&id), Budget::DEFAULT).expect("start");

    run.held.wait_started();

    let pid = run.pid();
    let [state, main_pid, timeout, fragment] = show_properties(
        &unit_of(&id),
        ["ActiveState", "MainPID", "TimeoutStartUSec", "FragmentPath"],
    );
    assert_eq!(state, "activating", "the run is still in its start job");
    assert_eq!(main_pid, pid, "the same process");
    assert_eq!(
        timeout, "infinity",
        "the reload did not give the unit systemd's start timeout back"
    );
    assert_eq!(fragment, standin(&id).display().to_string());
    let [timer] = show_properties(&format!("{id}.timer"), ["ActiveState"]);
    assert_eq!(timer, "inactive", "nothing will start another run");
    assert_not_installed(&mgr, &id);
    let text = fs::read_to_string(standin(&id)).expect("the stand-in is there");
    assert!(text.contains(&format!("Id={id}\n")), "{text}");

    run.held.release();
    wait_for_exit(&id, &pid);

    assert!(standin(&id).exists(), "nothing removes the stand-in by itself");
    assert_not_installed(&mgr, &id);
    let hand = cmd::run("systemctl", &["start", &unit_of(&id)]);
    assert!(!hand.ok(), "a hand start of the stand-in fails: {hand}");
    assert_eq!(
        mgr.install(&spec, false).expect("the next install succeeds"),
        Outcome::Create
    );
    assert!(!standin(&id).exists(), "and removes the stand-in");
    assert!(unit_files(&id).iter().all(|path| path.exists()));
}

/// With no run in flight `--no-stop` is a plain uninstall: nothing is left, the stand-in included.
#[skuld::test]
fn no_stop_on_a_task_that_is_not_running_leaves_nothing(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = goetia::testing::task(&id, sh("exit 0"), Some(DAILY));
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&id_of(&id)).expect("enable");

    mgr.uninstall(&id_of(&id), RunInFlight::Leave).expect("uninstall");

    assert_not_installed(&mgr, &id);
    assert!(!standin(&id).exists(), "no run, no stand-in");
}

// A service ===========================================================================================================

/// The run removes its own service and goes on: it keeps its main pid, the unit is loaded with no file,
/// and when the run exits by itself it is not restarted.
#[skuld::test]
fn a_service_that_uninstalls_itself_with_no_stop_keeps_running_until_it_exits(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let mut run = SelfUninstalling::new(&id);
    let spec = service(&id, run.held.command.clone());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.start(&id_of(&id), Budget::DEFAULT).expect("start");

    run.held.wait_started();

    let pid = run.pid();
    let [state, main_pid, load, fragment] =
        show_properties(&unit_of(&id), ["ActiveState", "MainPID", "LoadState", "FragmentPath"]);
    assert_eq!(state, "active");
    assert_eq!(main_pid, pid, "the service keeps its main pid");
    assert_eq!(load, "not-found", "the unit is loaded with no file");
    assert_eq!(fragment, "");
    assert_not_installed(&mgr, &id);
    assert!(
        !standin(&id).exists(),
        "a service's run is past its start phase and needs no stand-in"
    );

    run.held.release();
    wait_for_exit(&id, &pid);

    let [state, restarts, result] = show_properties(&unit_of(&id), ["ActiveState", "NRestarts", "Result"]);
    assert_eq!(state, "inactive", "it exited and was not restarted");
    assert_eq!(restarts, "0");
    assert_eq!(result, "success");
    assert_not_installed(&mgr, &id);
}
