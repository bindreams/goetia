//! The accounts a task runs as: a local user (a password logon, with the batch logon right goetia grants
//! it), and the virtual service account no task can run as.

use std::process::{Command, Output};

use goetia::error::Error;
use goetia::manager::{Exit, ServiceManager as _, State};
use goetia::spec::{Id, User};

use crate::install_helper::TASK_AS;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::CONNECT_BACK;
use crate::support::windows_tasks::ServiceAccountHost;
use crate::support::{self, ConnectBack, Group, LocalUserGuard, scm_group};
use crate::tasks::{SCHEDULE, TaskGuard, records_of, task_as, task_exists, task_status};

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
    let guard = TaskGuard::new();
    let id = &guard.id;
    let host = ServiceAccountHost::new(id);
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
