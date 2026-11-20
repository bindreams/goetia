//! Sentinel modes: the test binary re-invoked as the daemon under probe.
//!
//! The probes need a program the service manager can actually run, and before
//! any backend exists the only binary guaranteed to be on the host is this
//! one. `main` dispatches these modes before the harness starts, because in
//! them the process was launched by launchd or the SCM, not by a test runner.

use std::io::Write as _;
use std::net::{Ipv4Addr, TcpStream};

/// `<exe> --goetia-connect-back <port>`: connect to the test's listener and
/// exit. Used to prove a launchd job ran — or, by its absence after a
/// `bootout`, that it did not.
pub const CONNECT_BACK: &str = "--goetia-connect-back";

/// `<exe> --goetia-run-until-killed <port>`: connect to the test's listener
/// (proving the process is genuinely up and running, not merely that
/// `launchctl` returned success) and then block forever, so a test can go
/// on to observe `Running` state/pid and rely on it staying that way until
/// the manager stops it. Unlike `CONNECT_BACK`, which exits immediately
/// after reporting in and so could already be gone by the time a test's
/// next assertion runs.
pub const RUN_UNTIL_KILLED: &str = "--goetia-run-until-killed";

/// `<exe> --goetia-report-uid <port>`: connect to the test's listener and
/// send the process's real uid as decimal text, then exit. Used to prove
/// which account a job actually launched under — `UserName: root` in the
/// plist is a claim; this confirms launchd honoured it, proving whether
/// that key takes a name or a numeric uid (this backend always resolves
/// to a name; see `backend::account::resolve_account`'s doc
/// comment).
#[cfg(unix)]
pub const REPORT_UID: &str = "--goetia-report-uid";

/// `<exe> --goetia-contained-child`: spawn `/bin/sleep 300` as goetia spawns `launchctl`
/// (`goetia::testing::spawn_contained`, which is `backend::bounded::command`), print its pid and a
/// newline on stdout, and block forever. A test that signals this process's group sees whether the child
/// is in it.
#[cfg(unix)]
pub const CONTAINED_CHILD: &str = "--goetia-contained-child";

/// `<exe> --goetia-print-and-exit <code> <text>`: print `<text> <pid>` on a line and exit with `<code>`.
/// The pid tells one run's line from another's.
#[cfg(windows)]
pub const PRINT_AND_EXIT: &str = "--goetia-print-and-exit";

/// `<exe> --goetia-run-until-released <port>`: connect to the test's listener and wait until the test
/// drops its end of the connection, then exit 0. A run the test holds, and then lets end by itself:
/// `RUN_UNTIL_KILLED` ends only by a stop or a kill, which records another result.
#[cfg(windows)]
pub const RUN_UNTIL_RELEASED: &str = "--goetia-run-until-released";

/// `<exe> --goetia-print-when-released <port> <text>`: print `<text> first <pid>` on a line, connect to the
/// test's listener, and block reading the connection until the test shuts its writing half down; then print
/// `<text> released <pid>`, answer with one byte (so the test knows the line was written) and block forever.
/// The test chooses when the second line is written, which is how it writes a daemon's output after a change
/// it made to the service, with no wait on time.
#[cfg(windows)]
pub const PRINT_WHEN_RELEASED: &str = "--goetia-print-when-released";

/// `<exe> --goetia-service-host <service-name> <start-port> <stop-port>`:
/// a minimal SCM-aware service. Without one, `sc start` would fail with
/// ERROR_SERVICE_REQUEST_TIMEOUT and the registry probe would be claiming to
/// have survived a start/stop cycle it never performed.
#[cfg(windows)]
pub const SERVICE_HOST: &str = "--goetia-service-host";

/// Returns whether this process was launched as a sentinel and has now
/// finished its job.
pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(CONNECT_BACK) => {
            connect(args[2].parse().expect("port argument"));
            true
        }
        Some(RUN_UNTIL_KILLED) => {
            connect(args[2].parse().expect("port argument"));
            loop {
                std::thread::park();
            }
        }
        #[cfg(unix)]
        Some(REPORT_UID) => {
            report_uid(args[2].parse().expect("port argument"));
            true
        }
        #[cfg(unix)]
        Some(CONTAINED_CHILD) => contained_child(),
        #[cfg(windows)]
        Some(SERVICE_HOST) => {
            windows::run(&args[2]);
            true
        }
        #[cfg(windows)]
        Some(RUN_UNTIL_RELEASED) => {
            run_until_released(args[2].parse().expect("port argument"));
            true
        }
        #[cfg(windows)]
        Some(PRINT_WHEN_RELEASED) => print_when_released(args[2].parse().expect("port argument"), &args[3]),
        #[cfg(windows)]
        Some(PRINT_AND_EXIT) => {
            println!("{} {}", args[3], std::process::id());
            std::io::stdout().flush().expect("flush stdout");
            std::process::exit(args[2].parse().expect("exit code argument"));
        }
        _ => false,
    }
}

/// Reporting in is best-effort: a failure to connect is indistinguishable to
/// this process from never having been started, and it is the test — which
/// holds the listener — that decides what that means. A failure is logged to
/// stderr, which reaches anyone only where the launcher captures it (an
/// SCM-launched process, or a launchd job without `StandardErrorPath`, has none).
pub fn connect(port: u16) {
    drop(open(port));
}

/// [`connect`], then send `value` as the payload.
pub fn send_line(port: u16, value: &str) {
    if let Some(mut stream) = open(port) {
        match stream.write_all(value.as_bytes()) {
            Ok(()) => log_failure(port, "shutdown", stream.shutdown(std::net::Shutdown::Write)),
            Err(e) => log_failure(port, "write", Err(e)),
        }
    }
}

#[cfg(unix)]
fn report_uid(port: u16) {
    if let Some(mut stream) = open(port) {
        // SAFETY: `getuid` takes no arguments, reads no memory, and cannot fail.
        let uid = unsafe { libc::getuid() };
        log_failure(port, "write", write!(stream, "{uid}"));
    }
}

#[cfg(unix)]
fn contained_child() -> ! {
    // A job-control shell's background job, or a harness, may have left SIGINT ignored: the test sends
    // it to this process and means it to end the process.
    // SAFETY: restoring a signal's default disposition touches no memory.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    let child = goetia::testing::spawn_contained("/bin/sleep", &["300"]);
    println!("{}", child.pid());
    std::io::stdout().flush().expect("flush stdout");
    loop {
        std::thread::park();
    }
}

/// [`RUN_UNTIL_RELEASED`]. A connection that cannot be made ends the run at once, as it does for
/// [`connect`]: the test, which holds the listener, is the one that sees no connection.
#[cfg(windows)]
fn run_until_released(port: u16) {
    use std::io::Read as _;

    if let Some(mut stream) = open(port) {
        log_failure(port, "read", stream.read_to_end(&mut Vec::new()).map(drop));
    }
}

/// [`PRINT_WHEN_RELEASED`]. A connection that cannot be made is the test's to report (it waits for it).
#[cfg(windows)]
fn print_when_released(port: u16, text: &str) -> ! {
    use std::io::Read as _;

    println!("{text} first {}", std::process::id());
    std::io::stdout().flush().expect("flush stdout");
    if let Some(mut stream) = open(port) {
        let mut released = Vec::new();
        log_failure(port, "read", stream.read_to_end(&mut released).map(drop));
        println!("{text} released {}", std::process::id());
        std::io::stdout().flush().expect("flush stdout");
        log_failure(port, "write", stream.write_all(b"x"));
    }
    loop {
        std::thread::park();
    }
}

/// Connect to the test's listener, logging a failure to stderr.
pub fn open(port: u16) -> Option<TcpStream> {
    match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
        Ok(stream) => Some(stream),
        Err(e) => {
            log_failure(port, "connect", Err(e));
            None
        }
    }
}

fn log_failure(port: u16, what: &str, result: std::io::Result<()>) {
    if let Err(e) = result {
        eprintln!("{what} to 127.0.0.1:{port} failed: {e}");
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::sync::mpsc;

    use windows_service::{define_windows_service, service_dispatcher};

    use super::super::service_ctrl;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run(name: &str) {
        service_dispatcher::start(name, ffi_service_main).expect("connect to the SCM dispatcher");
    }

    fn service_main(_scm_arguments: Vec<OsString>) {
        // The ports come from the image path, not from `StartService`, so they
        // are read back off this process's own command line.
        let args: Vec<String> = std::env::args().collect();
        let name = args[2].clone();
        let start_port: u16 = args[3].parse().expect("start port");
        let stop_port: u16 = args[4].parse().expect("stop port");

        let (stop_tx, stop_rx) = mpsc::channel();
        let status_handle = service_ctrl::register(&name, Some(stop_tx));
        service_ctrl::report_running(status_handle, true);
        super::connect(start_port);

        // A real rendezvous with the control handler thread: the recv returns
        // when SCM delivers the stop, and not before.
        stop_rx.recv().expect("stop control");
        service_ctrl::report_stopped(status_handle, stop_port);
    }
}
