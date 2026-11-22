//! The daemon `protocol: plain` specs under test actually run: this test
//! binary itself, re-invoked in one of a few modes. Unlike
//! `tests/scm_integration/fixture.rs`, this is never dispatched to SCM —
//! for `protocol: plain`, SCM only ever launches the service host (the shim, `goetia-shim.exe`);
//! this process is the shim's own *child*, a perfectly ordinary
//! process from SCM's point of view.
//!
//! `<exe> --goetia-shim-fixture <mode> <args...>`
//!
//! - `report <port>` — write this process's own pid to `<port>`, then block
//!   forever (a channel nobody sends to) until killed. The conformance
//!   suite's own `start_and_stop_are_idempotent` scenario, and every test
//!   here that just needs "a running daemon" (via `ConnectBack::accept`,
//!   which discards the payload), use this; tests that need the pid itself
//!   use `accept_line`/`accept_value`.
//! - `spawn-grandchild <own-port> <grandchild-port>` — write this
//!   process's own pid to `<own-port>`, spawn a *plain* (uncontained — this
//!   fixture has no `cosca` dependency of its own) grandchild running
//!   `report <grandchild-port>`, then block forever.
//!   `stop_kills_the_whole_process_tree` uses this to prove the shim's Job
//!   Object reaches descendants, not only the direct child, confirming
//!   both pids are actually dead afterward rather than merely inferring it
//!   from the absence of a second connection.
//! - `cwd-env <port> <var>` — connect to `<port>`, write one line
//!   `cwd=<cwd>;env=<value-or-\0MISSING>`, then block forever.
//! - `write-log <port> <text>` — connect to `<port>`, write `<text>` to
//!   both stdout and stderr, then block forever.
//! - `exit <port> <code>` — connect to `<port>`, then exit with `<code>`.
//! - `user <port>` — connect to `<port>`, write the SID of the account this process runs as, then
//!   exit 0.
//! - `priority <port>` — connect to `<port>`, write this process's priority class (`0x20` is
//!   normal), then exit 0.
//! - `exit-on-close <port> <code>` — connect to `<port>`, hold the connection
//!   open until the test closes its end, then exit with `<code>`. The test
//!   decides when the daemon exits, so it can inspect the running service
//!   first. A failed connect exits 2.
//! - `catch <port> <record> <exit-on> [launcher]` — records every console event it receives as a line of
//!   `<record>` (`daemon: event=CTRL_C at_us=<unix microseconds>`), then, once its handler is in place,
//!   connects to `<port>` and writes its pid, and blocks. On the event `<exit-on>` names (`ctrl-c`,
//!   `ctrl-break`, `close`; `none` never) it records `daemon: exiting by the event` and exits 0; any other event
//!   is handled and ignored. `close` is `CTRL_CLOSE_EVENT`, which is what a task's `stop` sends.
//!   With `launcher` it starts a `catch-worker` of the same record first and reports nothing itself: the worker
//!   does, so a report means both handlers are in place. Once its handler is in place it also creates
//!   `<record>.started` ([`started_marker`]), so that a daemon that was never started is told from one that was
//!   started and received nothing.
//! - `catch-close <port> <record> <code>` — `catch` that, on the close event (what a task's `stop` sends),
//!   records `daemon: exiting by the event` and exits with `<code>`.
//! - `catch-hang <port> <record>` — `catch` that, on the close event, records it and never leaves its handler.
//! - `detach <port> <record> <runs-dir> <code|never>` — leaves the console (`FreeConsole`), so that the close
//!   event does not reach it, then writes its pid to `<port>` and waits for `<runs-dir>` (a task's run records) to
//!   hold a record, which only a stop makes the shim write while the program is alive. It notes the record's text in
//!   `<record>` (`daemon: record=<json>`) and then exits with `<code>`, or, for `never`, blocks forever.
//! - `catch-worker <record> <exit-on> <port>` — `catch` as the launcher's worker: on the event it takes 600 ms
//!   to shut down (a slow shutdown, which the launcher's own exit must not cut short), then records
//!   `worker: exiting by the event` and exits 0.
//!
//! `<exe> --goetia-shim-host <init|no-init>` is not a daemon but a host of the shim that a service runs
//! (`goetia::shim::main()`), as a consumer's program would: `init` has called `goetia::shim::init_host()`
//! first thing in `main` (as every invocation of this binary does), `no-init` has not.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, FreeConsole, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetPriorityClass};

use crate::support::sentinel::{connect, open, send_line};

pub const FIXTURE: &str = "--goetia-shim-fixture";

/// The argument that makes this binary a host of the shim, followed by `init` or `no-init`.
pub const HOST: &str = "--goetia-shim-host";

/// What the `no-init` host does, which must come before anything of `main` that calls `init_host`: runs
/// the shim and never returns; for any other invocation it returns at once.
pub fn run_uninitialised_host_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(HOST) && args.get(2).map(String::as_str) == Some("no-init") {
        goetia::shim::main();
    }
}

pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(HOST) {
        // `main` called `init_host` before this: the `init` host.
        assert_eq!(args.get(2).map(String::as_str), Some("init"), "{args:?}");
        goetia::shim::main();
    }
    if args.get(1).map(String::as_str) != Some(FIXTURE) {
        return false;
    }
    let mode = args[2].as_str();
    match mode {
        "report" => {
            let port: u16 = args[3].parse().expect("port");
            send_line(port, &std::process::id().to_string());
            block_forever();
        }
        "spawn-grandchild" => {
            let own_port: u16 = args[3].parse().expect("own port");
            let grandchild_port = args[4].clone();
            send_line(own_port, &std::process::id().to_string());
            let exe = std::env::current_exe().expect("current_exe");
            // Never waited on: this process (its parent) blocks forever
            // right below, until the Job Object the shim under test
            // assigned it to kills the whole tree — including this
            // grandchild — from outside. There is no point in this
            // process's own lifetime where waiting on it would ever return.
            #[allow(clippy::zombie_processes)]
            std::process::Command::new(exe)
                .arg(FIXTURE)
                .arg("report")
                .arg(&grandchild_port)
                .spawn()
                .expect("spawn grandchild");
            block_forever();
        }
        "cwd-env" => {
            let port: u16 = args[3].parse().expect("port");
            let var = args[4].as_str();
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|e| format!("<error: {e}>"));
            let value = std::env::var(var).unwrap_or_else(|_| "\u{0}MISSING".to_string());
            send_line(port, &format!("cwd={cwd};env={value}"));
            block_forever();
        }
        "write-log" => {
            let port: u16 = args[3].parse().expect("port");
            let text = args[4].as_str();
            println!("{text}");
            eprintln!("{text}");
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            connect(port);
            block_forever();
        }
        "exit" => {
            let port: u16 = args[3].parse().expect("port");
            let code: i32 = args[4].parse().expect("code");
            connect(port);
            std::process::exit(code);
        }
        "exit-on-close" => {
            let port: u16 = args[3].parse().expect("port");
            let code: i32 = args[4].parse().expect("code");
            let Some(mut stream) = open(port) else {
                std::process::exit(2);
            };
            let mut buf = [0u8; 64];
            // The test never writes: the read ends with `Ok(0)` when it drops
            // its end, or `Err` if the connection is reset.
            while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
            std::process::exit(code);
        }
        "user" => {
            let port: u16 = args[3].parse().expect("port");
            send_line(port, &own_user_sid());
            std::process::exit(0);
        }
        "priority" => {
            let port: u16 = args[3].parse().expect("port");
            // SAFETY: `GetCurrentProcess` is a pseudo-handle that is always valid.
            let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
            send_line(port, &format!("{class:#x}"));
            std::process::exit(0);
        }
        "catch" => {
            let port: u16 = args[3].parse().expect("port");
            let record = PathBuf::from(&args[4]);
            let exit_on = exit_on(&args[5]);
            let launcher = args.get(6).map(String::as_str) == Some("launcher");
            catch_events(Catch {
                record: record.clone(),
                exit_on,
                role: if launcher { "launcher" } else { "daemon" },
                linger: Duration::ZERO,
                exit_code: 0,
                hangs: false,
            });
            if launcher {
                // Not waited on: it ends by the event, or with the job that holds this tree.
                #[allow(clippy::zombie_processes)]
                std::process::Command::new(std::env::current_exe().expect("current_exe"))
                    .arg(FIXTURE)
                    .arg("catch-worker")
                    .arg(&record)
                    .arg(&args[5])
                    .arg(port.to_string())
                    .spawn()
                    .expect("spawn the worker");
            } else {
                send_line(port, &std::process::id().to_string());
            }
            block_forever();
        }
        "catch-close" => {
            let port: u16 = args[3].parse().expect("port");
            catch_events(Catch {
                record: PathBuf::from(&args[4]),
                exit_on: exit_on("close"),
                role: "daemon",
                linger: Duration::ZERO,
                exit_code: args[5].parse().expect("code"),
                hangs: false,
            });
            send_line(port, &std::process::id().to_string());
            block_forever();
        }
        "catch-hang" => {
            let port: u16 = args[3].parse().expect("port");
            catch_events(Catch {
                record: PathBuf::from(&args[4]),
                exit_on: exit_on("close"),
                role: "daemon",
                linger: Duration::ZERO,
                exit_code: 0,
                hangs: true,
            });
            send_line(port, &std::process::id().to_string());
            block_forever();
        }
        "detach" => {
            let port: u16 = args[3].parse().expect("port");
            let record = PathBuf::from(&args[4]);
            let runs = PathBuf::from(&args[5]);
            // `never` ends nothing by itself: the test reads what Windows leaves.
            let code: Option<i32> = (args[6] != "never").then(|| args[6].parse().expect("code"));
            // SAFETY: no argument; it detaches this process from the console it shares with the shim.
            assert_ne!(
                unsafe { FreeConsole() },
                0,
                "FreeConsole: {}",
                std::io::Error::last_os_error()
            );
            send_line(port, &std::process::id().to_string());
            let seen = await_record(&runs);
            append_line(&record, "daemon", &format!("record={seen}"));
            match code {
                Some(code) => std::process::exit(code),
                None => block_forever(),
            }
        }
        "catch-worker" => {
            let record = PathBuf::from(&args[3]);
            let port: u16 = args[5].parse().expect("port");
            catch_events(Catch {
                record,
                exit_on: exit_on(&args[4]),
                role: "worker",
                linger: Duration::from_millis(600),
                exit_code: 0,
                hangs: false,
            });
            send_line(port, &std::process::id().to_string());
            block_forever();
        }
        other => panic!("unknown shim fixture mode: {other}"),
    }
    // Every arm above diverges (`block_forever`/`process::exit`/`panic!`):
    // this function returns `true` to its caller only in the sense that it
    // never returns at all once a recognized mode matched — `main`'s own
    // `if fixture::run_if_requested() { return; }` never actually observes
    // the `true` and it would be dead code to write one.
}

// Console events ------------------------------------------------------------------------------------------------------

/// What a `catch` process does with the events it receives.
struct Catch {
    record: PathBuf,
    exit_on: Option<u32>,
    role: &'static str,
    /// How long it takes to end once the event it exits on arrives.
    linger: Duration,
    /// The code it ends with.
    exit_code: i32,
    /// Whether it never leaves the handler of the event it exits on, instead of exiting.
    hangs: bool,
}

static CATCH: OnceLock<Catch> = OnceLock::new();

fn exit_on(name: &str) -> Option<u32> {
    match name {
        "ctrl-c" => Some(CTRL_C_EVENT),
        "ctrl-break" => Some(CTRL_BREAK_EVENT),
        "close" => Some(CTRL_CLOSE_EVENT),
        "none" => None,
        other => panic!("`{other}` is not an event to exit on"),
    }
}

/// Installs the handler. The process is not yet reported in: the caller does that once this has returned.
fn catch_events(catch: Catch) {
    let marker = started_marker(&catch.record);
    assert!(CATCH.set(catch).is_ok(), "one `catch` per process");
    // SAFETY: `on_event` is a function, so it lives for the whole process; `TRUE` adds it.
    let added = unsafe { SetConsoleCtrlHandler(Some(on_event), 1) };
    assert_ne!(added, 0, "SetConsoleCtrlHandler: {}", std::io::Error::last_os_error());
    if let Err(e) = std::fs::write(&marker, std::process::id().to_string()) {
        eprintln!("the start marker {} was not written: {e}", marker.display());
    }
}

/// The file a `catch` process of `record` creates once it is running with its handler in place.
pub fn started_marker(record: &Path) -> PathBuf {
    let mut name = record.as_os_str().to_owned();
    name.push(".started");
    PathBuf::from(name)
}

/// One line of the record, appended in one write so that the two processes of a launcher's tree cannot split
/// each other's.
fn note(catch: &Catch, line: &str) {
    append_line(&catch.record, catch.role, line);
}

fn append_line(record: &Path, role: &str, line: &str) {
    let line = format!("{role}: {line}\n");
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(record)
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(e) = written {
        eprintln!("the record {} was not written: {e}", record.display());
    }
}

unsafe extern "system" fn on_event(event: u32) -> i32 {
    let Some(catch) = CATCH.get() else { return 0 };
    let name = match event {
        CTRL_C_EVENT => "CTRL_C",
        CTRL_BREAK_EVENT => "CTRL_BREAK",
        CTRL_CLOSE_EVENT => "CTRL_CLOSE",
        _ => return 0,
    };
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_micros());
    note(catch, &format!("event={name} at_us={at}"));
    if Some(event) == catch.exit_on {
        std::thread::sleep(catch.linger);
        if catch.hangs {
            block_forever();
        }
        note(catch, "exiting by the event");
        std::process::exit(catch.exit_code);
    }
    1
}

/// The text of the run record (`<instance>.json`) once `dir` holds one. Each pass re-reads the directory, and none
/// ends the wait by its length.
fn await_record(dir: &Path) -> String {
    loop {
        let held = std::fs::read_dir(dir).ok().and_then(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
                .find_map(|entry| std::fs::read_to_string(entry.path()).ok())
        });
        if let Some(text) = held {
            return text;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The SID of the account this process runs as, from `whoami /user`: `"<name>","<sid>"` on one line.
fn own_user_sid() -> String {
    let output = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .expect("run whoami");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "whoami: {text} {:?}", output.stderr);
    text.trim()
        .rsplit(',')
        .next()
        .map(|sid| sid.trim_matches('"').to_string())
        .expect("whoami prints a SID")
}

fn block_forever() -> ! {
    let (_tx, rx) = mpsc::channel::<()>();
    let _ = rx.recv();
    unreachable!("nobody ever sends on this channel; killed externally before recv returns");
}
