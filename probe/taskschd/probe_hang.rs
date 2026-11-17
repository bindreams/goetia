//! Throwaway probe: repeats the first run of a task of a new local user, as goetia's
//! `a_real_user_task_installs_arms_and_runs` does, and dumps the machine's state (minidumps of the Task Scheduler and
//! User Profile services and of itself, and the Operational, Application, Security, User Profile and System logs) when a
//! step has not finished in 45 seconds. Never merged.
//!
//! Claim (note: "Windows - A password-logon task's first start under a new local user can block for good in the User
//! Profile Service's call to the Sync Center notification handler, so Run returns and no process starts"): in about
//! one of ten rounds the step `Run` returns and nothing starts.
//! Platform: GitHub windows-latest runner (Windows Server 2025, image windows-2025-vs2026 20261103.298.1), elevated, run
//! by `cargo nextest` with the project's `ci` profile on a probe branch of goetia; it needs goetia's `tests/support`
//! (`LocalUserGuard`, `ServiceAccountHost`, `ConnectBack`) and its install helper, so it only builds inside that tree.
//! A correct run prints `[probe] <id> step 1` to `step 8` for each round, and for a stuck round `=== PROBE DUMP: ... stuck
//! at step 5` and the dumps' output.

use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use goetia::manager::{Exit, ServiceManager as _};
use goetia::spec::Id;

use crate::install_helper::TASK_AS;
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

/// Minidumps of the Task Scheduler's service, the User Profile service and this process, written to
/// `C:\probe-dumps` for the workflow to upload, and their stacks when `cdb` is there.
fn capture_dumps() {
    let s = |text: &str| text.to_owned();
    let _ = std::fs::create_dir_all(r"C:\probe-dumps");
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
        show(
            "minidump",
            "rundll32.exe",
            vec![
                s(r"C:\Windows\System32\comsvcs.dll,"),
                s("MiniDump"),
                pid.to_string(),
                format!(r"C:\probe-dumps\{name}.dmp"),
                s("full"),
            ],
        );
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

fn dump(id: &Id, step: usize) {
    eprintln!("=== PROBE DUMP: round of {id} stuck at step {step}");
    capture_dumps();
    let s = |text: &str| text.to_owned();
    let task = format!(r"\Goetia\{id}");
    show("schedule service", "sc.exe", vec![s("queryex"), s("Schedule")]);
    show(
        "schedule threads",
        "powershell.exe",
        vec![
            s("-NoProfile"),
            s("-Command"),
            s(
                "$p=(Get-CimInstance Win32_Service -Filter \"Name='Schedule'\").ProcessId; \"pid $p\"; (Get-Process -Id $p).Threads | Group-Object ThreadState,WaitReason | Format-Table Count,Name -AutoSize | Out-String -Width 200",
            ),
        ],
    );
    show("tasklist", "tasklist.exe", vec![s("/svc")]);
    show(
        "schtasks",
        "schtasks.exe",
        vec![s("/query"), s("/tn"), task, s("/v"), s("/fo"), s("list")],
    );
    for (label, channel, count) in [
        (
            "taskscheduler log",
            "Microsoft-Windows-TaskScheduler/Operational",
            "400",
        ),
        ("application log", "Application", "100"),
        ("security log", "Security", "300"),
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
    let program_data = support::program_data();
    let log = support::shim_default_log(&program_data, id.as_str());
    eprintln!(
        "--- shim log {}:\n{}",
        log.display(),
        std::fs::read_to_string(&log).unwrap_or_else(|e| e.to_string())
    );
    let records = support::task_records(id.as_str());
    eprintln!(
        "--- records {}: {:?}",
        records.display(),
        std::fs::read_dir(&records).map(|d| d.map(|e| e.map(|e| e.file_name())).collect::<Vec<_>>())
    );
}

fn round(n: usize) {
    let account = format!("gt{:08x}", rand::random::<u32>());
    let step = Arc::new(AtomicUsize::new(0));
    let id = test_id();
    eprintln!("[probe] round {n} {id} account {account}");
    {
        let (step, id) = (Arc::clone(&step), id.clone());
        std::thread::spawn(move || {
            let mut seen = usize::MAX;
            loop {
                std::thread::sleep(Duration::from_secs(45));
                let now = step.load(Ordering::SeqCst);
                if now == seen && now != 8 {
                    dump(&id, now);
                    return;
                }
                seen = now;
                if now == 8 {
                    return;
                }
            }
        });
    }
    let go = |n: usize| {
        step.store(n, Ordering::SeqCst);
        eprintln!("[probe] {id} step {n}");
    };
    go(1);
    let _user = LocalUserGuard::create(&account, PASSWORD);
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
    go(7);
    mgr.uninstall(&id).expect("uninstall");
    go(8);
}

macro_rules! rounds {
    ($($name:ident),*) => {
        $(
            #[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
            fn $name(#[fixture(scm_group)] _group: &Group) {
                for n in 0..5 {
                    round(n);
                }
            }
        )*
    };
}

rounds!(
    probe_a, probe_b, probe_c, probe_d, probe_e, probe_f, probe_g, probe_h, probe_i, probe_j, probe_k, probe_l
);
