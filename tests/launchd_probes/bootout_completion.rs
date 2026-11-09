//! When `launchctl bootout system/<label>` returns, has the job's process exited, and does
//! `launchctl print system/<label>` already fail?
//!
//! Two comments in `backend::launchd::manager` disagree: `start`'s verify block says a booted-out
//! job can still be tearing down, `request_start_after_stop` says `bootout` runs to completion.
//!
//! Six cases: a job that exits at once on SIGTERM, one that handles SIGTERM and exits
//! [`SLOW_EXIT`] later, and one that ignores SIGTERM (so launchd's SIGKILL at `ExitTimeOut`,
//! [`EXIT_TIMEOUT`], is the only way it ends), each with `KeepAlive` true and false.
//!
//! Per case: how long `bootout` took; right after it returned, one read of whether the job's pid is
//! alive (`kill(pid, 0)`) and of `launchctl print`'s exit code; and, from two watcher threads that
//! poll the real condition from before `bootout` is issued, when the pid first read as gone and
//! when `print` first failed. The watchers' bound is a probe-level failure guard, printed as "not
//! by the bound". `print` polling runs concurrently with `bootout`, so its times carry the
//! polling's own interference and a resolution of one poll (tens of ms).
//!
//! Cases run one after another in a single test: concurrent loads skew the timings, and one test
//! can write the cross-case table. Results go through `support::record_probe`.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard, cmd};

/// Opt-out for the probe, which takes about a minute: `SKULD_LABELS='!bootout-probe'`.
#[skuld::label]
const BOOTOUT_PROBE: skuld::Label;

/// The jobs' `ExitTimeOut`: how long launchd waits after SIGTERM before SIGKILL (default 20 s).
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the `Slow` job takes to exit after SIGTERM; under [`EXIT_TIMEOUT`].
const SLOW_EXIT: Duration = Duration::from_secs(2);
/// How far past `ExitTimeOut` the watchers look before giving up.
const GUARD: Duration = Duration::from_secs(30);

// Jobs ================================================================================================================

#[derive(Clone, Copy)]
enum Job {
    /// The sentinel itself: SIGTERM's default action ends it at once.
    Prompt,
    /// A `sh` that traps SIGTERM, waits [`SLOW_EXIT`], stops its child and exits 0.
    Slow,
    /// The sentinel with SIGTERM ignored (the disposition is inherited across `exec`).
    Ignore,
}

impl Job {
    fn name(self) -> &'static str {
        match self {
            Job::Prompt => "prompt",
            Job::Slow => "slow",
            Job::Ignore => "ignore",
        }
    }

    /// The job's `ProgramArguments`; each variant's sentinel reports in over loopback to `port`
    /// only after its SIGTERM handling is in place.
    fn program(self, port: u16) -> Vec<String> {
        let sentinel = [
            support::current_exe_str(),
            support::sentinel::RUN_UNTIL_KILLED.to_string(),
            port.to_string(),
        ];
        let script = match self {
            Job::Prompt => return sentinel.to_vec(),
            Job::Slow => format!(
                "trap 'sleep {}; kill $!; exit 0' TERM; \"$0\" \"$@\" & wait",
                SLOW_EXIT.as_secs()
            ),
            Job::Ignore => "trap '' TERM; exec \"$0\" \"$@\"".to_string(),
        };
        ["/bin/sh".to_string(), "-c".to_string(), script]
            .into_iter()
            .chain(sentinel)
            .collect()
    }
}

fn plist_path(label: &str) -> PathBuf {
    PathBuf::from(support::LAUNCHD_DAEMON_DIR).join(format!("{label}.plist"))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `RunAtLoad` so the job runs at `bootstrap`. goetia's own plists carry no `ExitTimeOut`, so the
/// plist is written here.
fn plist(label: &str, program: &[String], keep_alive: bool) -> String {
    let args: String = program
        .iter()
        .map(|arg| format!("\t\t<string>{}</string>\n", xml_escape(arg)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<{keep_alive}/>\n\
         \t<key>ExitTimeOut</key>\n\t<integer>{}</integer>\n\
         </dict>\n</plist>\n",
        EXIT_TIMEOUT.as_secs()
    )
}

// Observing ===========================================================================================================

/// `launchctl print system/<label>`, whatever its status.
fn print_job(label: &str) -> cmd::Run {
    cmd::run("launchctl", &["print", &format!("system/{label}")])
}

fn field<'a>(printed: &'a str, key: &str) -> Option<&'a str> {
    printed
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(key))
        .map(str::trim)
}

/// Whether `pid` still names a process (a zombie counts): `kill(pid, 0)`.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: `kill` takes a pid and a signal number and touches no memory; signal 0 sends nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The first instant `done` held, polling it back to back until `bound`; `None` if it never did.
fn watch(bound: Instant, done: impl Fn() -> bool + Send + 'static) -> JoinHandle<Option<Instant>> {
    std::thread::spawn(move || {
        loop {
            if done() {
                return Some(Instant::now());
            }
            if Instant::now() >= bound {
                return None;
            }
            std::thread::yield_now();
        }
    })
}

fn secs_since(at: Instant, base: Instant) -> f64 {
    at.duration_since(base).as_secs_f64()
}

fn seen(at: Option<Instant>, base: Instant) -> String {
    at.map_or("not by the bound".to_string(), |at| {
        format!("{:.3}", secs_since(at, base))
    })
}

// Measuring ===========================================================================================================

struct Row {
    name: String,
    cells: Vec<String>,
    detail: String,
}

const HEADER: [&str; 9] = [
    "case",
    "bootout took (s)",
    "bootout exit",
    "pid alive at return",
    "print exit at return",
    "pid gone at (s)",
    "print failed at (s)",
    "new run after",
    "print state at return",
];

fn measure(job: Job, keep_alive: bool) -> Row {
    let name = format!("{}-keepalive-{keep_alive}", job.name());
    let label = support::random_test_id();
    let target = format!("system/{label}");
    let listener = ConnectBack::listen();
    let program = job.program(listener.port());

    // Registered before the plist exists; its drop boots the job out and removes the plist.
    let _guard = ServiceGuard::new(&label);
    let path = plist_path(&label);
    fs::write(&path, plist(&label, &program, keep_alive)).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    // launchd refuses a group- or world-writable plist.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod plist");
    cmd::run(
        "launchctl",
        &["bootstrap", "system", path.to_str().expect("UTF-8 path")],
    )
    .expect_ok();
    listener.accept("the job's first run");

    // The job reported in, so launchd has spawned it; wait for `print` to say so too.
    let (pid, printed_before) = loop {
        let printed = print_job(&label);
        if let Some(pid) = field(&printed.stdout, "pid = ").and_then(|pid| pid.parse::<i32>().ok()) {
            break (pid, printed.stdout);
        }
        std::thread::yield_now();
    };

    let bound = Instant::now() + EXIT_TIMEOUT + GUARD;
    let pid_watch = watch(bound, move || !pid_alive(pid));
    let print_watch = {
        let label = label.clone();
        watch(bound, move || print_job(&label).code != Some(0))
    };

    let issued = Instant::now();
    let bootout = cmd::run("launchctl", &["bootout", &target]);
    let returned = Instant::now();
    // One read each, right after `bootout` returned.
    let alive_at_return = pid_alive(pid);
    let print_at_return = print_job(&label);

    let pid_gone = pid_watch.join().expect("pid watcher");
    let print_failed = print_watch.join().expect("print watcher");
    // `bootout` has returned and the old pid is gone (or the bound passed): a connection now is a
    // new run of the job.
    let new_run = listener.connected_yet();

    let cells = vec![
        name.clone(),
        format!("{:.3}", secs_since(returned, issued)),
        format!("{:?}", bootout.code),
        alive_at_return.to_string(),
        format!("{:?}", print_at_return.code),
        seen(pid_gone, issued),
        seen(print_failed, issued),
        new_run.to_string(),
        field(&print_at_return.stdout, "state = ").unwrap_or("-").to_string(),
    ];
    let detail = format!(
        "case: {name}\n\
         job: {} {program:?}\nKeepAlive: {keep_alive}\nExitTimeOut: {} s\njob pid: {pid}\n\
         times: seconds after `bootout` was issued\n\
         watcher bound: {} s after issue\n\
         bootout stderr: {:?}\n\n\
         launchctl print before bootout:\n{printed_before}\n\
         launchctl print right after bootout returned (exit {:?}):\n{}\n{}",
        job.name(),
        EXIT_TIMEOUT.as_secs(),
        (EXIT_TIMEOUT + GUARD).as_secs(),
        bootout.stderr,
        print_at_return.code,
        print_at_return.stdout,
        print_at_return.stderr,
    );
    Row { name, cells, detail }
}

fn table(rows: &[Row]) -> String {
    let mut lines = vec![HEADER.join(" | ")];
    lines.extend(rows.iter().map(|row| row.cells.join(" | ")));
    lines.join("\n")
}

// Probe ===============================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, BOOTOUT_PROBE])]
fn bootout_completion() {
    let mut rows = Vec::new();
    for job in [Job::Prompt, Job::Slow, Job::Ignore] {
        for keep_alive in [true, false] {
            let row = measure(job, keep_alive);
            support::record_probe(&format!("launchd-bootout-{}", row.name), &row.detail);
            rows.push(row);
        }
    }
    support::record_probe(
        "launchd-bootout-table",
        &format!(
            "ExitTimeOut {} s; the slow job exits {} s after SIGTERM\n\n{}\n",
            EXIT_TIMEOUT.as_secs(),
            SLOW_EXIT.as_secs(),
            table(&rows)
        ),
    );
}
