//! The accounts a task runs as: a local user (a password logon, with the batch logon right goetia grants
//! it), and the virtual service account no task can run as.

use std::process::{Command, Output};

use goetia::error::Error;
use goetia::manager::{Budget, Exit, ServiceManager as _, State};
use goetia::spec::{DaemonFields, DaemonSpec, Id, Protocol, ServiceFields, ServiceSpec, User};

use crate::install_helper::TASK_AS;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::{CONNECT_BACK, PRINT_AND_EXIT, RUN_UNTIL_KILLED};
use crate::support::windows_tasks::{ServiceAccountHost, task_spec};
use crate::support::{self, ConnectBack, Group, LocalUserGuard, ServiceGuard, cleanup, kill, scm_group};
use crate::tasks::{SCHEDULE, TaskGuard, records_of, task_as, task_exists, task_status, test_id};

/// The password of the accounts these tests make: at most 14 characters, since a longer one makes
/// `net user ... /add` ask about pre-Windows-2000 compatibility and wait on stdin.
const PASSWORD: &str = "Goetia!Test12";

/// `verb` of a task made by the install helper, a child process that has the password in its environment
/// and no other process does (see `install_helper`).
fn as_user(verb: &str, id: &Id, account: &str, host: &ServiceAccountHost, command: &[String]) -> Output {
    Command::new(support::current_exe_str())
        .args([TASK_AS, verb, id.as_str(), account, &host.shim_path()])
        .args(command)
        .env("GOETIA_SERVICE_PASSWORD", PASSWORD)
        .output()
        .expect("spawn the task helper")
}

#[track_caller]
fn assert_succeeded(what: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{what} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A task of a local user installs, is armed and disarmed, runs and is removed. `install`, `enable` and
/// `disable` read the password from the environment of the process that makes them (a logon is
/// registered with it, and Task Scheduler takes none back, M3), so a child makes them; `uninstall`
/// never needs it, and runs here, where no variable holds one.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_real_user_task_installs_arms_and_runs(#[fixture(scm_group)] _group: &Group) {
    assert!(
        std::env::var_os("GOETIA_SERVICE_PASSWORD").is_none(),
        "precondition: this process holds no password, so its `uninstall` shows it needs none"
    );
    // Windows local account names are capped at 20 characters.
    let account = format!("gt{:08x}", rand::random::<u32>());
    // Declared first, so the account goes after the task, whose logon it is.
    let _user = LocalUserGuard::create(&account, PASSWORD);
    // The copies of this binary and the shim before the guard, so they go after the task is removed, and the
    // shim copy is not deleted under a shim that still runs.
    let id = &test_id();
    let host = ServiceAccountHost::new(id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let listener = ConnectBack::listen();
    let command = vec![
        host.exe.path_str(),
        CONNECT_BACK.to_owned(),
        listener.port().to_string(),
    ];

    assert_succeeded("install", &as_user("install", id, &account, &host, &command));
    let installed = task_status(&mgr, id);
    assert_eq!(installed.common.state, State::Idle);
    assert!(!installed.common.enabled, "a task is installed disarmed");

    // M3 and Decision 22: a task whose logon is a password is rewritten only with it, so without it
    // `enable` is refused before anything is written.
    let refused = mgr.enable(id).expect_err("enable without the password");
    assert!(matches!(refused.cause(), Error::PasswordRequired { .. }), "{refused:?}");
    assert!(
        !task_status(&mgr, id).common.enabled,
        "the refused enable wrote nothing"
    );

    assert_succeeded("enable", &as_user("enable", id, &account, &host, &command));
    let armed = task_status(&mgr, id);
    assert!(armed.common.enabled && armed.next_run.is_some(), "{armed:?}");
    assert_succeeded("disable", &as_user("disable", id, &account, &host, &command));
    assert!(!task_status(&mgr, id).common.enabled);

    // A first logon under a new account builds its profile, which is slow: `run` waits on the run itself.
    let exit = mgr
        .run(id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run as {account}: {}", e.report()));
    assert_eq!(
        exit,
        Exit::Code(0),
        "a recorded run, so the shim could write its records as the user"
    );
    listener.accept("the user's program to have run");

    mgr.uninstall(id).expect("uninstall without the password");
    assert!(!task_exists(id));
    assert!(!records_of(id).exists());
}

/// `NT SERVICE\<name>` is a virtual account, and Task Scheduler would run a task registered with one as
/// LOCAL SERVICE: the install is refused naming that, before anything is written.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_virtual_account_is_refused_at_install_and_writes_nothing(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_as(id, User::Name(format!(r"NT SERVICE\{id}")), Some(SCHEDULE));

    let refused = mgr.install(&spec, false).expect_err("a virtual account runs no task");

    assert!(matches!(refused.cause(), Error::Invalid { .. }), "{refused:?}");
    assert!(
        refused.to_string().contains("is a virtual service account"),
        "the refusal names the limit: {refused}"
    );
    assert!(!task_exists(id), "no task was registered");
    assert!(!records_of(id).exists(), "no records were made");
    let absent = mgr.status(id).expect_err("nothing is installed");
    assert!(matches!(absent.cause(), Error::NotInstalled { .. }), "{absent:?}");
}

/// The names of the run records of `id`, in order.
fn record_files(id: &Id) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(records_of(id))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| name.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// What a service of `account` (no `logs:`) leaves of goetia's folders, then a task install: with no
/// `%ProgramData%\Goetia` to start from, a service run as a non-administrator account is installed, runs
/// and logs, and a task install after it must not be refused for the folders that service made. Only an
/// elevated install makes `Goetia` and `Goetia\logs`, so they are goetia's, and the account can write the
/// log in the one it is given.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_service_of_a_standard_account_leaves_no_folder_that_refuses_a_task_install(#[fixture(scm_group)] _group: &Group) {
    // Declared first, so it goes last: the folders that every account of this test made.
    let goetia = support::program_data().join("Goetia");
    let _folders = cleanup::DirGuard::new(goetia.clone());
    for account in ["LocalService", "NetworkService"] {
        cleanup::remove_or_warn(&goetia, true);
        assert!(!goetia.exists(), "precondition: no {} to start from", goetia.display());

        let service = test_id();
        let host = ServiceAccountHost::new(&service);
        let _service_guard = ServiceGuard::new(service.as_str());
        let mgr = host.manager();
        let started = ConnectBack::listen();
        let spec = ServiceSpec::new({
            let base = support::base_fields(
                service.as_str(),
                vec![
                    host.exe.path_str(),
                    RUN_UNTIL_KILLED.to_owned(),
                    started.port().to_string(),
                ],
            );
            ServiceFields {
                common: DaemonFields {
                    user: User::Name(account.to_owned()),
                    ..base.common
                },
                protocol: Protocol::Plain,
                ..base
            }
        })
        .map(DaemonSpec::from)
        .expect("a valid service");
        mgr.install(&spec, false)
            .unwrap_or_else(|e| panic!("install the service as {account}: {}", e.report()));
        mgr.start(&service, Budget::Unbounded)
            .unwrap_or_else(|e| panic!("start the service as {account}: {}", e.report()));
        started.accept("the program of a service run by a standard account");
        let log = support::shim_default_log(&support::program_data(), service.as_str());
        assert!(log.is_file(), "the shim of {account} opened {}", log.display());
        mgr.stop(&service, Budget::DEFAULT)
            .unwrap_or_else(|e| panic!("stop the service as {account}: {}", e.report()));

        let task = TaskGuard::new();
        mgr.install(&task_as(&task.id, User::Root, None), false)
            .unwrap_or_else(|e| panic!("install a task after a service of {account}: {}", e.report()));
    }
}

/// A task installed as another account over the records of the first, without being removed first: the
/// task is registered again as the new account, whose shim reads it, writes its record and prunes the
/// ones the first account's run and its own left (a record has an entry for the account that wrote it
/// only, so the new account deletes them through the record directory's own delete-child right). The
/// accounts are two that need no password, LocalService and NetworkService. The prune is waited for with
/// no cap, since it is a state of the directory: a prune that fails hangs the test.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_task_installed_as_another_account_over_the_first_ones_records_still_records_and_prunes(
    #[fixture(scm_group)] _group: &Group,
) {
    // The copies of this binary and the shim before the guard, so they go after the task is removed.
    let id = &test_id();
    let host = ServiceAccountHost::new(id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let command = vec![
        host.exe.path_str(),
        PRINT_AND_EXIT.to_owned(),
        "0".to_owned(),
        "run".to_owned(),
    ];
    let as_account = |name: &str| task_spec(id, command.clone(), None, User::Name(name.to_owned()), None);

    mgr.install(&as_account("LocalService"), false)
        .unwrap_or_else(|e| panic!("install as LocalService: {}", e.report()));
    let exit = mgr
        .run(id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run as LocalService: {}", e.report()));
    assert_eq!(exit, Exit::Code(0), "a recorded run of the first account");
    let first_records = record_files(id);
    assert_eq!(first_records.len(), 1, "{first_records:?}");

    // Installed over the task, which is not removed: the account changes while the task and its records
    // stay. The run is a `run`, whose pin keeps the prune off, so a task its new shim cannot read ends
    // unrecorded here, as an error, and not as a wait that has no end.
    mgr.install(&as_account("NetworkService"), false)
        .unwrap_or_else(|e| panic!("install over it as NetworkService: {}", e.report()));
    let exit = mgr
        .run(id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run as NetworkService: {}", e.report()));
    assert_eq!(exit, Exit::Code(0), "a recorded run of the second account");
    let second_records = record_files(id);
    assert!(
        second_records.len() == 2 && second_records.contains(&first_records[0]),
        "the pin kept the first record: {second_records:?}"
    );

    // `start`, not `run`: a write of a run nobody waits for prunes the records of both runs before it.
    mgr.start(id, Budget::DEFAULT)
        .unwrap_or_else(|e| panic!("start as NetworkService: {}", e.report()));
    kill::until(|| {
        let now = record_files(id);
        now.len() == 1 && !second_records.contains(&now[0])
    });
}
