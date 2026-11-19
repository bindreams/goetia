//! The accounts a task runs as: a local user (a password logon, with the batch logon right goetia grants
//! it), and the virtual service account no task can run as.

use std::process::{Command, Output};

use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{Budget, Exit, ServiceManager as _, State};
use goetia::spec::{DaemonFields, DaemonSpec, Id, Protocol, ServiceFields, ServiceSpec, User};
use goetia::testing::sid_of;

use crate::fixture::FIXTURE;
use crate::install_helper::TASK_AS;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::{CONNECT_BACK, PRINT_AND_EXIT, PRINT_WHEN_RELEASED, RUN_UNTIL_KILLED};
use crate::support::windows_tasks::{ServiceAccountHost, manager_hosted_by, task_spec, this_binary_as_a_service_host};
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

    // A first logon under a new account builds its profile, which is slow: `run` waits on the run itself,
    // with no cap. On CI that wait ends because the workflow removes the Sync Center profile handler before
    // the tests: with it, about 1 first logon in 10 blocks for good in Windows and `run` never returns (knowledge
    // base: "Windows - A password-logon task's first start under a new local user can block for good in the
    // User Profile Service's call to the Sync Center notification handler").
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
        let log = support::shim_default_log(&support::program_data(), service.as_str());
        assert!(
            log.is_file(),
            "the install made {} for the account to append to; the shim makes none",
            log.display()
        );
        mgr.start(&service, Budget::Unbounded)
            .unwrap_or_else(|e| panic!("start the service as {account}: {}", e.report()));
        started.accept("the program of a service run by a standard account");
        assert!(log.is_file(), "the shim of {account} opened {}", log.display());
        mgr.stop(&service, Budget::DEFAULT)
            .unwrap_or_else(|e| panic!("stop the service as {account}: {}", e.report()));
        mgr.uninstall(&service)
            .unwrap_or_else(|e| panic!("uninstall the service as {account}: {}", e.report()));
        assert!(log.is_file(), "an uninstall keeps {}", log.display());

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
    assert_eq!(
        accounts_of_default_log(id),
        [sid_of(5, &LOCAL_SERVICE)],
        "a new task names its account alone"
    );
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
    assert_eq!(
        accounts_of_default_log(id),
        [sid_of(5, &NETWORK_SERVICE)],
        "a change of account that landed names the new account alone"
    );
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

/// A program that does not exist, which the shim cannot spawn and says so in its log: the default log, since
/// the task has no `logs`. Absolute, as a command must be.
fn missing_program(host: &ServiceAccountHost) -> Vec<String> {
    vec![format!("{}.missing", host.exe.path_str())]
}

/// What the default log of `id` holds after a run that failed.
fn default_log_of(id: &Id) -> String {
    let log = support::shim_default_log(&support::program_data(), id.as_str());
    std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("read the default log {}: {e}", log.display()))
}

/// The shim of a task run by an account that needs no password writes its failure to the default log, in
/// the folder the install made: the shim makes no folder, so the account's write in it is what is tested.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_shim_of_a_standard_accounts_task_writes_its_failure_to_the_default_log(#[fixture(scm_group)] _group: &Group) {
    for account in ["LocalService", "NetworkService"] {
        let id = &test_id();
        let host = ServiceAccountHost::new(id);
        let _guard = ServiceGuard::new(id.as_str());
        let mgr = host.manager();
        let spec = task_spec(id, missing_program(&host), None, User::Name(account.to_owned()), None);
        mgr.install(&spec, false)
            .unwrap_or_else(|e| panic!("install as {account}: {}", e.report()));
        let log = support::shim_default_log(&support::program_data(), id.as_str());
        assert!(log.is_file(), "{account}: the install made {}", log.display());

        let refused = mgr.run(id, &mut Vec::new()).expect_err("a program that is not there");

        assert!(
            matches!(refused.cause(), Error::RunUnrecorded { .. }),
            "{account}: {refused:?}"
        );
        assert!(
            default_log_of(id).contains(id.as_str()),
            "{account}: the shim wrote its failure to the default log"
        );
    }
}

// The accounts a default log names ------------------------------------------------------------------------------------

const LOCAL_SERVICE: [u32; 1] = [19];
const NETWORK_SERVICE: [u32; 1] = [20];

/// The accounts the default log of `id` lets append, apart from SYSTEM and Administrators, which it always
/// names: each trustee of an allow entry, sorted.
fn accounts_of_default_log(id: &Id) -> Vec<Vec<u8>> {
    let log = support::shim_default_log(&support::program_data(), id.as_str());
    let dacl =
        goetia::testing::file_dacl(&log).unwrap_or_else(|e| panic!("read the permissions of the log: {}", e.report()));
    assert!(dacl.protected, "{} inherits permissions", log.display());
    let mut accounts: Vec<Vec<u8>> = dacl
        .aces
        .expect("a DACL")
        .into_iter()
        .filter(|ace| ace.allow)
        .map(|ace| ace.trustee)
        .filter(|sid| *sid != sid_of(5, &[18]) && *sid != sid_of(5, &[32, 544]))
        .collect();
    accounts.sort();
    accounts
}

/// The service `id` that runs `command` as `account`.
fn service_running(id: &Id, account: &str, command: Vec<String>) -> DaemonSpec {
    let base = support::base_fields(id.as_str(), command);
    ServiceSpec::new(ServiceFields {
        common: DaemonFields {
            user: User::Name(account.to_owned()),
            ..base.common
        },
        protocol: Protocol::Plain,
        ..base
    })
    .map(DaemonSpec::from)
    .expect("a valid service")
}

/// [`service_running`] a program that runs until it is killed.
fn service_as(id: &Id, host: &ServiceAccountHost, account: &str) -> DaemonSpec {
    service_running(
        id,
        account,
        vec![host.exe.path_str(), RUN_UNTIL_KILLED.to_owned(), "0".to_owned()],
    )
}

/// What an install names in the default log is computed from the accounts it knows, never read back from the
/// file: the registered account alone for a new service and for one installed again as it is (an entry an
/// administrator added is gone), and for one whose account changes, the new account alone once it has.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_default_log_of_a_service_names_the_registered_account_alone(#[fixture(scm_group)] _group: &Group) {
    let id = &test_id();
    let host = ServiceAccountHost::new(id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let log = support::shim_default_log(&support::program_data(), id.as_str());
    let local_service = sid_of(5, &LOCAL_SERVICE);
    let network_service = sid_of(5, &NETWORK_SERVICE);

    mgr.install(&service_as(id, &host, "LocalService"), false)
        .unwrap_or_else(|e| panic!("install as LocalService: {}", e.report()));
    assert_eq!(
        accounts_of_default_log(id),
        vec![local_service.clone()],
        "a new service"
    );

    // An entry an administrator added, which an install that finds nothing to change still sets the log
    // without.
    let granted = support::cmd::run("icacls.exe", &[&log.to_string_lossy(), "/grant", "*S-1-5-32-545:(R)"]);
    granted.expect_ok();
    assert_eq!(accounts_of_default_log(id).len(), 2, "the entry was added");
    let again = mgr
        .install(&service_as(id, &host, "LocalService"), false)
        .unwrap_or_else(|e| panic!("install again: {}", e.report()));
    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(accounts_of_default_log(id), [local_service], "an install as it is");

    mgr.install(&service_as(id, &host, "NetworkService"), false)
        .unwrap_or_else(|e| panic!("install as NetworkService: {}", e.report()));
    assert_eq!(
        accounts_of_default_log(id),
        [network_service],
        "an account change that landed"
    );
}

/// Reads the default log of `id`: its lines.
fn log_lines(id: &Id) -> Vec<String> {
    default_log_of(id).lines().map(str::to_owned).collect()
}

/// A service that runs as one account while an install changes it to another: SCM does not restart a service
/// it updates, so the shim goes on as the first account with the second one the only account in the log's
/// permissions. The daemon's output and the shim's handle are the ones it opened at its start, so its next line
/// lands in `<id>.log`; after a restart the second account runs, and its lines land too. The program prints a
/// line, tells the test, and prints another when the test says, so no line is waited for by time.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_running_service_whose_account_changes_keeps_its_log_and_the_new_account_writes_it_after_a_restart(
    #[fixture(scm_group)] _group: &Group,
) {
    use std::io::Read as _;
    use std::net::Shutdown;

    let id = &test_id();
    let host = ServiceAccountHost::new(id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let listener = ConnectBack::listen();
    let as_account = |account: &str| {
        service_running(
            id,
            account,
            vec![
                host.exe.path_str(),
                PRINT_WHEN_RELEASED.to_owned(),
                listener.port().to_string(),
                "run".to_owned(),
            ],
        )
    };
    // The test releases the program that runs, and reads its answer: the line is in the file once it comes.
    let release = |mut stream: std::net::TcpStream| {
        stream.shutdown(Shutdown::Write).expect("release the program");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("the program's answer");
        assert_eq!(answer, "x");
    };

    mgr.install(&as_account("LocalService"), false)
        .unwrap_or_else(|e| panic!("install as LocalService: {}", e.report()));
    mgr.start(id, Budget::Unbounded)
        .unwrap_or_else(|e| panic!("start as LocalService: {}", e.report()));
    let first_run = listener.accept_stream("the program of the service, as LocalService");

    // The account changes under the running service.
    mgr.install(&as_account("NetworkService"), false)
        .unwrap_or_else(|e| panic!("install as NetworkService, while it runs: {}", e.report()));
    let status = mgr.status(id).expect("status");
    assert_eq!(status.common().state, State::Running, "an install does not restart it");
    assert_eq!(
        accounts_of_default_log(id),
        [sid_of(5, &NETWORK_SERVICE)],
        "the log names the new account alone"
    );

    release(first_run);
    let lines = log_lines(id);
    assert!(
        lines.iter().any(|line| line.starts_with("run first "))
            && lines.iter().any(|line| line.starts_with("run released ")),
        "the running service's output before and after the change: {lines:?}"
    );

    // A restart runs it as the new account, which the log names.
    mgr.stop(id, Budget::DEFAULT)
        .unwrap_or_else(|e| panic!("stop: {}", e.report()));
    mgr.start(id, Budget::Unbounded)
        .unwrap_or_else(|e| panic!("start as NetworkService: {}", e.report()));
    let second_run = listener.accept_stream("the program of the service, as NetworkService");
    release(second_run);
    let lines = log_lines(id);
    let count = |prefix: &str| lines.iter().filter(|line| line.starts_with(prefix)).count();
    assert_eq!(
        (count("run first "), count("run released ")),
        (2, 2),
        "both runs' lines: {lines:?}"
    );
    assert_eq!(accounts_of_default_log(id), [sid_of(5, &NETWORK_SERVICE)]);
}

/// A log deleted under a running service, whose account an install then changes, is made again naming both
/// accounts: the running shim reopens the path as the account it runs as, which a log naming the new one alone
/// would refuse. The next install, which finds the service as it is, names the new account alone.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_log_made_under_a_running_service_whose_account_changes_names_both_accounts_until_the_next_install(
    #[fixture(scm_group)] _group: &Group,
) {
    use std::net::Shutdown;

    let id = &test_id();
    let host = ServiceAccountHost::new(id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let listener = ConnectBack::listen();
    let as_account = |account: &str| {
        service_running(
            id,
            account,
            vec![
                host.exe.path_str(),
                PRINT_WHEN_RELEASED.to_owned(),
                listener.port().to_string(),
                "run".to_owned(),
            ],
        )
    };
    let (local_service, network_service) = (sid_of(5, &LOCAL_SERVICE), sid_of(5, &NETWORK_SERVICE));
    mgr.install(&as_account("LocalService"), false)
        .unwrap_or_else(|e| panic!("install as LocalService: {}", e.report()));
    mgr.start(id, Budget::Unbounded)
        .unwrap_or_else(|e| panic!("start as LocalService: {}", e.report()));
    let run = listener.accept_stream("the program of the service, as LocalService");

    let log = support::shim_default_log(&support::program_data(), id.as_str());
    std::fs::remove_file(&log).unwrap_or_else(|e| panic!("delete {} under the running shim: {e}", log.display()));
    mgr.install(&as_account("NetworkService"), false)
        .unwrap_or_else(|e| panic!("install as NetworkService, while it runs: {}", e.report()));
    let mut both = vec![local_service, network_service.clone()];
    both.sort();
    assert_eq!(
        accounts_of_default_log(id),
        both,
        "the running shim may reopen the log as LocalService"
    );

    let again = mgr
        .install(&as_account("NetworkService"), false)
        .unwrap_or_else(|e| panic!("install as NetworkService again: {}", e.report()));
    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(accounts_of_default_log(id), [network_service]);

    run.shutdown(Shutdown::Write).expect("release the program");
    mgr.stop(id, Budget::DEFAULT)
        .unwrap_or_else(|e| panic!("stop: {}", e.report()));
}

/// Ends the process of a parked service host if the test fails before it releases it, so that the service guard,
/// which stops and deletes the service, finds nothing parked in `ServiceMain`.
struct ParkedProcess(std::cell::Cell<Option<u32>>);

impl ParkedProcess {
    fn disarm(&self) {
        self.0.set(None);
    }
}

impl Drop for ParkedProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.0.get()
            && let Some(problem) =
                support::cmd::run_cleanup("taskkill.exe", &[&["/F", "/PID", &pid.to_string()]], &[128])
        {
            eprintln!("ParkedProcess[{pid}]: cleanup failed: {problem}");
        }
    }
}

/// SCM reads `START_PENDING` from its launch of a service's process until the process's first status report, which
/// the shim makes only after it holds its log (before it registers its control handler). An install that changes the
/// account of a service in that state therefore leaves both accounts in the log's permissions: a shim launched as the
/// old account may not have opened it yet, and the narrowing would deny it, so the whole run would have no default
/// log. The first install that finds the service up names the new account alone.
///
/// The host is a copy of this binary (`fixture`'s `before-handler`): it connects to SCM's dispatcher, and in
/// `ServiceMain`, before it registers a control handler, tells the test its pid and blocks until the test shuts down
/// its end of the connection. The window the narrowing rests on is measured: the test fails unless SCM reads
/// `START_PENDING` there.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn an_account_change_during_a_pending_start_leaves_both_accounts_in_the_log_until_the_next_install(
    #[fixture(scm_group)] _group: &Group,
) {
    use std::io::Read as _;
    use std::net::Shutdown;
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let id = &test_id();
    let host = this_binary_as_a_service_host(&format!("{id}-host"));
    let _guard = ServiceGuard::new(id.as_str());
    // After the service guard, so a failing test ends the parked process first.
    let parked = ParkedProcess(std::cell::Cell::new(None));
    let reached = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let mgr = manager_hosted_by(
        &host,
        vec![
            FIXTURE.to_owned(),
            id.to_string(),
            reached.port().to_string(),
            stopped.port().to_string(),
            "before-handler".to_owned(),
        ],
    );
    let as_account = |account: &str| service_running(id, account, vec![host.path_str()]);
    let local_service = sid_of(5, &LOCAL_SERVICE);
    let network_service = sid_of(5, &NETWORK_SERVICE);
    mgr.install(&as_account("LocalService"), false)
        .unwrap_or_else(|e| panic!("install as LocalService: {}", e.report()));
    assert_eq!(accounts_of_default_log(id), std::slice::from_ref(&local_service));

    // `StartService` returns once the service reports `RUNNING`, which the parked process does not do until it
    // is released.
    let name = id.to_string();
    let start = std::thread::spawn(move || {
        let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
        let service = scm
            .open_service(&name, ServiceAccess::START)
            .expect("open the service to start it");
        service.start::<&str>(&[])
    });
    let mut connection = reached.accept_stream("the service's process, parked in ServiceMain");
    let mut pid = String::new();
    connection.read_to_string(&mut pid).expect("the process's pid");
    parked.0.set(Some(pid.parse().expect("a pid")));
    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
    let service = scm
        .open_service(id.as_str(), ServiceAccess::QUERY_STATUS)
        .expect("open the service to read it");
    let status = service.query_status().expect("query the service");
    assert_eq!(
        status.current_state,
        ServiceState::StartPending,
        "SCM reports START_PENDING from the launch of the process until its first status report, with the          dispatcher connected and no control handler registered"
    );

    mgr.install(&as_account("NetworkService"), false)
        .unwrap_or_else(|e| panic!("install as NetworkService, during a pending start: {}", e.report()));
    let mut both = vec![local_service, network_service.clone()];
    both.sort();
    assert_eq!(
        accounts_of_default_log(id),
        both,
        "a shim launched as the old account may still be about to open the log"
    );

    // Releasing the process lets it register its handler and report `RUNNING`, which ends the start.
    connection.shutdown(Shutdown::Write).expect("release the process");
    parked.disarm();
    start
        .join()
        .expect("the start thread")
        .unwrap_or_else(|e| panic!("the released service starts: {e}"));
    assert_eq!(
        mgr.status(id).expect("status").common().state,
        State::Running,
        "the start has ended"
    );

    let again = mgr
        .install(&as_account("NetworkService"), false)
        .unwrap_or_else(|e| panic!("install as NetworkService again: {}", e.report()));
    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(
        accounts_of_default_log(id),
        [network_service],
        "an install that finds the service up names the new account alone"
    );

    mgr.stop(id, Budget::DEFAULT)
        .unwrap_or_else(|e| panic!("stop: {}", e.report()));
}
