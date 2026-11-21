//! A `protocol: plain` service's stop asks the daemon to exit first (goetia#263, unit 3a), through the real
//! SCM and the real shim: the shim sends the console event its `send-signal` names, waits for the daemon's whole
//! tree up to `timeout`, and only then ends what is left.
//!
//! The daemon is `fixture`'s `catch` mode: it records the events it receives in a file and exits 0 on the one
//! it is told to exit on. A record that shows the event *and* the daemon's own exit is what says the daemon was
//! asked and answered, which a stop that ended it at once never produces.
//!
//! Each test logs the time from the stop being issued to the event reaching the daemon (the shim's ask
//! returns within it) and writes it to the probe results, so the 10 s that `stop` and `restart` allow beyond
//! the `timeout` rest on a measurement.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use goetia::backend::scm::exit_status::{RecordedExit, SHIM_REFUSED};
use goetia::backend::scm::manager::Scm;
use goetia::backend::scm::wait::StopSeen;
use goetia::manager::{Budget, ServiceManager as _};
use goetia::shim::{CheckPermissions, StandaloneShim};
use goetia::spec::{DaemonFields, DaemonSpec, ServiceFields, ServiceSpec, StopAction, StopSignal, User};
use goetia::testing::service_fields;
use windows_service::service::ServiceState;

use crate::common::{
    LogDir, ShimGuard, UNIT_DIR_EXCLUSIVE, embedded_manager, fixture_command_with_exe, manager, recorded_exit,
    scm_status, simple_spec,
};
use crate::fixture::HOST;
use crate::provision::default_folder;
use crate::supervisor_integration::pid_is_alive;
use crate::support::{self, ConnectBack, Group, WorldReadableExe, scm_group};

/// How the stop is issued.
#[derive(Clone, Copy, Debug)]
enum StopBy {
    /// `goetia daemon stop`: the library call it makes, with the budget the spec gives it.
    Goetia,
    /// `sc stop`: SCM's own control, which is what the Services snap-in and `net stop` send.
    Sc,
}

/// The daemon's signal and timeout, and what `catch` does.
#[derive(Clone, Copy)]
struct Setup {
    signal: StopSignal,
    timeout: Option<Duration>,
    /// What the daemon exits on: `ctrl-c`, `ctrl-break` or `none`.
    exit_on: &'static str,
    launcher: bool,
    /// Stop as soon as `start` returns, without waiting for the daemon to report in.
    stop_at_once: bool,
}

impl Setup {
    /// A daemon that exits on the event `signal` sends, with the default `timeout`.
    fn asked_by(signal: StopSignal) -> Setup {
        Setup {
            signal,
            timeout: Some(Duration::from_secs(30)),
            exit_on: event_of(signal),
            launcher: false,
            stop_at_once: false,
        }
    }
}

/// The event `signal` sends on Windows, as `catch` names the one to exit on.
fn event_of(signal: StopSignal) -> &'static str {
    match signal {
        StopSignal::Terminate | StopSignal::Interrupt | StopSignal::CtrlC => "ctrl-c",
        StopSignal::Quit | StopSignal::CtrlBreak => "ctrl-break",
        other => panic!("`{other}` sends no console event"),
    }
}

/// The record's name for `exit_on`'s event.
fn event_line(exit_on: &str) -> &'static str {
    match exit_on {
        "ctrl-c" => "CTRL_C",
        "ctrl-break" => "CTRL_BREAK",
        other => panic!("`{other}` is not an event"),
    }
}

/// The shim's log's name for `exit_on`'s event.
fn event_name(exit_on: &str) -> &'static str {
    match exit_on {
        "ctrl-c" => "Ctrl+C",
        "ctrl-break" => "Ctrl+Break",
        other => panic!("`{other}` is not an event"),
    }
}

/// One service to install, start and stop.
struct Run<'a> {
    mgr: &'a Scm,
    how: StopBy,
    setup: Setup,
    logs: LogDir,
    id: String,
    /// The program the daemon runs: this test binary, or a copy of it that an account can execute.
    exe: String,
    user: User,
}

/// A stopped service and what it left, held so that its cleanup runs after the test has read it: the guard
/// first, the directory it logs to after, which is the order they drop in.
struct Stopped {
    _guard: ShimGuard,
    logs: LogDir,
    id: String,
    /// What the fixtures reported in with: their pids.
    pids: Vec<String>,
    stop_took: Duration,
    /// When the stop was issued, to read the record's timestamps against.
    issued: SystemTime,
}

impl Stopped {
    fn record_path(&self) -> PathBuf {
        self.logs.path().join("record.txt")
    }

    /// The record, line by line: empty if the daemon never wrote one.
    fn record(&self) -> Vec<String> {
        match std::fs::read_to_string(self.record_path()) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("read the record {}: {e}", self.record_path().display()),
        }
    }

    /// The milliseconds from the stop being issued to the first event a `catch` process received, if one did.
    fn stop_to_event(&self) -> Option<i128> {
        let issued = self
            .issued
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_micros() as i128;
        self.record().iter().find_map(|line| {
            let at = line.split_once("at_us=")?.1.trim().parse::<i128>().ok()?;
            Some((at - issued) / 1000)
        })
    }

    /// Logs the timing, and writes it to the probe results as `name`.
    fn log_timing(&self, name: &str) {
        let line = format!(
            "{name}: stop to event {} ms, stop returned after {} ms\n",
            self.stop_to_event().map_or("(none)".to_string(), |ms| ms.to_string()),
            self.stop_took.as_millis()
        );
        support::record_probe(&format!("graceful-stop-{name}"), &line);
    }

    /// The service stopped cleanly, which is the record SCM does not restart from, and left nothing of the
    /// daemon's tree.
    fn assert_clean_and_gone(&self) {
        let status = scm_status(&self.id);
        assert_eq!(status.current_state, ServiceState::Stopped, "{status:?}");
        assert_eq!(
            recorded_exit(&self.id),
            RecordedExit { win32: 0, specific: 0 },
            "{status:?}"
        );
        for pid in &self.pids {
            assert!(!pid_is_alive(pid), "the daemon {pid} is still running after the stop");
        }
    }
}

/// A `protocol: plain` service run by `run.mgr`, started and stopped as `run` says. Each wait here ends on a
/// real event: the daemon reporting in (a failure bound in `ConnectBack`'s message), the notification that
/// the service stopped.
fn run_to_stop(run: Run) -> Stopped {
    let Run {
        mgr,
        how,
        setup,
        logs,
        id,
        exe,
        user,
    } = run;
    let guard = ShimGuard::new(&id);
    let reported = ConnectBack::listen();
    let record = logs.path().join("record.txt");
    let mut args = vec![
        reported.port().to_string(),
        record.to_str().expect("a UTF-8 path").to_string(),
        setup.exit_on.to_string(),
    ];
    if setup.launcher {
        args.push("launcher".to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let spec = ServiceSpec::new({
        let base = service_fields(simple_spec(&id, fixture_command_with_exe(&exe, "catch", &args), &logs));
        ServiceFields {
            common: DaemonFields { user, ..base.common },
            stop_action: StopAction {
                send_signal: setup.signal,
                timeout: setup.timeout,
            },
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap_or_else(|e| panic!("the spec: {}", e.report()));
    mgr.install(&spec, false)
        .unwrap_or_else(|e| panic!("install: {}", e.report()));
    let goetia_id = spec.common().id.clone();
    // Unbounded: a first logon under a new account builds a profile, which outruns any fixed number.
    mgr.start(&goetia_id, Budget::Unbounded)
        .unwrap_or_else(|e| panic!("start: {}", e.report()));
    let mut pids = Vec::new();
    if !setup.stop_at_once {
        pids.push(reported.accept_line("the daemon to install its handler and report in"));
    }

    let mut actor = match how {
        StopBy::Sc => Some(support::scm_wait::open(&id)),
        StopBy::Goetia => None,
    };
    let issued = SystemTime::now();
    let started = Instant::now();
    match how {
        StopBy::Goetia => mgr
            .stop(&goetia_id, None)
            .unwrap_or_else(|e| panic!("stop: {}", e.report())),
        StopBy::Sc => {
            support::cmd::run("sc.exe", &["stop", &id]).expect_ok();
            let actor = actor.as_mut().expect("opened above");
            support::scm_wait::wait_for_stopped(actor, &id, StopSeen::Requested);
        }
    }
    let stop_took = started.elapsed();
    Stopped {
        _guard: guard,
        logs,
        id,
        pids,
        stop_took,
        issued,
    }
}

/// [`run_to_stop`] for a daemon the stock shim runs as LocalSystem, from this test binary.
fn stop_the_catcher(how: StopBy, setup: Setup) -> Stopped {
    run_to_stop(Run {
        mgr: &manager(),
        how,
        setup,
        logs: LogDir::new(),
        id: support::random_test_id(),
        exe: support::current_exe_str(),
        user: User::Root,
    })
}

fn assert_asked_and_answered(stopped: &Stopped, exit_on: &str) {
    let record = stopped.record();
    let event = format!("daemon: event={}", event_line(exit_on));
    assert!(
        record.iter().any(|line| line.starts_with(&event)),
        "the daemon was not sent {}: {record:?}",
        event_line(exit_on)
    );
    assert_eq!(
        record.last().map(String::as_str),
        Some("daemon: exiting by the event"),
        "the daemon did not exit by the event: {record:?}"
    );
}

/// [`assert_asked_and_answered`] for a stop that may reach the daemon before its `main` has installed the handler,
/// which Windows' own handler then ends with no record. Either way the daemon was asked and not ended by the
/// teardown, and nothing here depends on which side of the installation the event lands:
/// - a record with lines in it shows the event and the daemon's exit by it;
/// - an empty record is a daemon ended by Windows' handler, or by the teardown unasked, and only the shim's own
///   log tells them apart: the teardown's kill of a daemon that was never asked is logged as ended unasked.
///
/// In both, the shim's log has the line it writes when the ask was sent, so a daemon that was asked is shown by
/// that line and not by the absence of another.
fn assert_asked_not_killed(stopped: &Stopped, exit_on: &str) {
    if !stopped.record().is_empty() {
        assert_asked_and_answered(stopped, exit_on);
    }
    let log_path = stopped.logs.path_for(&stopped.id);
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("read the daemon's log {}: {e}", log_path.display()));
    let prefix = format!("goetia-shim[{}]: ", stopped.id);
    let sent = format!("{prefix}asked the daemon to exit with {}", event_name(exit_on));
    assert!(
        log.lines().any(|line| line.starts_with(&sent)),
        "the shim's log has no line for the ask:\n{log}"
    );
    assert!(
        !log.lines()
            .any(|line| line.starts_with(&prefix) && line.contains("ended without being asked")),
        "the teardown ended the daemon without asking it:\n{log}"
    );
}

// The signals ---------------------------------------------------------------------------------------------------------

/// `goetia daemon stop` asks the daemon with the event its `send-signal` names: Ctrl+C for `terminate`,
/// `interrupt` and `ctrl-c`, Ctrl+Break for `quit` and `ctrl-break`. The daemon exits by itself, well before
/// its `timeout`, and the stop is the clean one SCM does not restart from.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn a_goetia_stop_asks_the_daemon_with_the_event_of_each_signal(#[fixture(scm_group)] _group: &Group) {
    for signal in [
        StopSignal::Terminate,
        StopSignal::Interrupt,
        StopSignal::CtrlC,
        StopSignal::Quit,
        StopSignal::CtrlBreak,
    ] {
        let setup = Setup::asked_by(signal);

        let stopped = stop_the_catcher(StopBy::Goetia, setup);

        assert_asked_and_answered(&stopped, setup.exit_on);
        assert!(
            stopped.stop_took < setup.timeout.expect("a timeout"),
            "{signal}: the stop took {:?}, which is its timeout: the daemon was ended, not asked",
            stopped.stop_took
        );
        stopped.assert_clean_and_gone();
        stopped.log_timing(&format!("goetia-stop-{signal}"));
    }
}

/// `sc stop` is the same stop: SCM's control reaches the shim as the same `STOP`.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn an_sc_stop_asks_the_daemon_with_ctrl_c_and_ctrl_break(#[fixture(scm_group)] _group: &Group) {
    for signal in [StopSignal::CtrlC, StopSignal::CtrlBreak] {
        let setup = Setup::asked_by(signal);

        let stopped = stop_the_catcher(StopBy::Sc, setup);

        assert_asked_and_answered(&stopped, setup.exit_on);
        stopped.assert_clean_and_gone();
        stopped.log_timing(&format!("sc-stop-{signal}"));
    }
}

/// A daemon that ignores the event is ended when its `timeout` is over, and not before: the stop takes at
/// least the `timeout`, the daemon was sent the event and never exited by it, and nothing of it is left.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn a_daemon_that_ignores_the_event_is_ended_when_its_timeout_is_over(#[fixture(scm_group)] _group: &Group) {
    let timeout = Duration::from_secs(3);
    let setup = Setup {
        timeout: Some(timeout),
        exit_on: "none",
        ..Setup::asked_by(StopSignal::CtrlC)
    };

    let stopped = stop_the_catcher(StopBy::Goetia, setup);

    let record = stopped.record();
    assert!(
        record.iter().any(|line| line.starts_with("daemon: event=CTRL_C")),
        "the daemon was not asked: {record:?}"
    );
    assert!(
        !record.iter().any(|line| line.contains("exiting")),
        "the daemon was to ignore the event: {record:?}"
    );
    assert!(
        stopped.stop_took >= timeout,
        "the daemon was ended after {:?}, before its timeout of {timeout:?}",
        stopped.stop_took
    );
    stopped.assert_clean_and_gone();
    stopped.log_timing("goetia-stop-ignored");
}

/// `send-signal: kill` asks nothing and waits for nothing: the daemon is sent no event.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn send_signal_kill_ends_the_daemon_at_once(#[fixture(scm_group)] _group: &Group) {
    let setup = Setup {
        signal: StopSignal::Kill,
        timeout: None,
        exit_on: "ctrl-c",
        launcher: false,
        stop_at_once: false,
    };

    let stopped = stop_the_catcher(StopBy::Goetia, setup);

    assert_eq!(
        stopped.record(),
        Vec::<String>::new(),
        "a `kill` sent the daemon an event"
    );
    stopped.assert_clean_and_gone();
}

/// A stop issued as soon as `start` returns reaches a daemon that has no console yet, so the first ask is
/// refused (nothing sent) and asked again: the daemon is still asked, and ends by itself, and not at its
/// `timeout`, which is what a stop that asked once would wait for. The event can reach the daemon before its
/// handler is installed, so the record is read by [`assert_asked_not_killed`].
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn a_stop_right_after_the_start_still_asks_the_daemon(#[fixture(scm_group)] _group: &Group) {
    let timeout = Duration::from_secs(20);
    let setup = Setup {
        timeout: Some(timeout),
        stop_at_once: true,
        ..Setup::asked_by(StopSignal::CtrlC)
    };

    let stopped = stop_the_catcher(StopBy::Goetia, setup);

    assert_asked_not_killed(&stopped, setup.exit_on);
    assert!(
        stopped.stop_took < timeout,
        "the stop took {:?}, which is its timeout: the daemon was never asked",
        stopped.stop_took
    );
    let status = scm_status(&stopped.id);
    assert_eq!(status.current_state, ServiceState::Stopped, "{status:?}");
    assert_eq!(recorded_exit(&stopped.id), RecordedExit { win32: 0, specific: 0 });
    stopped.log_timing("goetia-stop-right-after-start");
}

/// A launcher exits at the event while its worker, which got the event too, takes 600 ms to shut down: the
/// stop waits for the worker, whose record shows it finished, and does not end it at the launcher's exit.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn the_worker_of_a_launcher_that_exits_late_is_given_the_timeout(#[fixture(scm_group)] _group: &Group) {
    let setup = Setup {
        launcher: true,
        ..Setup::asked_by(StopSignal::CtrlC)
    };

    let stopped = stop_the_catcher(StopBy::Goetia, setup);

    let record = stopped.record();
    assert!(
        record.iter().any(|line| line == "launcher: exiting by the event"),
        "the launcher did not exit by the event: {record:?}"
    );
    assert!(
        record.iter().any(|line| line == "worker: exiting by the event"),
        "the worker was ended before it finished its shutdown: {record:?}"
    );
    stopped.assert_clean_and_gone();
    stopped.log_timing("goetia-stop-launcher");
}

// Accounts and hosts --------------------------------------------------------------------------------------------------

/// The account a service runs as, from the service's id: a virtual account is named for it.
type AccountOf = fn(&str) -> User;

/// The proxy runs as the shim's own account: a service account that is not LocalSystem is asked as well, with
/// its output in a file the account can write.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_service_account_is_asked_through_the_proxy(#[fixture(scm_group)] _group: &Group) {
    let (_folder, _folder_guard) = default_folder();
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let accounts: [(&str, AccountOf); 2] = [
        ("local-service", |_| User::Name("LocalService".to_string())),
        ("virtual-account", |id| User::Name(format!(r"NT SERVICE\{id}"))),
    ];
    for (label, user) in accounts {
        for signal in [StopSignal::CtrlC, StopSignal::CtrlBreak] {
            let setup = Setup::asked_by(signal);
            let id = support::random_test_id();
            // Held until the service has stopped: the account cannot run the build tree's binary.
            let exe = WorldReadableExe::new(&id);

            let stopped = run_to_stop(Run {
                mgr: &mgr,
                how: StopBy::Goetia,
                setup,
                logs: LogDir::world_writable(),
                exe: exe.path_str(),
                user: user(&id),
                id,
            });

            assert_asked_and_answered(&stopped, setup.exit_on);
            stopped.assert_clean_and_gone();
            stopped.log_timing(&format!("{label}-{signal}"));
        }
    }
}

/// A manager whose services run this test binary as their host, started with `mode` after the host argument:
/// a consumer's own program that calls `goetia::shim::main()`.
fn host_manager(mode: &str) -> Scm {
    Scm::with_shim(
        StandaloneShim::new(
            PathBuf::from(support::current_exe_str()),
            vec![HOST.to_string(), mode.to_string()],
        ),
        CheckPermissions::Disable,
    )
}

/// A host that is not the stock shim, a consumer's program, asks as well: its `main` called
/// `goetia::shim::init_host()` first, so the copy of it that cosca starts as the proxy is the proxy.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn a_consumer_style_host_asks_the_daemon(#[fixture(scm_group)] _group: &Group) {
    let setup = Setup::asked_by(StopSignal::CtrlC);

    let stopped = run_to_stop(Run {
        mgr: &host_manager("init"),
        how: StopBy::Goetia,
        setup,
        logs: LogDir::new(),
        id: support::random_test_id(),
        exe: support::current_exe_str(),
        user: User::Root,
    });

    assert_asked_and_answered(&stopped, setup.exit_on);
    stopped.assert_clean_and_gone();
    stopped.log_timing("consumer-host");
}

/// A host that did not call `init_host` cannot be the proxy, so a service whose stop asks is refused when it
/// starts, with the shim's refusal record (which SCM does not restart) and a log line that names the call.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn a_host_that_did_not_initialise_is_refused_at_start(#[fixture(scm_group)] _group: &Group) {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let _guard = ShimGuard::new(&id);
    let reported = ConnectBack::listen();
    let mgr = host_manager("no-init");
    let spec = simple_spec(
        &id,
        fixture_command_with_exe(
            &support::current_exe_str(),
            "catch",
            &[
                &reported.port().to_string(),
                logs.path().join("record.txt").to_str().expect("a UTF-8 path"),
                "ctrl-c",
            ],
        ),
        &logs,
    );
    mgr.install(&spec, false).expect("install");

    let refused = mgr.start(&spec.common().id, Budget::DEFAULT);

    assert!(refused.is_err(), "a host that did not initialise was started");
    assert_eq!(
        recorded_exit(&id),
        RecordedExit {
            win32: 0,
            specific: SHIM_REFUSED
        },
        "{refused:?}"
    );
    let log = std::fs::read_to_string(logs.path_for(&id)).expect("the shim's log");
    assert!(
        log.contains("goetia::shim::init_host()"),
        "the log does not name the call:\n{log}"
    );
    assert!(
        !reported.connected_yet(),
        "the daemon was started by a host that was refused"
    );
}

/// The same host runs a service whose stop asks nothing: `send-signal: kill` needs no proxy, so it is not refused.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE])]
fn a_host_that_did_not_initialise_runs_a_service_that_is_killed(#[fixture(scm_group)] _group: &Group) {
    let setup = Setup {
        signal: StopSignal::Kill,
        timeout: None,
        exit_on: "ctrl-c",
        launcher: false,
        stop_at_once: false,
    };

    let stopped = run_to_stop(Run {
        mgr: &host_manager("no-init"),
        how: StopBy::Goetia,
        setup,
        logs: LogDir::new(),
        id: support::random_test_id(),
        exe: support::current_exe_str(),
        user: User::Root,
    });

    assert_eq!(stopped.record(), Vec::<String>::new());
    stopped.assert_clean_and_gone();
}
