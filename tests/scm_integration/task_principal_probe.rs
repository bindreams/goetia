//! PROBE (branch `probe/task-principal`, never merged): how Task Scheduler stores a task's principal, and what
//! goetia's reader makes of it, for a local user given each way, the built-in accounts, and a user deleted and
//! made again under the same name.

use std::process::Output;

use goetia::spec::{AccountId, Id, User};
use goetia::testing::{probe_principal, run_task, shown_sid, sid_for_account_name};

use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::{PRINT_AND_EXIT, REPORT_USER};
use crate::support::windows_tasks::{ServiceAccountHost, status_once_ended, task_spec};
use crate::support::{self, ConnectBack, Group, LocalUserGuard, ServiceGuard, cleanup, scm_group};
use crate::task_accounts::{as_user, assert_succeeded};
use crate::tasks::{SCHEDULE, test_id};

const PASSWORD: &str = LocalUserGuard::PASSWORD;

fn say(label: &str, body: &str) {
    eprintln!("PROBE>>> {label}\n{body}\nPROBE<<< {label}");
}

fn show_output(output: &Output) -> String {
    format!(
        "exit={:?} stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Turns the Task Scheduler operational log on for the probe and off after.
struct OperationalLog;

impl OperationalLog {
    const NAME: &'static str = "Microsoft-Windows-TaskScheduler/Operational";

    fn on() -> Self {
        let guard = Self;
        support::cmd::run("wevtutil.exe", &["sl", Self::NAME, "/e:true"]).expect_ok();
        guard
    }

    /// The events that name `id`, newest first.
    fn events_of(&self, id: &Id) -> String {
        let run = support::cmd::run("wevtutil.exe", &["qe", Self::NAME, "/c:300", "/rd:true", "/f:text"]);
        run.stdout
            .split("Event[")
            .filter(|block| block.contains(id.as_str()))
            .map(|block| block.lines().map(str::trim).collect::<Vec<_>>().join(" | "))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for OperationalLog {
    fn drop(&mut self) {
        cleanup::warn_left_behind(support::cmd::run_cleanup(
            "wevtutil.exe",
            &[&["sl", Self::NAME, "/e:false"]],
            &[],
        ));
    }
}

/// Items 1 and 2: what `<UserId>` reads back for a local user given each way, and for the built-in accounts given
/// each way the generator can write them, and what goetia's reader makes of each.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn probe_principal_spellings(#[fixture(scm_group)] _group: &Group) {
    let account = format!("gt{:08x}", rand::random::<u32>());
    let _user = LocalUserGuard::create(&account, PASSWORD);
    let machine = std::env::var("COMPUTERNAME").expect("COMPUTERNAME");
    let sid = shown_sid(&sid_for_account_name(&account).expect("sid of the user"));
    let spellings = [
        account.clone(),
        format!(r".\{account}"),
        format!(r"{machine}\{account}"),
        account.to_uppercase(),
        sid.clone(),
    ];
    for given in spellings {
        let id = &test_id();
        let host = ServiceAccountHost::new(id);
        let _guard = ServiceGuard::new(id.as_str());
        let command = vec![host.exe.path_str(), PRINT_AND_EXIT.to_owned()];
        let output = as_user("install", id, &given, &host, &command);
        say(
            &format!("user given as {given:?} (machine {machine}, sid {sid})"),
            &format!("install: {}\n{}", show_output(&output), probe_principal(id)),
        );
    }

    let names = [
        "LocalSystem",
        "SYSTEM",
        "system",
        r"NT AUTHORITY\SYSTEM",
        r"NT AUTHORITY\LocalSystem",
        "LocalService",
        r"NT AUTHORITY\LocalService",
        r"NT AUTHORITY\LOCAL SERVICE",
        "NetworkService",
        r"NT AUTHORITY\NetworkService",
        r"NT AUTHORITY\NETWORK SERVICE",
        "S-1-5-18",
        "S-1-5-19",
        "S-1-5-20",
    ];
    let mut builtins: Vec<(String, User)> = names
        .into_iter()
        .map(|name| (format!("name {name:?}"), User::Name(name.to_owned())))
        .collect();
    builtins.push(("root".to_owned(), User::Root));
    for sid in ["S-1-5-18", "S-1-5-19", "S-1-5-20"] {
        builtins.push((format!("id {sid}"), User::Id(AccountId::Sid(sid.to_owned()))));
    }
    for (label, user) in builtins {
        let id = &test_id();
        let host = ServiceAccountHost::new(id);
        let _guard = ServiceGuard::new(id.as_str());
        let mgr = host.manager();
        let command = vec![host.exe.path_str(), PRINT_AND_EXIT.to_owned()];
        let spec = task_spec(id, command, None, user, None);
        let installed = match goetia::manager::ServiceManager::install(&mgr, &spec, false) {
            Ok(outcome) => format!("OK {outcome:?}"),
            Err(e) => format!("ERR {}", e.report()),
        };
        say(
            &format!("builtin given as {label}"),
            &format!("install: {installed}\n{}", probe_principal(id)),
        );
    }
}

/// Items 3 and 4: a task of a local user, the user deleted (and made again under the same name when `recreate`),
/// then what the unchanged manifest's install answers. With `runs`, `Run` before the user is deleted, after, and
/// after the install.
fn principal_of_a_dead_account(spelled_as_sid: bool, recreate: bool, runs: bool) {
    use goetia::manager::ServiceManager as _;

    let log = OperationalLog::on();
    let name = format!("gt{:08x}", rand::random::<u32>());
    let _first_user = LocalUserGuard::create(&name, PASSWORD);
    let first_sid = shown_sid(&sid_for_account_name(&name).expect("sid of the user"));
    let given = if spelled_as_sid { first_sid.clone() } else { name.clone() };
    let id = &test_id();
    let host = ServiceAccountHost::new(id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let listener = ConnectBack::listen();
    let command = vec![host.exe.path_str(), REPORT_USER.to_owned(), listener.port().to_string()];
    let label = format!("given {given:?}, recreate={recreate}");
    let who = |when: &str| {
        let started = run_task(id);
        let reported = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener.accept_value("the program of the task")
        }));
        let reported = match reported {
            Ok(text) => format!("the program reported: {}", text.trim()),
            Err(_) => "the program reported nothing within 60 s".to_owned(),
        };
        say(
            &format!("{label}: Run {when}"),
            &format!("run_task: {:?}\n{reported}", started.map_err(|e| e.report().to_string())),
        );
    };

    let first = as_user("install", id, &given, &host, &command);
    say(&format!("{label}: first install"), &show_output(&first));
    assert_succeeded("first install", &first);
    say(&format!("{label}: stored after the first install"), &probe_principal(id));
    if runs {
        who("before the user is deleted");
        let ended = status_once_ended(&mgr, id);
        say(&format!("{label}: status after the first run"), &format!("{ended:?}"));
    }

    support::cmd::run("net.exe", &["user", &name, "/delete"]).expect_ok();
    // Declared after the first guard, so it goes first; the first then finds no account, which it takes as done.
    let _second_user = recreate.then(|| LocalUserGuard::create(&name, PASSWORD));
    let second_sid = sid_for_account_name(&name).map(|sid| shown_sid(&sid));
    say(
        &format!("{label}: the account now"),
        &format!("first sid {first_sid}; sid of the name now: {second_sid:?}"),
    );
    say(&format!("{label}: stored after the delete"), &probe_principal(id));
    if runs {
        who("after the delete, before the install");
    }

    let user = if spelled_as_sid {
        User::Id(AccountId::Sid(given.clone()))
    } else {
        User::Name(given.clone())
    };
    let spec = task_spec(id, command.clone(), Some(SCHEDULE), user, None);
    let with_password = as_user("install", id, &given, &host, &command);
    say(&format!("{label}: install again, with the password"), &show_output(&with_password));
    let without_password = match mgr.install(&spec, false) {
        Ok(outcome) => format!("OK {outcome:?}"),
        Err(e) => format!("ERR {}", e.report()),
    };
    say(&format!("{label}: install again, without the password"), &without_password);
    say(&format!("{label}: stored after the installs"), &probe_principal(id));
    if runs {
        who("after the install");
        say(&format!("{label}: Task Scheduler log"), &log.events_of(id));
        say(&format!("{label}: stored after the last run"), &probe_principal(id));
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn probe_a_task_under_a_recreated_account(#[fixture(scm_group)] _group: &Group) {
    principal_of_a_dead_account(false, true, true);
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn probe_a_task_under_a_deleted_account(#[fixture(scm_group)] _group: &Group) {
    principal_of_a_dead_account(false, false, false);
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn probe_a_sid_spelled_task_under_a_recreated_account(#[fixture(scm_group)] _group: &Group) {
    principal_of_a_dead_account(true, true, false);
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn probe_a_sid_spelled_task_under_a_deleted_account(#[fixture(scm_group)] _group: &Group) {
    principal_of_a_dead_account(true, false, false);
}
