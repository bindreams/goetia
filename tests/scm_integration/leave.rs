//! `uninstall --no-stop` of a task on the real Task Scheduler: a run that uninstalls its own id through the `goetia`
//! binary is not ended by it, and goes on until the test releases it through its connection. While it is held the
//! tests assert what the system reports, never that time has passed; the run's end is waited for through a handle
//! on its process.

use std::io::Read as _;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{Budget, RunInFlight, ServiceManager as _};
use goetia::spec::{DaemonSpec, Id, Protocol, User};

use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::UNINSTALL_SELF_THEN_RELEASE;
use crate::support::windows_tasks::{ProcessHandle, task_spec};
use crate::support::{self, ConnectBack, Group, scm_group, wait};
use crate::tasks::{TaskGuard, records_of, service_as, task_exists};

/// The task `id`, whose program is this binary told to uninstall `id` with `--no-stop` and then go on until
/// released, writing `after`.
fn self_uninstalling(id: &Id, listener: &ConnectBack, after: &std::path::Path) -> DaemonSpec {
    task_spec(
        id,
        vec![
            support::current_exe_str(),
            UNINSTALL_SELF_THEN_RELEASE.to_owned(),
            env!("CARGO_BIN_EXE_goetia").to_owned(),
            id.to_string(),
            listener.port().to_string(),
            after.display().to_string(),
        ],
        None,
        User::Root,
        None,
    )
}

/// The run that removed its own task and waits for the test: what it reported of its uninstall, and the two ends
/// of its life. Dropping it releases the run (the test's end of the connection closes).
struct LeftRun {
    uninstall_code: String,
    uninstall_output: String,
    process: ProcessHandle,
    stream: TcpStream,
}

impl LeftRun {
    /// Blocks until the run has uninstalled its own task and reported: the event the run causes, with no bound.
    fn accept(listener: &ConnectBack) -> Self {
        let mut stream = listener.accept_unbounded();
        let mut report = String::new();
        stream.read_to_string(&mut report).expect("the run's report");
        let mut parts = report.splitn(3, '\n');
        let pid = parts.next().and_then(|pid| pid.parse().ok());
        let (Some(pid), Some(code), Some(output)) = (pid, parts.next(), parts.next()) else {
            panic!("the run reported {report:?}, not `<pid>\\n<code>\\n<output>`");
        };
        Self {
            uninstall_code: code.to_owned(),
            uninstall_output: output.to_owned(),
            process: ProcessHandle::open(pid),
            stream,
        }
    }

    /// Lets the run go on, and returns once its process has exited by itself.
    fn release_and_wait_for_its_exit(self) {
        drop(self.stream);
        self.process.wait_for_exit();
    }
}

/// Where the removal's marker of `id` stands, beside the run records.
fn marker_of(id: &Id) -> PathBuf {
    records_of(id).with_file_name(format!("{id}~removing"))
}

/// The run removes its own task and goes on: the task is not registered, its process is alive and has written
/// nothing after the uninstall, and its records and the marker stay. An `install` of the id is refused while the run
/// holds the records. Released, the run writes its file and exits by itself, and then the id is not installed and the
/// next `install` of it succeeds and leaves no marker of the old removal.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_run_that_uninstalls_its_own_task_with_no_stop_goes_on_until_it_exits(#[fixture(scm_group)] _group: &Group) {
    // Declared first, so they go last: the task and its records are removed before the directory the run writes in.
    let dir = tempfile::tempdir().expect("tempdir");
    let guard = TaskGuard::new();
    let id = &guard.id;
    let _marker = support::cleanup::FileGuard::new(marker_of(id));
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    let after = dir.path().join("after");
    let spec = self_uninstalling(id, &listener, &after);
    mgr.install(&spec, false).expect("install");
    mgr.start(id, Budget::DEFAULT).expect("start");

    let run = LeftRun::accept(&listener);

    assert_eq!(
        run.uninstall_code, "0",
        "the run's own uninstall failed: {}",
        run.uninstall_output
    );
    let gone = mgr.status(id).expect_err("the task is not installed");
    assert!(matches!(gone.cause(), Error::NotInstalled { .. }), "{gone:?}");
    assert!(!task_exists(id), "Task Scheduler has no task of the id");
    assert!(!run.process.has_exited(), "the run was not ended");
    assert!(
        !after.exists(),
        "the run is held, so it has written nothing after its uninstall"
    );
    assert!(records_of(id).is_dir(), "the run's records stay");
    assert!(marker_of(id).exists(), "and so does the removal's marker");

    let refused = mgr.install(&spec, false).expect_err("the run holds the records");
    assert!(
        matches!(refused.cause(), Error::RunRecordsHeld { .. }),
        "{}",
        refused.report()
    );
    assert!(marker_of(id).exists(), "the refused install changed nothing");
    assert!(!task_exists(id), "and registered nothing");
    // A service at the id is the same install over the same records.
    let refused = mgr
        .install(&service_as(id, Protocol::Native), false)
        .expect_err("the run holds the records");
    assert!(
        matches!(refused.cause(), Error::RunRecordsHeld { .. }),
        "{}",
        refused.report()
    );
    let absent = mgr.status(id).expect_err("no service was registered");
    assert!(matches!(absent.cause(), Error::NotInstalled { .. }), "{absent:?}");

    run.release_and_wait_for_its_exit();

    assert!(
        after.exists(),
        "the run went on after its uninstall returned, and wrote its file"
    );
    let gone = mgr.status(id).expect_err("the task is still not installed");
    assert!(matches!(gone.cause(), Error::NotInstalled { .. }), "{gone:?}");
    // The run's shim holds its pin until it exits, a moment after the program: a refusal changes nothing, and each
    // attempt re-reads whether the records are still held.
    let mut created = None;
    wait::until_paced(Duration::from_millis(50), || match mgr.install(&spec, false) {
        Ok(outcome) => {
            created = Some(outcome);
            true
        }
        Err(e) if matches!(e.cause(), Error::RunRecordsHeld { .. }) => false,
        Err(e) => panic!("the next install: {}", e.report()),
    });
    assert!(matches!(created, Some(Outcome::Create)), "{created:?}");
    assert!(!marker_of(id).exists(), "no marker of the old removal is left");
    assert!(task_exists(id), "the task is installed again");
}

/// `uninstall --no-stop` of the id the run left does not wait for it: it returns at once with the records and the
/// marker as they were. Once the run has ended, an `uninstall` of the id removes them.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_no_stop_uninstall_of_the_id_a_run_left_does_not_wait_for_it(#[fixture(scm_group)] _group: &Group) {
    let dir = tempfile::tempdir().expect("tempdir");
    let guard = TaskGuard::new();
    let id = &guard.id;
    let _marker = support::cleanup::FileGuard::new(marker_of(id));
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    let spec = self_uninstalling(id, &listener, &dir.path().join("after"));
    mgr.install(&spec, false).expect("install");
    mgr.start(id, Budget::DEFAULT).expect("start");
    let run = LeftRun::accept(&listener);
    assert_eq!(run.uninstall_code, "0", "{}", run.uninstall_output);

    let absent = mgr
        .uninstall(id, RunInFlight::Leave)
        .expect_err("nothing is installed at the id");

    assert!(matches!(absent.cause(), Error::NotInstalled { .. }), "{absent:?}");
    assert!(!run.process.has_exited(), "the run was not ended");
    assert!(
        records_of(id).is_dir() && marker_of(id).exists(),
        "the held records and the marker stay"
    );

    run.release_and_wait_for_its_exit();
    // Waits for the shim's pin, which the run's exit releases.
    let absent = mgr
        .uninstall(id, RunInFlight::Stop)
        .expect_err("nothing is installed at the id");

    assert!(matches!(absent.cause(), Error::NotInstalled { .. }), "{absent:?}");
    assert!(!records_of(id).exists(), "the records are removed");
    assert!(!marker_of(id).exists(), "and the marker");
}
