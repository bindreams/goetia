//! The daemon `type: simple` specs under test actually run: this test
//! binary itself, re-invoked in one of a few modes. Unlike
//! `tests/scm_integration/fixture.rs`, this is never dispatched to SCM —
//! for `type: simple`, SCM only ever launches `goetia-shim.exe <id>`; this
//! process is the shim's own supervised *child*, a perfectly ordinary
//! process from SCM's point of view.
//!
//! `<exe> --goetia-shim-fixture <mode> <args...>`
//!
//! - `report <port>` — connect to `<port>`, then block forever (a channel
//!   nobody sends to) until killed. The conformance suite's own
//!   `start_and_stop_are_idempotent` scenario, and every test here that just
//!   needs "a running daemon", use this.
//! - `spawn-grandchild <own-port> <grandchild-port>` — connect to
//!   `<own-port>`, spawn a *plain* (uncontained — this fixture has no
//!   `cosca` dependency of its own) grandchild running `report
//!   <grandchild-port>`, then block forever. `stop_kills_the_whole_process_tree`
//!   uses this to prove the shim's Job Object reaches descendants, not only
//!   the direct child.
//! - `cwd-env <port> <var>` — connect to `<port>`, write one line
//!   `cwd=<cwd>;env=<value-or-\0MISSING>`, then block forever.
//! - `write-log <port> <text>` — connect to `<port>`, write `<text>` to
//!   both stdout and stderr, then block forever.
//! - `exit <port> <code>` — connect to `<port>`, then exit with `<code>`.

use std::io::Write as _;
use std::net::{Ipv4Addr, TcpStream};
use std::sync::mpsc;

pub const FIXTURE: &str = "--goetia-shim-fixture";

pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(FIXTURE) {
        return false;
    }
    let mode = args[2].as_str();
    match mode {
        "report" => {
            let port: u16 = args[3].parse().expect("port");
            connect(port);
            block_forever();
        }
        "spawn-grandchild" => {
            let own_port: u16 = args[3].parse().expect("own port");
            let grandchild_port = args[4].clone();
            connect(own_port);
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
        other => panic!("unknown shim fixture mode: {other}"),
    }
    // Every arm above diverges (`block_forever`/`process::exit`/`panic!`):
    // this function returns `true` to its caller only in the sense that it
    // never returns at all once a recognized mode matched — `main`'s own
    // `if fixture::run_if_requested() { return; }` never actually observes
    // the `true` and it would be dead code to write one.
}

fn block_forever() -> ! {
    let (_tx, rx) = mpsc::channel::<()>();
    let _ = rx.recv();
    unreachable!("nobody ever sends on this channel; killed externally before recv returns");
}

/// Reporting in is best-effort: a failure to connect is indistinguishable
/// to this process from never having been started, and it is the test —
/// which holds the listener — that decides what that means.
fn connect(port: u16) {
    drop(TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
}

fn send_line(port: u16, value: &str) {
    if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
        let _ = stream.write_all(value.as_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
}
