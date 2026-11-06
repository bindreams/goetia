//! Does killing a `launchctl kickstart -p system/<label>` that is blocked on the job's respawn
//! throttle cancel the start request?
//!
//! goetia's bounded `start` leaves that `kickstart` running when it times out; with a long
//! `restart_delay` it lingers that long. Two questions:
//!
//! 1. After SIGTERM, or SIGKILL, to the client: does launchd still start the job when the throttle
//!    ends?
//! 2. With no kill (the control): when the throttle ends, does the `kickstart -p` start the job
//!    and return?
//!
//! Two job kinds, because `restart: always` (`KeepAlive`) respawns the job at the throttle's end
//! whatever a `kickstart` asked, so a run after a kill proves nothing there. `restart: on-failure`
//! with a job that exits 0 is not respawned by launchd, so a run after the throttle can only be the
//! `kickstart`'s.
//!
//! This is a measurement, not an assertion: the outcome is written with `support::record_probe`
//! as `launchd-kickstart-<case>.txt`. A run that never comes is shown with a bound that is printed
//! in the result, read by a human as "no run within N s", not as proof.

use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use goetia::backend::launchd::manager::{Launchd, STAGING_DIR};
use goetia::manager::ServiceManager;
use goetia::spec::{DaemonSpec, Id, Kind, Restart};

use crate::support::{self, ConnectBack, ELEVATED, cmd};

/// Opt-out for the probe, which takes under a minute per case: `SKULD_LABELS='!launchd-probe'`.
#[skuld::label]
const LAUNCHD_PROBE: skuld::Label;

/// The job's `ThrottleInterval`, written by goetia from `restart_delay`.
const THROTTLE: Duration = Duration::from_secs(20);
/// How long after the throttle's end the probe watches for a run. A failure bound printed in the
/// result, not a synchronization.
const SLACK: Duration = Duration::from_secs(15);
/// The polling step of the observation loop: the resolution of every time in the result.
const STEP: Duration = Duration::from_millis(20);

// Guards ==============================================================================================================

/// Installs `spec` and `uninstall`s it on drop (which boots the job out).
struct Guard(Id);

impl Guard {
    fn install(mgr: &Launchd, spec: &DaemonSpec) -> Self {
        let guard = Self(spec.id.clone());
        mgr.install(spec, false).expect("install");
        guard
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        match Launchd::new().uninstall(&self.0) {
            Ok(()) | Err(goetia::Error::NotInstalled { .. }) => {}
            Err(e) => eprintln!("Guard[{}]: cleanup failed: {e}", self.0),
        }
    }
}

/// The probe's own `launchctl kickstart` child, killed (SIGKILL) and reaped on drop. Declare it
/// after the [`Guard`], so it drops before the uninstall's `bootout`.
struct ClientGuard(Option<Child>);

impl ClientGuard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the client is spawned")
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // An already-exited child makes both calls do nothing.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// Measuring ===========================================================================================================

#[derive(Clone, Copy)]
enum Signal {
    /// The control: the client is left alone.
    None,
    Term,
    Kill,
}

impl Signal {
    fn name(self) -> &'static str {
        match self {
            Signal::None => "none",
            Signal::Term => "SIGTERM",
            Signal::Kill => "SIGKILL",
        }
    }

    fn number(self) -> Option<i32> {
        match self {
            Signal::None => None,
            Signal::Term => Some(libc::SIGTERM),
            Signal::Kill => Some(libc::SIGKILL),
        }
    }
}

fn spec_for(id: &str, port: u16, restart: Restart) -> DaemonSpec {
    DaemonSpec {
        kind: Kind::Simple,
        restart,
        restart_delay: Some(THROTTLE),
        ..support::spec(
            id,
            vec![
                support::current_exe_str(),
                support::sentinel::CONNECT_BACK.to_string(),
                port.to_string(),
            ],
        )
    }
}

/// `launchctl print system/<id>`, whatever its status.
fn print_job(id: &str) -> cmd::Run {
    cmd::run("launchctl", &["print", &format!("system/{id}")])
}

fn field<'a>(printed: &'a str, key: &str) -> Option<&'a str> {
    printed
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(key))
        .map(str::trim)
}

/// Seconds of `at` relative to `base`, negative when earlier.
fn rel(at: Instant, base: Instant) -> f64 {
    match at.checked_duration_since(base) {
        Some(d) => d.as_secs_f64(),
        None => -base.duration_since(at).as_secs_f64(),
    }
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("killed by signal {sig}"),
        (None, None) => format!("{status:?}"),
    }
}

fn read_all(pipe: Option<impl std::io::Read>) -> String {
    let mut text = String::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_string(&mut text);
    }
    text
}

fn run_case(name: &str, restart: Restart, signal: Signal) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let spec = spec_for(&id, listener.port(), restart);
    let target = format!("system/{id}");

    // Registered before the job is loaded and before the client exists.
    let _guard = Guard::install(&mgr, &spec);
    let mut client = ClientGuard(None);

    // `RunAtLoad` (implied by `KeepAlive`) runs the job at load; it reports in and exits.
    let staged = Path::new(STAGING_DIR).join(format!("{id}.plist"));
    cmd::run(
        "launchctl",
        &["bootstrap", "system", staged.to_str().expect("UTF-8 path")],
    )
    .expect_ok();
    listener.accept("the job's first run");
    let first_run = Instant::now();
    let throttle_end = first_run + THROTTLE;
    let bound = throttle_end + SLACK;
    // The run is still exiting when it reports in: wait for launchd to record the exit, as the
    // bounded-start test does.
    support::kill::until(|| {
        let printed = print_job(&id);
        field(&printed.stdout, "last exit code = ").is_some_and(|code| !code.contains("never exited"))
            && field(&printed.stdout, "pid = ").is_none()
    });

    let kicked = Instant::now();
    client.0 = Some(
        Command::new("launchctl")
            .args(["kickstart", "-p", &target])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn launchctl kickstart"),
    );

    // Throttled: the request is blocked and the job is not running, which `launchctl print` reads
    // as `state = spawn scheduled` with no pid. Ends early, and the result says so, when the client
    // has already returned or the bound passes.
    let (armed_by, state_before_signal) = loop {
        let printed = print_job(&id).stdout;
        if client.child().try_wait().expect("poll the client").is_some() {
            break ("the client had already exited", printed);
        }
        if field(&printed, "state = ") == Some("spawn scheduled") && field(&printed, "pid = ").is_none() {
            break ("`state = spawn scheduled`, no pid", printed);
        }
        if Instant::now() >= bound {
            break ("the observation bound (never read as throttled)", printed);
        }
    };

    let signalled = signal.number().map(|number| {
        let at = Instant::now();
        let pid = client.child().id() as i32;
        // SAFETY: `kill` takes a pid and a signal number and touches no memory. The pid is this
        // probe's own child, not yet reaped (`try_wait` above returned `None`, or the call fails
        // with ESRCH, which is recorded).
        let rc = unsafe { libc::kill(pid, number) };
        (at, rc, std::io::Error::last_os_error())
    });

    // Watch to the bound: runs of the job, and the client's exit.
    let mut runs: Vec<Instant> = Vec::new();
    let mut client_exit: Option<(Instant, ExitStatus)> = None;
    while Instant::now() < bound {
        if client_exit.is_none() {
            if let Some(status) = client.child().try_wait().expect("poll the client") {
                client_exit = Some((Instant::now(), status));
            }
        }
        if listener.connected_yet() {
            runs.push(Instant::now());
        }
        std::thread::sleep(STEP);
    }
    let state_at_bound = print_job(&id).stdout;
    let (client_out, client_err) = match client_exit {
        Some(_) => {
            let child = client.child();
            (read_all(child.stdout.take()), read_all(child.stderr.take()))
        }
        None => (String::new(), String::new()),
    };

    let after_signal = |at: Instant| {
        signalled
            .as_ref()
            .map(|(sent, ..)| format!(", {:+.2} s relative to the signal", rel(at, *sent)))
            .unwrap_or_default()
    };
    let mut lines = vec![
        format!("case: {name}"),
        format!("job restart policy: {restart:?}"),
        format!("throttle (ThrottleInterval): {} s", THROTTLE.as_secs()),
        format!("signal to the client: {}", signal.name()),
        format!(
            "observation bound: throttle end + {} s (first run + {} s)",
            SLACK.as_secs(),
            (THROTTLE + SLACK).as_secs()
        ),
        "times: seconds relative to the estimated throttle end (first run + throttle); resolution 20 ms".to_string(),
        format!("first run: {:+.2} s", rel(first_run, throttle_end)),
        format!("kickstart spawned: {:+.2} s", rel(kicked, throttle_end)),
        format!("throttled state read by: {armed_by}"),
    ];
    lines.push(match &signalled {
        Some((at, rc, err)) => format!(
            "signal sent: {:+.2} s, kill() returned {rc}{}",
            rel(*at, throttle_end),
            if *rc == 0 { String::new() } else { format!(" ({err})") }
        ),
        None => "signal sent: none".to_string(),
    });
    lines.push(match client_exit {
        Some((at, status)) => format!(
            "client exited: {:+.2} s{}, {}",
            rel(at, throttle_end),
            after_signal(at),
            describe(status)
        ),
        None => "client exited: not by the bound".to_string(),
    });
    lines.push(format!("client stdout: {client_out:?}"));
    lines.push(format!("client stderr: {client_err:?}"));
    lines.push(format!("runs of the job after the kickstart: {}", runs.len()));
    for at in &runs {
        lines.push(format!(
            "  run at {:+.2} s relative to the throttle end{}",
            rel(*at, throttle_end),
            after_signal(*at)
        ));
    }
    lines.push(format!(
        "job ran after the throttle ended: {}",
        if runs.iter().any(|at| *at >= throttle_end) {
            "yes"
        } else {
            "no, within the bound"
        }
    ));
    lines.push(format!("launchctl print before the signal:\n{state_before_signal}"));
    lines.push(format!("launchctl print at the bound:\n{state_at_bound}"));

    support::record_probe(&format!("launchd-kickstart-{name}"), &lines.join("\n"));
}

// Cases ===============================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, LAUNCHD_PROBE])]
fn kickstart_on_failure_job_control() {
    run_case("on-failure-control", Restart::OnFailure, Signal::None);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, LAUNCHD_PROBE])]
fn kickstart_on_failure_job_sigterm() {
    run_case("on-failure-sigterm", Restart::OnFailure, Signal::Term);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, LAUNCHD_PROBE])]
fn kickstart_on_failure_job_sigkill() {
    run_case("on-failure-sigkill", Restart::OnFailure, Signal::Kill);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, LAUNCHD_PROBE])]
fn kickstart_always_job_control() {
    run_case("always-control", Restart::Always, Signal::None);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, LAUNCHD_PROBE])]
fn kickstart_always_job_sigterm() {
    run_case("always-sigterm", Restart::Always, Signal::Term);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, LAUNCHD_PROBE])]
fn kickstart_always_job_sigkill() {
    run_case("always-sigkill", Restart::Always, Signal::Kill);
}
