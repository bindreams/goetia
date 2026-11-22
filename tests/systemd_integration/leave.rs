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
use goetia::spec::{DaemonSpec, Restart, ServiceFields, ServiceSpec};

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
    /// A run that exits 0 when released.
    fn new(id: &str) -> Self {
        Self::exiting(id, 0)
    }

    /// A run that exits with `code` when released.
    fn exiting(id: &str, code: u8) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid = dir.path().join("pid");
        let held = Held::after_exiting(
            &format!(
                "echo $$ > \"{}\"; \"{}\" daemon uninstall --no-stop {id} 2>&1; ",
                pid.display(),
                env!("CARGO_BIN_EXE_goetia"),
            ),
            code,
        );
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

/// A service that systemd restarts whenever its process ends, after `delay`.
fn restarting_service(id: &str, command: Vec<String>, delay: Option<Duration>) -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        restart: Restart::Always,
        restart_delay: delay,
        ..goetia::testing::base_fields(id, command)
    })
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
/// and when the run exits by itself with a failure it is not restarted, although the removed unit said
/// `Restart=always`.
#[skuld::test]
fn a_service_that_uninstalls_itself_with_no_stop_keeps_running_and_is_not_restarted_when_it_fails(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let mut run = SelfUninstalling::exiting(&id, 3);
    let spec = restarting_service(&id, run.held.command.clone(), None);
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

    // The process is gone, and the unit has not gone to `activating` for a restart: it is failed, as the
    // process was, and one start is all there was.
    let [state, restarts, result] = show_properties(&unit_of(&id), ["ActiveState", "NRestarts", "Result"]);
    assert_eq!(state, "failed", "it exited and was not restarted");
    assert_eq!(restarts, "0");
    assert_eq!(result, "exit-code");
    assert_eq!(run.held.begun(), 1, "one run, not a second");
    assert_not_installed(&mgr, &id);
}

/// A service that crashes at once and is restarted after goetia's default delay is, at any moment, a process
/// or a restart waiting; `--no-stop` over either sends nothing to any process. A restart that was waiting
/// is cleared (the reload fails it on the missing unit, and goetia clears what failed); a process is left
/// to exit, and is not restarted when it does. So whichever it was, the unit ends `inactive` and unlisted, or
/// `failed` with no restart behind it, and no process was ever signalled.
#[skuld::test]
fn no_stop_on_a_service_that_restarts_signals_nothing_and_is_not_restarted_again(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let (starts, signals) = (dir.path().join("starts"), dir.path().join("signals"));
    let command = sh(&format!(
        "echo $$ >> \"{}\"; trap 'echo signal >> \"{}\"' TERM INT HUP; sleep 0.2 & wait $!; exit 3",
        starts.display(),
        signals.display()
    ));
    let spec = restarting_service(&id, command, None);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.start(&id_of(&id), Budget::Immediate).expect("start");
    let began = || fs::read_to_string(&starts).map_or(0, |text| text.lines().count());
    wait::until_paced(Duration::from_millis(20), || began() >= 2);

    mgr.uninstall(&id_of(&id), RunInFlight::Leave).expect("uninstall");

    assert_not_installed(&mgr, &id);
    let [restarts_at_return] = show_properties(&unit_of(&id), ["NRestarts"]);
    wait::until_paced(Duration::from_millis(20), || {
        let [state] = show_properties(&unit_of(&id), ["ActiveState"]);
        matches!(state.as_str(), "inactive" | "failed")
    });
    let [state, load, main_pid, restarts] = show_properties(
        &unit_of(&id),
        ["ActiveState", "LoadState", "MainPID", "NRestarts"],
    );
    assert_eq!(restarts, restarts_at_return, "nothing restarted it after the uninstall returned");
    assert_eq!(main_pid, "0", "no process is left");
    let listed = cmd::run("systemctl", &["list-units", "--all", "--plain", "--no-legend", &unit_of(&id)]);
    // A process caught between its fork and its exec is still in its start job, which gets a stand-in.
    let loaded_from = if standin(&id).exists() { "loaded" } else { "not-found" };
    match state.as_str() {
        "inactive" => assert!(
            !listed.stdout.contains(&id),
            "a restart that was waiting is cleared, and nothing is left listed: {listed}"
        ),
        _ => assert_eq!(
            load, loaded_from,
            "a process that was left exited and was not restarted: {listed}"
        ),
    }
    assert!(
        fs::read_to_string(&signals).unwrap_or_default().is_empty(),
        "no process was sent a signal"
    );
}

// A definition whose file is gone =====================================================================================

/// An earlier `--no-stop` uninstall removed the unit file and its reload failed, so systemd still holds the
/// definition and would restart the service from memory when it exits: the retry finds nothing installed and
/// reloads, and the process, still running, is left alone.
#[skuld::test]
fn a_retry_after_a_removal_with_no_reload_reloads_and_leaves_the_run(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let mut held = Held::new();
    let spec = restarting_service(&id, held.command.clone(), None);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.start(&id_of(&id), Budget::DEFAULT).expect("start");
    held.wait_started();
    let [pid, load] = show_properties(&unit_of(&id), ["MainPID", "LoadState"]);
    assert_eq!(load, "loaded");
    fs::remove_file(&unit_files(&id)[0]).expect("remove the unit file by hand, with no reload");
    let [load] = show_properties(&unit_of(&id), ["LoadState"]);
    assert_eq!(load, "loaded", "systemd still holds the definition");

    match mgr.uninstall(&id_of(&id), RunInFlight::Leave) {
        Err(Error::NotInstalled { .. }) => {}
        other => panic!("nothing is installed at `{id}`, got {other:?}"),
    }

    let [load, state, main_pid] = show_properties(&unit_of(&id), ["LoadState", "ActiveState", "MainPID"]);
    assert_eq!(load, "not-found", "systemd no longer holds the definition");
    assert_eq!(state, "active");
    assert_eq!(main_pid, pid, "and the process still runs");

    held.release();
    wait_for_exit(&id, &pid);
}

// What a stop of the left run uses ====================================================================================

/// A drop-in sets the task's `KillSignal=` and `TimeoutStopSec=`, and the uninstall deletes it with the job:
/// the stand-in carries the values systemd was applying, so a hand stop of the left run signals and waits as
/// before.
#[skuld::test]
fn the_standin_carries_the_stop_settings_a_drop_in_gave_the_job(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let mut run = SelfUninstalling::new(&id);
    let spec = goetia::testing::task(&id, run.held.command.clone(), None);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    let dropin = Path::new(SYSTEMD_UNIT_DIR).join(format!("{}.d", unit_of(&id)));
    fs::create_dir_all(&dropin).expect("create the drop-in directory");
    fs::write(
        dropin.join("override.conf"),
        "[Service]\nKillSignal=SIGUSR2\nTimeoutStopSec=45s\n",
    )
    .expect("write the drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
    mgr.start(&id_of(&id), Budget::DEFAULT).expect("start");

    run.held.wait_started();

    let [signal, timeout, fragment] = show_properties(&unit_of(&id), ["KillSignal", "TimeoutStopUSec", "FragmentPath"]);
    assert_eq!(fragment, standin(&id).display().to_string());
    assert_eq!(signal, "12", "SIGUSR2");
    assert_eq!(timeout, "45s");
    assert!(!dropin.exists(), "the drop-in went with the job");

    run.held.release();
    wait_for_exit(&id, &run.pid());
}
