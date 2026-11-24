//! `uninstall --no-stop` on a real launchd: a run that uninstalls its own id is not ended by it, and exits by
//! itself when the test releases it. While it is held the tests assert what `launchctl print` reports,
//! never that some time has passed; a run is released by a FIFO, and its end is the pid launchd stops
//! showing.

use std::fs;
use std::time::Duration;

use goetia::Error;
use goetia::backend::launchd::manager::Launchd;
use goetia::decide::Outcome;
use goetia::manager::{Budget, RunInFlight, ServiceManager};
use goetia::spec::{DaemonSpec, Protocol, Restart, ServiceFields, ServiceSpec};
use goetia::testing::task;

use crate::launchd::{
    Guard, enabled_path, is_loaded, print_field, print_job, staging_path, timer_enabled_path, timer_staging_path,
};
use crate::support::tasks::{SelfUninstalling, id_of};
use crate::support::{self, Group, launchd_group, wait};
use crate::tasks::pid_of;

/// A daily schedule that does not come due while a test runs.
const DAILY: &str = "0 3 * * *";

const GOETIA: &str = env!("CARGO_BIN_EXE_goetia");

fn assert_not_installed(mgr: &Launchd, id: &str) {
    match mgr.status(&id_of(id)) {
        Err(Error::NotInstalled { .. }) => {}
        other => panic!("`{id}` should read as not installed, got {other:?}"),
    }
    let files = [
        staging_path(id),
        enabled_path(id),
        timer_staging_path(id),
        timer_enabled_path(id),
    ];
    assert!(files.iter().all(|path| !path.exists()), "{files:?}");
}

/// The run removes its own task and goes on: launchd still has the job with the run's own pid, the timer is
/// gone, and no plist is left. When the run exits by itself the label stays, idle, until the next `install`
/// of the id boots it out.
#[skuld::test]
fn a_run_that_uninstalls_its_own_task_with_no_stop_goes_on_until_it_exits(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = Guard(id_of(&id));
    let mut run = SelfUninstalling::new(GOETIA, &id);
    let spec = task(&id, run.held.command.clone(), Some(DAILY));
    let mgr = Launchd::new();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&id_of(&id)).expect("enable");
    mgr.start(&id_of(&id), Budget::DEFAULT).expect("start");

    run.held.wait_started();

    let pid: u32 = run.pid().parse().expect("the run wrote its pid");
    let printed = print_job(&id).stdout;
    assert_eq!(
        pid_of(&id),
        Some(pid),
        "the label is loaded, with the run's own process:\n{printed}"
    );
    assert_eq!(print_field(&printed, "state = "), Some("running"), "{printed}");
    assert!(!is_loaded(&format!("{id}@timer")), "nothing will start another run");
    assert_not_installed(&mgr, &id);

    run.held.release();
    wait::until_paced(Duration::from_millis(50), || pid_of(&id).is_none());

    let printed = print_job(&id).stdout;
    assert_eq!(
        print_field(&printed, "last exit code = "),
        Some("0"),
        "the run exited by itself:\n{printed}"
    );
    assert_eq!(run.held.begun(), 1, "one run, and no second");
    assert_not_installed(&mgr, &id);
    assert!(is_loaded(&id), "the idle label stays until the next install");
    assert_eq!(
        mgr.install(&spec, false).expect("the next install succeeds"),
        Outcome::Create
    );
    assert!(!is_loaded(&id), "and boots the label out");
}

/// A service launchd keeps alive refuses `--no-stop` before anything is changed: it goes on running with the
/// same process, and its plist is as it was.
#[skuld::test]
fn no_stop_is_refused_for_a_service_launchd_keeps_alive_and_it_keeps_running(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = Guard(id_of(&id));
    let spec = ServiceSpec::new(ServiceFields {
        restart: Restart::Always,
        protocol: Protocol::Plain,
        ..support::base_fields(&id, ["/bin/sleep", "300"].map(String::from).to_vec())
    })
    .map(DaemonSpec::from)
    .expect("a service");
    let mgr = Launchd::new();
    mgr.install(&spec, false).expect("install");
    mgr.start(&id_of(&id), Budget::DEFAULT).expect("start");
    let pid = pid_of(&id).expect("the service runs");
    let plist = fs::read_to_string(staging_path(&id)).expect("the plist is installed");

    match mgr.uninstall(&id_of(&id), RunInFlight::Leave) {
        Err(Error::KeptAlive { .. }) => {}
        other => panic!("`{id}` is kept alive by launchd, got {other:?}"),
    }

    assert_eq!(pid_of(&id), Some(pid), "the service keeps running");
    assert_eq!(
        fs::read_to_string(staging_path(&id)).expect("the plist is still there"),
        plist
    );
    mgr.status(&id_of(&id)).expect("the id is still installed");
}
