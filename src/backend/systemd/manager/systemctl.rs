//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::process::Command;

use crate::backend::bounded::{self, Capture, Finished, Role};
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State, Status};

pub(super) fn run_systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| Error::Other(format!("failed to run `systemctl {}`: {e}", args.join(" "))))
}

pub(super) fn daemon_reload() -> Result<()> {
    let output = run_systemctl(&["daemon-reload"])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl daemon-reload failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// `daemon-reload` after a unit file write that itself succeeded. A failure here must not be reported
/// as a successful install — the on-disk artifact and systemd's loaded view of it have diverged.
pub(super) fn daemon_reload_or_report(id: &str) -> Result<()> {
    daemon_reload().map_err(|e| {
        Error::Other(format!(
            "wrote the unit for `{id}` but `systemctl daemon-reload` failed, so systemd may not have \
             picked it up yet: {e}"
        ))
    })
}

/// The oldest systemd goetia runs against: every start and stop runs `systemctl --show-transaction`
/// (242).
const SYSTEMD_FLOOR: u32 = 242;

/// The oldest systemd that parses the `Type=exec` generated units declare.
const TYPE_EXEC: u32 = 240;

/// Every version probe's environment. An inherited `SYSTEMD_COLORS` forces colour into a pipe, and
/// wraps `systemctl --version`'s number in escapes.
const UNCOLOURED: [(&str, &str); 1] = [("SYSTEMD_COLORS", "0")];

/// Refuse a systemd older than [`SYSTEMD_FLOOR`]. Asked wherever goetia writes a unit or starts or
/// stops one, and nowhere else: an older systemd does not reject `Type=exec` but logs that it cannot
/// parse it and runs the unit as `Type=simple`, silently losing the failed-exec detection the
/// directive is there for. Asked once per [`GateScope`].
pub(super) fn require_supported() -> Result<()> {
    gated(|| require_supported_via(&["systemctl"], &Host::this()))
}

thread_local! {
    /// How many [`GateScope`]s are open on this thread, and whether the version gate passed inside
    /// them.
    static GATE: Cell<(usize, bool)> = const { Cell::new((0, false)) };
}

/// While one lives on this thread, a version gate that passed there is not asked again: the steps of
/// one verb for one id — `restart`'s two legs, `install --start`'s install and start — share one
/// answer, and one pair of probes spends one share of the budget. Scopes nest and end in any order;
/// the answer is forgotten when the last one ends, so nothing outlives the invocation that opened
/// them, and a manager that changes between two is asked again.
pub(super) struct GateScope {
    this_thread: PhantomData<*const ()>,
}

pub(super) fn gate_scope() -> GateScope {
    let (open, passed) = GATE.get();
    GATE.set((open + 1, passed));
    GateScope {
        this_thread: PhantomData,
    }
}

impl Drop for GateScope {
    fn drop(&mut self) {
        let (open, passed) = GATE.get();
        GATE.set((open - 1, passed && open > 1));
    }
}

/// `check`, unless it already passed inside the open [`GateScope`]s. Only a pass is remembered: a
/// refusal ends the verb anyway, and one that did not is asked again.
pub(super) fn gated(check: impl FnOnce() -> Result<()>) -> Result<()> {
    if GATE.get().1 {
        return Ok(());
    }
    check()?;
    let (open, _) = GATE.get();
    GATE.set((open, open > 0));
    Ok(())
}

/// Whose version [`supported`] judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The running manager's `Version` property. PID 1 is what parses `Type=exec` and answers
    /// `--show-transaction`'s `EnqueueUnitJob`.
    Manager,
    /// `systemctl --version`, next to a running manager: the client is what parses the
    /// `--show-transaction` option, and a newer manager does not rescue an older client.
    Client,
    /// `systemctl --version` where no manager runs, and why. It stands in for the manager too: an
    /// image built offline boots the systemd it was built with.
    OfflineClient(NoManager),
}

/// The positive evidence that no systemd manager runs this system — the only grounds on which the
/// gate judges the client alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoManager {
    /// `SYSTEMD_OFFLINE` is set true, so `systemctl` asks no manager.
    Offline,
    /// `systemctl` says it runs in a chroot, and asks no manager.
    Chroot,
    /// `/run/systemd/system` does not exist: systemd is not this system's init (`sd_booted()`).
    NotBooted,
}

impl std::fmt::Display for NoManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NoManager::Offline => "`SYSTEMD_OFFLINE` is set",
            NoManager::Chroot => "`systemctl` says it is running in a chroot",
            NoManager::NotBooted => "`/run/systemd/system` does not exist",
        })
    }
}

/// Where `sd_booted()` looks: it exists iff systemd is this system's init.
const SD_BOOTED: &str = "/run/systemd/system";

/// What goetia reads of this system before asking `systemctl` for its manager: a seam, so a test can
/// stand any system in.
#[derive(Debug, Clone)]
struct Host {
    /// `SYSTEMD_OFFLINE`, as every `systemctl` goetia runs inherits it.
    offline: Option<String>,
    /// [`SD_BOOTED`] is established absent — see [`established_absent`].
    unbooted: bool,
}

impl Host {
    fn this() -> Host {
        Host {
            offline: std::env::var("SYSTEMD_OFFLINE").ok(),
            unbooted: established_absent(std::path::Path::new(SD_BOOTED)),
        }
    }

    /// Why no manager runs here, from what is known without asking `systemctl`.
    fn no_manager(&self) -> Option<NoManager> {
        if self.offline.as_deref().is_some_and(is_true) {
            Some(NoManager::Offline)
        } else if self.unbooted {
            Some(NoManager::NotBooted)
        } else {
            None
        }
    }
}

/// Whether `dir` is established not to be a directory, as `sd_booted()`'s `laccess("…/")` finds
/// it: not there, or something other than a directory, or under something other than one. A stat
/// that failed any other way establishes nothing.
fn established_absent(dir: &std::path::Path) -> bool {
    match std::fs::metadata(dir) {
        Ok(meta) => !meta.is_dir(),
        Err(e) => matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)),
    }
}

/// A true value as systemd's `parse_boolean` reads one, which is how `systemctl` reads
/// `SYSTEMD_OFFLINE`.
fn is_true(value: &str) -> bool {
    value == "1"
        || ["yes", "y", "true", "t", "on"]
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
}

/// What `systemctl` says, with its log level forced audible, when it asks no manager because it runs
/// in a chroot: "Running in chroot, ignoring command 'show'" on systemd 257, "… ignoring request."
/// before.
const IN_CHROOT: &str = "Running in chroot, ignoring";

/// [`require_supported`] with `systemctl`'s argv prefix and the host named, so a test can stand a
/// shell script and any system in. Both the running manager and the client must be new enough; the
/// client alone only on positive evidence that no manager runs this system ([`NoManager`]) — where
/// `install` must still work, since systemd enables units offline and an image build is the ordinary
/// case. Any other failure to read the manager's version is a refusal: a manager that is running and
/// could not be asked is not one goetia can vouch for.
fn require_supported_via(systemctl: &[&str], host: &Host) -> Result<()> {
    let client = probe(systemctl, &["--version"])?;
    if !client.status.success() {
        return Err(Error::Other(format!(
            "`systemctl --version` failed, so goetia cannot tell whether this is systemd {SYSTEMD_FLOOR}+: {}",
            String::from_utf8_lossy(&client.stderr)
        )));
    }
    let client = String::from_utf8_lossy(&client.stdout);
    let client = client.lines().next().unwrap_or_default();
    let no_manager = match host.no_manager() {
        Some(why) => why,
        None => {
            let manager = probe(systemctl, &["show", "--property=Version", "--value"])?;
            let reported = String::from_utf8_lossy(&manager.stdout);
            let said = String::from_utf8_lossy(&manager.stderr);
            if manager.status.success() && !reported.trim().is_empty() {
                let verdicts = [
                    verdict(Source::Manager, reported.trim()),
                    verdict(Source::Client, client),
                ];
                return refusal(verdicts.into_iter().flatten().collect());
            }
            if !(manager.status.success() && said.contains(IN_CHROOT)) {
                let how = if manager.status.success() {
                    "printed no version".to_string()
                } else {
                    format!("failed ({})", manager.status)
                };
                return Err(Error::Other(format!(
                    "`systemctl show --property=Version` {how}, so goetia cannot tell whether the running systemd \
                     is {SYSTEMD_FLOOR}+: {said}"
                )));
            }
            NoManager::Chroot
        }
    };
    refusal(verdict(Source::OfflineClient(no_manager), client).into_iter().collect())
}

/// One probe, uncoloured, and audible: a chroot's "ignoring" notice is `log_info`, which an inherited
/// `SYSTEMD_LOG_LEVEL=warning` would otherwise silence.
fn probe(systemctl: &[&str], args: &[&str]) -> Result<std::process::Output> {
    let (program, prefix) = systemctl.split_first().expect("a program to run");
    Command::new(program)
        .args(prefix)
        .args(args)
        .envs(UNCOLOURED)
        .envs(AUDIBLE)
        .output()
        .map_err(|e| Error::Other(format!("failed to run `systemctl {}`: {e}", args.join(" "))))
}

/// The major version in what `source` reported: the `Version` property (`257.13-1~deb13u1`,
/// `252-78.el9`, `258~rc1`), or `systemctl --version`'s first line (`systemd 257 (...)`). `None` for
/// anything else — escapes included, which a colour goetia failed to switch off would add.
fn major(source: Source, reported: &str) -> Option<u32> {
    if reported.chars().any(char::is_control) {
        return None;
    }
    let version = match source {
        Source::Manager => reported.strip_prefix('v').unwrap_or(reported),
        Source::Client | Source::OfflineClient(_) => reported.strip_prefix("systemd ")?,
    };
    let digits: String = version.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Why the version `source` reported is refused, as one sentence; `None` if it is not.
fn verdict(source: Source, reported: &str) -> Option<String> {
    const ANSWER: &str = "cannot answer the `systemctl --show-transaction` every start and stop runs";
    const TYPE_SIMPLE: &str = "runs goetia's `Type=exec` units as `Type=simple`, which cannot report a failed exec";
    let Some(n) = major(source, reported) else {
        let asked = match source {
            Source::Manager => "the running systemd reports",
            Source::Client | Source::OfflineClient(_) => "`systemctl --version` reports",
        };
        return Some(format!(
            "goetia cannot read the version {asked} ({reported:?}), so cannot tell whether it is."
        ));
    };
    if n >= SYSTEMD_FLOOR {
        return None;
    }
    let found = match source {
        Source::Manager => format!("The running systemd is {n}"),
        Source::Client => format!("The `systemctl` client is {n}"),
        Source::OfflineClient(why) => format!("No systemd runs here ({why}), and `systemctl --version` reports {n}"),
    };
    Some(match source {
        Source::Client => {
            format!("{found}, which does not accept the `--show-transaction` option every start and stop passes it.")
        }
        Source::Manager | Source::OfflineClient(_) if n >= TYPE_EXEC => format!("{found}, which {ANSWER}."),
        Source::Manager | Source::OfflineClient(_) => format!("{found}, which {ANSWER}, and {TYPE_SIMPLE}."),
    })
}

/// `Ok` for no verdicts; otherwise the refusal naming every one.
fn refusal(verdicts: Vec<String>) -> Result<()> {
    if verdicts.is_empty() {
        return Ok(());
    }
    Err(Error::Other(format!(
        "goetia requires systemd {SYSTEMD_FLOOR} or newer. {}",
        verdicts.join(" ")
    )))
}

/// The argv for one `systemctl <verb> <unit>` under `budget`.
///
/// `--show-transaction` on every path: its announcement ([`enqueued`]) is the only evidence that
/// systemd took the request — `systemctl` exits `0` having done nothing in a chroot or under
/// `SYSTEMD_OFFLINE=1` — and on the bounded path it is also what the wait holds out for. A budget
/// that does not wait adds `--no-block`, which still announces the job (measured on systemd 257).
///
/// Its own function so that each path's flags are observable without a timing bet: no elevated test
/// can see `--no-block` (`start` returns either way). `systemctl_tests.rs` asserts both argvs.
fn verb_args<'a>(verb: &'a str, unit: &'a str, budget: Budget) -> Vec<&'a str> {
    if budget.waits() {
        vec![verb, "--show-transaction", unit]
    } else {
        vec![verb, "--no-block", "--show-transaction", unit]
    }
}

/// `systemctl`'s environment on every path. The announcement is `log_info`: an inherited
/// `SYSTEMD_LOG_LEVEL=warning` or `SYSTEMD_LOG_TARGET=null` silences it, and without it a request
/// systemd took is indistinguishable from one it ignored.
const AUDIBLE: [(&str, &str); 2] = [("SYSTEMD_LOG_LEVEL", "info"), ("SYSTEMD_LOG_TARGET", "console")];

/// Whether `systemctl --show-transaction`'s stderr says systemd has answered the request with a
/// job — the line it writes after `StartUnit`/`StopUnit` returns and before it waits for the job.
/// A substring, not a whole line: `SYSTEMD_LOG_TIME`/`SYSTEMD_LOG_LOCATION` prefix it.
fn enqueued(stderr: &[u8]) -> bool {
    const LINE: &[u8] = b"Enqueued anchor job ";
    stderr.windows(LINE.len()).any(|w| w == LINE)
}

/// One `systemctl <verb> <unit>` under `budget`, as three deliberately different paths.
///
/// A budget that does not wait becomes `systemctl <verb> --no-block`, the exact native expression
/// of "issue the request and return": systemd enqueues the job and `systemctl` exits without
/// waiting for it to complete. `Budget::Unbounded` is the plain blocking call. Both keep
/// `Command::output()`, because neither has anything to bound — cosca enters this module only where
/// a bound is actually required, which is the third path and only the third.
///
/// That third path waits on `deadline`, which the caller derived at verb entry, so discovery spends
/// the budget too — but the deadline bounds only the wait for the job, never whether it is
/// enqueued: until `systemctl` announces the job ([`enqueued`]) the wait is unbounded, so a budget
/// that discovery used up, or one a few milliseconds long, still reaches systemd, and an expiry is
/// then exactly what `budget::timed_out` says it is. `budget` only picks the path and the wording.
fn run_verb(verb: &str, unit: &str, budget: Budget, deadline: Deadline) -> Result<Finished> {
    require_supported()?;
    run_verb_via("systemctl", &verb_args(verb, unit, budget), budget, deadline)
}

/// Whether [`run_verb`] takes its bounded path under `budget` — the one whose `systemctl` goetia
/// watches, and which needs a thread for each of its streams to do it.
pub(super) fn watched(budget: Budget) -> bool {
    budget.waits() && budget != Budget::Unbounded
}

/// [`run_verb`] with the program and argv named, so its bounded path is testable against a child
/// that cannot exit on its own.
fn run_verb_via(program: &str, args: &[&str], budget: Budget, deadline: Deadline) -> Result<Finished> {
    let failed = |e: String| Error::Other(format!("failed to run `{program} {}`: {e}", args.join(" ")));
    if !watched(budget) {
        let output = Command::new(program)
            .args(args)
            .envs(AUDIBLE)
            .output()
            .map_err(|e| failed(e.to_string()))?;
        // `output()` reads both pipes to EOF before returning, so on either
        // unbounded path nothing was cut short and `complete` is simply true.
        return Ok(Finished::Exited {
            status: output.status,
            capture: Capture {
                stdout: output.stdout,
                stderr: output.stderr,
                complete: true,
            },
        });
    }

    let mut cmd = bounded::command(program, args).map_err(|e| failed(e.to_string()))?;
    for (key, value) in AUDIBLE {
        cmd.env(key, value);
    }
    let spawned = bounded::spawn(&mut cmd, Role::AnnouncedRequest(enqueued)).map_err(|e| failed(e.to_string()))?;
    bounded::wait_bounded(spawned, deadline).map_err(|e| failed(e.to_string()))
}

/// Whether `line` is one `systemctl` writes on success as much as on failure — the transaction
/// `--show-transaction` announces, or a job's `finished` notice. Protocol, not diagnosis: left in, it
/// opens every failure message, and a diagnostic the deadline cut short can consist of nothing else.
fn is_protocol(line: &str) -> bool {
    enqueued(line.as_bytes())
        || line.contains("Enqueued auxiliary job ")
        || (line.contains("Job for ") && line.trim_end().ends_with(" finished."))
}

/// What `systemctl` said about its outcome: stderr, where it writes its diagnostics, or stdout when
/// stderr said nothing else — less the [`is_protocol`] lines either way.
fn diagnostic(capture: &Capture) -> String {
    let said = |stream: &[u8]| -> String {
        String::from_utf8_lossy(stream)
            .split_inclusive('\n')
            .filter(|line| !is_protocol(line))
            .collect()
    };
    let stderr = said(&capture.stderr);
    if stderr.is_empty() {
        said(&capture.stdout)
    } else {
        stderr
    }
}

/// The message for a `systemctl` invocation that exited non-zero, built from the whole [`Capture`]
/// rather than from its bytes alone.
///
/// `systemctl` writes nothing but protocol on success, so `complete == false` only ever bites here —
/// on the one path whose entire value is the diagnostic. A truncated read presented as the whole
/// story is how "goetia stopped reading" comes out reading as "systemd said nothing".
///
/// `systemctl_tests.rs` covers every shape a capture arrives in; nothing else can, since reaching
/// this through a real `systemctl` needs one that actually fails.
fn failed(verb: &str, unit: &str, capture: &Capture) -> Error {
    let diagnostic = diagnostic(capture);
    // Ahead of the `complete` branch, not inside the truncated one: both
    // streams can be empty either way, and trailing off after a colon is
    // just as uninformative when the read did finish. Only the *reason* it
    // is empty differs.
    if diagnostic.is_empty() {
        let why = if capture.complete {
            "and wrote no diagnostic"
        } else {
            "and no diagnostic was captured before goetia's budget expired"
        };
        return Error::Other(format!("systemctl {verb} {unit} failed, {why}"));
    }
    if capture.complete {
        return Error::Other(format!("systemctl {verb} {unit} failed: {diagnostic}"));
    }
    Error::Other(format!(
        "systemctl {verb} {unit} failed: {diagnostic} (truncated — goetia's budget expired while \
         reading the diagnostic, so this is a prefix of what systemd wrote)"
    ))
}

/// A `systemctl` that exited `0`: `Ok` only if systemd answered with a job. `systemctl` in a chroot,
/// or under `SYSTEMD_OFFLINE=1`, says "Running in chroot, ignoring command" and exits `0` having
/// asked systemd nothing — an image build's `install --start` is the ordinary case — and reporting
/// that as started or stopped is reporting a state nobody established.
///
/// Reliable on every path: without an announcement the bounded wait reads to EOF before it waits at
/// all, so a job line cannot be missing for want of reading.
fn took(verb: &str, unit: &str, capture: &Capture) -> Result<()> {
    if enqueued(&capture.stderr) {
        return Ok(());
    }
    let diagnostic = diagnostic(capture);
    let why = if diagnostic.is_empty() {
        " and said nothing about why".to_string()
    } else {
        format!(": {diagnostic}")
    };
    Err(Error::Other(format!(
        "systemctl {verb} {unit} exited 0 without enqueuing a job, so systemd did not act on it{why}"
    )))
}

/// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
/// needed. Idempotent: starting an already-active unit is a no-op that still exits 0. An expiry
/// means the job was enqueued and not waited out — see [`run_verb`]. That holds for an active unit
/// too: systemd answers the start with a job, not a state, so a budget shorter than that no-op job's
/// round trip expires where launchd and SCM report started (see `ServiceManager::start`).
///
/// Every generated unit carries `Type=exec` (see `generate::unit`'s doc comment for why), so a
/// bounded `systemctl start` confirms the `exec` itself succeeded — not merely that systemd forked
/// the process. A missing executable or missing user comes back as a failed start.
pub(super) fn start_impl(id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
    let unit = super::unit_name(id);
    match run_verb("start", &unit, budget, deadline)? {
        Finished::Exited { status, capture } if status.success() => took("start", &unit, &capture),
        Finished::Exited { capture, .. } => Err(failed("start", &unit, &capture)),
        Finished::Expired => Err(budget::timed_out(id, "running", budget)),
    }
}

/// Idempotent per `ServiceManager::stop`'s doc comment. Exit code 5 ("unit not loaded") means there
/// was nothing to stop — the same convention `tests/support/service_guard.rs` already uses for
/// cleanup — which is success here, not a failure to stop something that was never running.
pub(super) fn stop_impl(id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
    let unit = super::unit_name(id);
    match run_verb("stop", &unit, budget, deadline)? {
        Finished::Exited { status, capture } if status.success() => took("stop", &unit, &capture),
        Finished::Exited { status, .. } if status.code() == Some(5) => Ok(()),
        Finished::Exited { capture, .. } => Err(failed("stop", &unit, &capture)),
        Finished::Expired => Err(budget::timed_out(id, "stopped", budget)),
    }
}

/// `systemctl restart --no-block`: one job, where a stop and a separate start leave a window in
/// which the start replaces the stop. It stops the unit and then starts it — unless the unit is
/// still activating, when systemd folds the restart into the start job already running: nothing is
/// stopped, and the daemon comes up fresh from that start. [`took`] as for `start`: an exit of `0`
/// with no job is a restart nobody made.
pub(super) fn request_restart_impl(id: &str) -> Result<()> {
    let unit = super::unit_name(id);
    match run_verb("restart", &unit, Budget::Immediate, Budget::Immediate.start())? {
        Finished::Exited { status, capture } if status.success() => took("restart", &unit, &capture),
        Finished::Exited { capture, .. } => Err(failed("restart", &unit, &capture)),
        Finished::Expired => unreachable!("a budget that does not wait never takes the bounded path"),
    }
}

fn show_properties(unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
    let joined = props.join(",");
    let output = run_systemctl(&["show", "--property", &joined, unit])?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "systemctl show {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}

/// `UnitFileState` is the same install-state systemd derives for `systemctl is-enabled` — one query
/// covers state, pid, and boot-enablement together.
pub(super) fn status_from_unit(unit: &str) -> Result<Status> {
    let props = show_properties(unit, &["ActiveState", "MainPID", "UnitFileState"])?;
    let state = match props.get("ActiveState").map(String::as_str) {
        Some("active") => State::Running,
        Some("inactive") => State::Stopped,
        Some("failed") => State::Failed,
        // `activating`/`deactivating` land here, along with anything else
        // systemd reports. One of three places a `State::Starting` would be
        // produced if goetia grows one (routed post-0.1.0) — the other two
        // are `state::classify` in `src/backend/launchd/state.rs` and
        // `map_state` in `src/backend/scm/manager.rs`.
        _ => State::Unknown,
    };
    let pid = props
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0);
    let enabled = props.get("UnitFileState").is_some_and(|s| s == "enabled");
    Ok(Status { state, pid, enabled })
}

#[cfg(test)]
#[path = "systemctl_tests.rs"]
mod systemctl_tests;
