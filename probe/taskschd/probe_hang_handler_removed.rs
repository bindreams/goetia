//! Throwaway probe: with the Sync Center profile-notification handler's subkey (`SyncCenter`) removed from
//! `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileNotification` (a workflow step before the tests, no reboot, no
//! User Profile Service restart), every round makes a new local user and logs it on for the first time, alternately by
//! the first run of a task of the user (even rounds) and by the first start of a service of the user (odd rounds). A
//! round passes when the user's process started and the User Profile Service logged a new event 67 (profile loaded).
//! Never merged.
//!
//! Claim (note: "Windows - A password-logon task's first start under a new local user can block for good in the User
//! Profile Service's call to the Sync Center notification handler, so Run returns and no process starts"): without the
//! removal about one first logon in ten stalls; with it, none does.
//! Platform: GitHub windows-latest runner (Windows Server 2025, image windows-2025-vs2026 20261103.298), elevated, run by
//! `cargo nextest` with the project's `ci` profile on a probe branch of goetia (`probe/hang-handler`, whose workflow ran
//! only this file's three tests, on 8 jobs, after a step that finds the subkey by the CLSID and removes it); it needs
//! goetia's `tests/support` and its install helper, so it only builds inside that tree. A correct round prints
//! `[probe] <id> step 1` to `step 8` and `[probe] <id> newest event 67 record <before> -> <after>`; a stalled one prints
//! `=== PROBE DUMP: ... stuck at step N` and the dumps' output.

use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use goetia::backend::scm::manager::Scm;
use goetia::manager::{Budget, Exit, ServiceManager as _, State};
use goetia::spec::Id;

use crate::install_helper::{INSTALL_AS, TASK_AS};
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::CONNECT_BACK;
use crate::support::windows_tasks::ServiceAccountHost;
use crate::support::{self, ConnectBack, Group, LocalUserGuard, ServiceGuard, scm_group};
use crate::tasks::test_id;

const PASSWORD: &str = "Goetia!Test12";

fn as_user(verb: &str, id: &Id, account: &str, host: &ServiceAccountHost, command: &[String]) -> Output {
    Command::new(support::current_exe_str())
        .args([TASK_AS, verb, id.as_str(), account, &host.shim_path()])
        .args(command)
        .env("GOETIA_SERVICE_PASSWORD", PASSWORD)
        .output()
        .expect("spawn the task helper")
}

/// Runs `program` on a thread of its own and prints what it says when it ends, so that one command that
/// never ends hides none of the others. Says first that it started.
fn show(label: &'static str, program: &'static str, args: Vec<String>) {
    eprintln!("[probe dump] start {label}: {program} {args:?}");
    std::thread::spawn(move || match Command::new(program).args(&args).output() {
        Ok(out) => eprintln!(
            "[probe dump] END {label} ({:?})\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => eprintln!("[probe dump] END {label}: {e}"),
    });
}

/// The pid of the service `name`, read from `sc queryex`.
fn pid_of_service(name: &str) -> Option<u32> {
    let out = Command::new("sc.exe").args(["queryex", name]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("PID")?.split(':').nth(1)?.trim().parse().ok())
}

/// The stacks of the Task Scheduler's service, the User Profile service and this process, when `cdb` is there.
fn capture_dumps() {
    let s = |text: &str| text.to_owned();
    let processes = [
        ("schedule", pid_of_service("Schedule")),
        ("profsvc", pid_of_service("ProfSvc")),
        ("test", Some(std::process::id())),
    ];
    for (name, pid) in processes {
        let Some(pid) = pid else {
            eprintln!("[probe dump] no pid for {name}");
            continue;
        };
        let cdb = r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe";
        if std::path::Path::new(cdb).exists() {
            show(
                "cdb stacks",
                cdb,
                vec![
                    s("-pv"),
                    s("-p"),
                    pid.to_string(),
                    s("-c"),
                    s(r".sympath srv*C:\sym*https://msdl.microsoft.com/download/symbols;.reload;~*k;q"),
                ],
            );
        } else {
            eprintln!("[probe dump] no cdb at {cdb}");
        }
    }
}

fn dump(id: &str, step: usize) {
    eprintln!("=== PROBE DUMP: round of {id} stuck at step {step}");
    capture_dumps();
    let s = |text: &str| text.to_owned();
    show("schedule service", "sc.exe", vec![s("queryex"), s("Schedule")]);
    show("tasklist", "tasklist.exe", vec![s("/svc")]);
    for (label, channel, count) in [
        (
            "taskscheduler log",
            "Microsoft-Windows-TaskScheduler/Operational",
            "200",
        ),
        (
            "user profile log",
            "Microsoft-Windows-User Profile Service/Operational",
            "60",
        ),
        ("system log", "System", "60"),
    ] {
        show(
            label,
            "wevtutil.exe",
            vec![s("qe"), s(channel), format!("/c:{count}"), s("/rd:true"), s("/f:text")],
        );
    }
}

/// Starts the watchdog of a round: when the round has stayed 45 seconds in one step, dumps the machine's state
/// (it ends nothing: a stalled round stays stalled and nextest's timeout names it).
fn watch(id: String, step: Arc<AtomicUsize>) {
    std::thread::spawn(move || {
        let mut seen = usize::MAX;
        loop {
            std::thread::sleep(Duration::from_secs(45));
            let now = step.load(Ordering::SeqCst);
            if now == 8 {
                return;
            }
            if now == seen {
                dump(&id, now);
                return;
            }
            seen = now;
        }
    });
}

/// The record number of the newest event 67 ("profile loaded") of the User Profile Service (0 for none). The log is
/// circular, so its event count stops growing once it is full; record numbers only grow. Rounds run one at a time,
/// so a round whose number after the logon is higher than before it loaded a profile.
fn count_profile_loads() -> usize {
    let out = Command::new("wevtutil.exe")
        .args([
            "qe",
            "Microsoft-Windows-User Profile Service/Operational",
            "/q:*[System[(EventID=67)]]",
            "/c:1",
            "/rd:true",
            "/f:xml",
        ])
        .output()
        .expect("spawn wevtutil");
    assert!(out.status.success(), "wevtutil: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split("<EventRecordID>")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .map_or(0, |id| id.parse().expect("a record number"))
}

/// Asserts that the round's logon loaded a profile: a newer event 67 was logged since `before`.
fn assert_profile_loaded(id: &str, before: usize) {
    let after = count_profile_loads();
    eprintln!("[probe] {id} newest event 67 record {before} -> {after}");
    if after > before {
        return;
    }
    // Names what the machine holds instead, once, and goes on: the test fails at its end.
    if MISSING_67.swap(true, Ordering::SeqCst) {
        return;
    }
    let profile_list = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";
    for (program, args) in [
        ("wevtutil.exe", vec!["qe", "System", "/c:25", "/rd:true", "/f:text"]),
        ("wevtutil.exe", vec!["qe", "Application", "/c:25", "/rd:true", "/f:text"]),
        ("sc.exe", vec!["queryex", "ProfSvc"]),
        ("reg.exe", vec!["query", "HKU"]),
        ("reg.exe", vec!["query", profile_list]),
    ] {
        let out = Command::new(program).args(&args).output().expect("spawn");
        eprintln!(
            "[probe] {id} {program} {args:?}:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    eprintln!(
        "[probe] {id} C:\\Users: {:?}",
        std::fs::read_dir(r"C:\Users").map(|d| d.map(|e| e.map(|e| e.file_name())).collect::<Vec<_>>())
    );
}

/// Set when a round's logon logged no new event 67.
static MISSING_67: AtomicBool = AtomicBool::new(false);

/// A new user's first logon by the first run of a task of the user.
fn task_round(n: usize) {
    let account = format!("gt{:08x}", rand::random::<u32>());
    let step = Arc::new(AtomicUsize::new(0));
    let id = test_id();
    eprintln!("[probe] round {n} (task) {id} account {account}");
    watch(id.to_string(), Arc::clone(&step));
    let go = |n: usize| {
        step.store(n, Ordering::SeqCst);
        eprintln!("[probe] {id} step {n}");
    };
    go(1);
    let _user = LocalUserGuard::create(&account, PASSWORD);
    let before = count_profile_loads();
    let host = ServiceAccountHost::new(&id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let listener = ConnectBack::listen();
    let command = vec![
        host.exe.path_str(),
        CONNECT_BACK.to_owned(),
        listener.port().to_string(),
    ];
    go(2);
    let out = as_user("install", &id, &account, &host, &command);
    assert!(out.status.success(), "install: {out:?}");
    go(3);
    let out = as_user("enable", &id, &account, &host, &command);
    assert!(out.status.success(), "enable: {out:?}");
    go(4);
    let out = as_user("disable", &id, &account, &host, &command);
    assert!(out.status.success(), "disable: {out:?}");
    go(5);
    let exit = mgr
        .run(&id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run: {}", e.report()));
    assert_eq!(exit, Exit::Code(0));
    go(6);
    listener.accept("the user's program to have run");
    assert_profile_loaded(id.as_str(), before);
    go(7);
    mgr.uninstall(&id).expect("uninstall");
    go(8);
}

/// A new user's first logon by the first start of a service of the user.
fn service_round(n: usize) {
    let account = format!("gt{:08x}", rand::random::<u32>());
    let step = Arc::new(AtomicUsize::new(0));
    let id = support::random_test_id();
    eprintln!("[probe] round {n} (service) {id} account {account}");
    watch(id.clone(), Arc::clone(&step));
    let go = |n: usize| {
        step.store(n, Ordering::SeqCst);
        eprintln!("[probe] {id} step {n}");
    };
    go(1);
    let _user = LocalUserGuard::create(&account, PASSWORD);
    let before = count_profile_loads();
    let _guard = ServiceGuard::new(&id);
    go(2);
    let out = Command::new(support::current_exe_str())
        .arg(INSTALL_AS)
        .arg(&id)
        .arg(format!(r".\{account}"))
        .env("GOETIA_SERVICE_PASSWORD", PASSWORD)
        .output()
        .expect("spawn the install helper");
    assert!(out.status.success(), "install as a real account: {out:?}");
    go(3);
    let mgr = Scm::new();
    let target = Id::try_from(id.as_str()).expect("a valid id");
    mgr.start(&target, Budget::Unbounded)
        .unwrap_or_else(|e| panic!("start: {e}"));
    go(4);
    assert_eq!(mgr.status(&target).expect("status").common().state, State::Running);
    assert_profile_loaded(&id, before);
    go(5);
    let _ = mgr.stop(&target, Budget::DEFAULT);
    go(8);
}

fn round(n: usize) {
    if n % 2 == 0 {
        task_round(n);
    } else {
        service_round(n);
    }
}

macro_rules! rounds {
    ($($name:ident),*) => {
        $(
            #[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
            fn $name(#[fixture(scm_group)] _group: &Group) {
                for n in 0..6 {
                    round(n);
                }
                assert!(!MISSING_67.load(Ordering::SeqCst), "a round logged no event 67");
            }
        )*
    };
}

// Three tests of six rounds: a runner's C: drive holds about 21 new users' profiles (the 22nd logon fails with "Windows
// cannot copy profile ... you do not have enough disk space", event 1600, and logs no event 67), so a job makes 18.
rounds!(hang_a, hang_b, hang_c);
