//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back — every one through the manager boundary, [`Systemctl::door`] and [`answered`].

use std::collections::BTreeMap;
use std::io;
use std::process::ExitStatus;

use crate::backend::bounded::{Capture, Finished, Reservation, Role};
use crate::backend::client::{Client, ClientError, Real, Wait};
use crate::backend::systemd::SYSTEMD_FLOOR;
use crate::error::{Error, Result, VersionReading};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, DaemonStatus, State, Step};

use super::{UnitKind, unit_name};

// Held ================================================================================================================

/// What one verb of systemd, or a sequence of them, draws on, made by [`Held::of`] before anything is
/// sent and handed to every [`Systemctl`] call: the threads a watched `systemctl` reads its streams
/// with, and whether the version gate has passed.
///
/// A [`Default`] holds nothing and has not passed the gate: for a call that draws nothing and
/// asks the gate for itself (`disable`, `request_restart`). A read never asks the gate.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(super) draws: Reservation,
    /// The gate passed on this reservation, so its remaining steps do not ask it again: `restart`'s
    /// two legs, `uninstall`'s stop, `disable` and `daemon-reload`, and `install --start`'s install,
    /// `--enable` and start share one pair of probes. A refusal is not kept, since it ends the verb.
    gate_passed: bool,
}

impl Held {
    /// What `steps` draw under `budget`, all or none: see [`listeners`].
    ///
    /// cosca 0.7.0, as measured: every Linux `spawn` runs a helper thread (`cosca-pidfd-handshake`)
    /// that is joined before `spawn` returns. goetia cannot reserve it. A host short by exactly that
    /// thread fails the spawn as [`Error::Spawn`], before the fork, which sent nothing. goetia reads
    /// no other thread of cosca's: the drain pump of a contained child (`cosca-cgrp-pump`) starts
    /// only on a blocking wait for the tree to drain, which is `Child::wait_tree`,
    /// `Child::wait_tree_timeout`, `Child::graceful_shutdown_tree` and a leaf's teardown on drop,
    /// and of those goetia never calls the first three. Re-measure both on a cosca upgrade.
    ///
    /// None of cosca's threads is reserved, and a thread joined, like a listener detached, may still
    /// hold its `TasksMax` slot until the kernel releases it. At a limit the `stop` leg exactly fits,
    /// a waited `restart`'s `start` leg can fail to spawn after the stop was sent. This is not
    /// covered here.
    pub(super) fn of(steps: &[Step], budget: Budget) -> io::Result<Held> {
        let needed: usize = steps.iter().map(|step| listeners(*step, budget)).sum();
        Ok(Held {
            draws: Reservation::of_listeners(needed)?,
            gate_passed: false,
        })
    }
}

/// The listener threads `step` draws under `budget`: [`Role::listeners`] for each watched `systemctl`
/// it runs, which is one for a `start` or `stop` under a budget [`watched`], and none for anything
/// that runs to completion. The count sits beside [`Systemctl::run_verb`], which draws them.
fn listeners(step: Step, budget: Budget) -> usize {
    match step {
        Step::Start | Step::Stop if watched(budget) => Role::AnnouncedRequest(enqueued).listeners(),
        // `uninstall`'s stop is unbounded, and `request_start_after_stop` does not wait.
        Step::Start | Step::Stop | Step::Install | Step::Kill | Step::Uninstall | Step::RequestStartAfterStop => 0,
    }
}

// The manager boundary ================================================================================================

/// What one `systemctl` run is for: what [`Systemctl::door`] establishes before it runs, and what a
/// failure to run it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// One of the version gate's own probes.
    Probe,
    /// A read, `show`, which changes nothing.
    Read,
    /// A request, which may change something: `daemon-reload`, `enable`, `disable`, `start`, `stop`,
    /// `restart`.
    Request,
}

/// `systemctl`, reached through a [`Client`], on the system `host` reads. Every run goes through
/// [`Systemctl::door`] and [`answered`].
pub(crate) struct Systemctl {
    client: Box<dyn Client>,
    /// Read at every [`Systemctl::door`], never kept: the evidence is the system as it is at the run.
    host: fn() -> Host,
}

impl Systemctl {
    pub(crate) fn new(client: Box<dyn Client>, host: fn() -> Host) -> Systemctl {
        Systemctl { client, host }
    }

    /// The real `systemctl`, on this system.
    pub(super) fn native() -> Systemctl {
        Systemctl::new(
            Box::new(Real::new(vec!["systemctl".to_string()], environment)),
            Host::this,
        )
    }

    /// Where every `systemctl` goetia runs is refused before it is spawned: on evidence, known
    /// without asking, that no manager can be asked here ([`Host::evidence`]); and for a request,
    /// until the manager has passed the version gate. The gate's `show` is what finds out, before
    /// anything acts, a chroot goetia could not see for itself: `enable` and `disable` act on unit
    /// files offline and say nothing about it. [`answered`] is the other half.
    fn door(&self, purpose: Purpose, held: &mut Held) -> Result<()> {
        refuse_unreachable(self.host)?;
        if purpose == Purpose::Request {
            self.require_supported(held)?;
        }
        Ok(())
    }

    /// `systemctl <args>` for `purpose`, through [`Systemctl::door`], waited on as `wait`, and
    /// [`answered`]. What the run draws comes from `held`. A failure to run it is [`unran`].
    fn run(&self, purpose: Purpose, args: &[&str], wait: Wait, held: &mut Held) -> Result<Finished> {
        self.door(purpose, held)?;
        let finished = self
            .client
            .run(args, wait, &mut held.draws)
            .map_err(|error| unran(purpose, args, error))?;
        if let Finished::Exited { capture, .. } = &finished {
            answered(&capture.stdout, &capture.stderr)?;
        }
        Ok(finished)
    }

    /// [`Systemctl::run`] to completion, which never expires and draws nothing from `held`.
    fn completed(&self, purpose: Purpose, args: &[&str], held: &mut Held) -> Result<(ExitStatus, Capture)> {
        match self.run(purpose, args, Wait::ToCompletion, held)? {
            Finished::Exited { status, capture } => Ok((status, capture)),
            Finished::Expired | Finished::Left(_) => unreachable!("a run to completion does not expire"),
        }
    }
}

/// The refusal where `host` shows, without asking, that no manager can be asked: the first half of
/// [`Systemctl::door`], and the whole of `busctl`'s.
pub(super) fn refuse_unreachable(host: fn() -> Host) -> Result<()> {
    match host().evidence() {
        Some(evidence) => Err(no_manager(evidence)),
        None => Ok(()),
    }
}

/// What a `systemctl` that failed to complete says, by what it was for. A request's two ways to
/// fail are kept apart: one never run was never sent, and one that may have run, or was lost after
/// it did, may have reached the manager ([`Error::RequestInDoubt`], with `reached` as far as the
/// client announced it). Anything else that fails to run is a plain failure, since it changes
/// nothing.
fn unran(purpose: Purpose, args: &[&str], error: ClientError) -> Error {
    let request = format!("systemctl {}", args.join(" "));
    let (source, may_have_run, reached) = match error {
        ClientError::NotRun(source) => (source, false, false),
        ClientError::MayHaveRun(source) => (source, true, false),
        ClientError::Lost { error, announced } => (error, true, announced),
    };
    if purpose == Purpose::Request && may_have_run {
        Error::RequestInDoubt {
            request,
            reached,
            note: None,
            source: source.into(),
        }
    } else {
        Error::spawn(request, source)
    }
}

/// What `systemctl` says when it asked no manager and did nothing, exiting `0`: its chroot report,
/// worded by version. Matched as a substring of a line.
const IGNORED: [&str; 2] = ["ignoring command", "ignoring request"];

/// Every `systemctl` answer, checked before anything reads it as one: an answer saying it ignored
/// what it was asked ([`IGNORED`]) is the refusal on every path, and never a success. The backstop
/// for what [`Systemctl::door`] cannot see.
///
/// It is the last line in a chroot with no `/proc` at all: goetia establishes nothing there, while
/// `systemctl` reports the chroot from that same absence. Nothing in the environment can put the
/// report there instead, since [`DENIED`] takes every switch that would declare a chroot off the
/// child, so what reaches this is always the child's own detection. Where `/proc` hides PID 1
/// rather than lacking it, `systemctl` is as blind as goetia and reports nothing.
fn answered(stdout: &[u8], stderr: &[u8]) -> Result<()> {
    let ignored = |stream: &[u8]| {
        String::from_utf8_lossy(stream)
            .lines()
            .find(|line| IGNORED.iter().any(|report| line.contains(report)))
            .map(|line| line.trim().to_string())
    };
    match ignored(stderr).or_else(|| ignored(stdout)) {
        Some(line) => Err(no_manager(Evidence::Ignored(line))),
        None => Ok(()),
    }
}

/// The refusal, naming `evidence`: one message, whichever showed it.
fn no_manager(evidence: Evidence) -> Error {
    Error::NoManager {
        evidence: evidence.to_string(),
    }
}

/// What every `systemctl` goetia runs is given: colour off, and a log level and target that keep the
/// `log_info` lines goetia reads audible, the job `--show-transaction` announces ([`enqueued`]) and
/// the report that the request was ignored ([`IGNORED`]).
const ENVIRONMENT: [(&str, &str); 3] = [
    ("SYSTEMD_COLORS", "0"),
    ("SYSTEMD_LOG_LEVEL", "info"),
    ("SYSTEMD_LOG_TARGET", "console"),
];

/// The two prefixes systemd's own switches for this binary carry. An inherited name under either is
/// removed from every `systemctl` goetia runs, whatever it is, and [`ENVIRONMENT`] alone is given.
///
/// Three of them turn off the chroot report [`answered`] is the last line
/// against, and `SYSTEMCTL_FORCE_BUS` points `systemctl` at another bus. Removed rather than set,
/// because every value asserts something goetia is in no position to assert, such as whether this
/// is a chroot: detecting that is the child's job.
///
/// Not reached: `DBUS_*`, which a non-root `systemctl` honours with no switch at all, and variables under neither prefix (`PAGER`, `LESS`,
/// `TERM`, the locale), none of which changes the report.
const DENIED: [&str; 2] = ["SYSTEMD_", "SYSTEMCTL_"];

/// The inherited names that are removed from a `systemctl`'s environment: every [`DENIED`] one that
/// [`ENVIRONMENT`] does not set. The two are kept disjoint here — a name goetia sets is never also
/// removed — rather than by the order the calls happen to be made in, so `Command`'s own rule for a
/// key both set and removed cannot decide what the child gets.
fn removals(inherited: impl Iterator<Item = String>) -> Vec<String> {
    let given = |key: &str| ENVIRONMENT.iter().any(|(name, _)| *name == key);
    let denied = |key: &str| DENIED.iter().any(|prefix| key.starts_with(prefix));
    inherited.filter(|key| denied(key) && !given(key)).collect()
}

/// `cmd`'s environment as every `systemctl` goetia runs gets it: every inherited [`DENIED`] name
/// [removed](removals), and [`ENVIRONMENT`] set. The names are those this process would pass a
/// child of its own; one that is not UTF-8 is skipped, because `getenv` finds a switch by its exact
/// ASCII name, so a name that does not decode is not one of them.
pub(super) fn environment(cmd: &mut cosca::Command) {
    for key in removals(std::env::vars_os().filter_map(|(key, _)| key.into_string().ok())) {
        cmd.env_remove(key);
    }
    for (key, value) in ENVIRONMENT {
        cmd.env(key, value);
    }
}

/// The positive evidence that no systemd manager can be asked here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    /// `SYSTEMD_OFFLINE` is set true, with this value: systemd is told to ask no manager here, and
    /// goetia takes the caller at their word rather than spawning one to find out.
    Offline(String),
    /// `/run/systemd/system` does not exist, which systemd makes when it boots a system
    /// (`sd_booted()`): not booted with systemd, or a chroot with no `/run` of the host's.
    NotBooted,
    /// `/` is established to be a root of goetia's own rather than this system's, found as this
    /// says: either it is not PID 1's root — a chroot (`running_in_chroot()`), or a container
    /// sharing the host's PID namespace — or it is no mount at all, which only `chroot(2)` leaves,
    /// and which PID 1's table has no say in.
    Chroot(Chroot),
    /// `systemctl` said, in this line, that it ignored what it was asked: its chroot report.
    Ignored(String),
}

impl std::fmt::Display for Evidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Evidence::Offline(value) => write!(f, "`SYSTEMD_OFFLINE={value}` is set"),
            Evidence::NotBooted => write!(
                f,
                "`{SD_BOOTED}`, which systemd makes when it boots a system, does not exist here"
            ),
            Evidence::Chroot(Chroot::Root) => write!(f, "`/` is not PID 1's root (`{INIT_ROOT}`){ANOTHER_ROOT}"),
            Evidence::Chroot(Chroot::Mount) => write!(
                f,
                "the mount at `/` is none of PID 1's (`{OWN_MOUNTS}`, `{INIT_MOUNTS}`){ANOTHER_ROOT}"
            ),
            Evidence::Chroot(Chroot::NoMount) => write!(
                f,
                "no mount is at `/` (`{OWN_MOUNTS}`): `/` is a directory inside one, so goetia runs in a chroot"
            ),
            Evidence::Ignored(line) => write!(f, "`systemctl` said {line:?}"),
        }
    }
}

/// What `/` not being PID 1's root means, as far as goetia can tell.
const ANOTHER_ROOT: &str =
    ", so goetia runs under another root than PID 1, as in a chroot or a container sharing the host's PID namespace";

/// Where `sd_booted()` looks: it exists iff systemd is this system's init.
const SD_BOOTED: &str = "/run/systemd/system";

/// PID 1's root, which `running_in_chroot()` compares `/` with.
const INIT_ROOT: &str = "/proc/1/root";

/// goetia's mount table, and PID 1's: each lists the mounts its process sees, `/` included.
const OWN_MOUNTS: &str = "/proc/self/mountinfo";
const INIT_MOUNTS: &str = "/proc/1/mountinfo";

/// How goetia established that its `/` is a root of its own rather than this system's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chroot {
    /// `/` and [`INIT_ROOT`] are different directories — see [`chroot_from`]. Readable only
    /// to a caller that may trace PID 1.
    Root,
    /// The mount at `/` is none of PID 1's — see [`mounts_differ`]. Readable unelevated.
    Mount,
    /// No mount is at `/`: `/` is a directory inside one, which only `chroot(2)` makes a root — see
    /// [`mounts_differ`]. Readable unelevated, and without PID 1's table.
    NoMount,
}

/// What goetia reads of this system without asking `systemctl`: a seam, so a test can stand any
/// system in.
#[derive(Debug, Clone)]
pub(crate) struct Host {
    /// `SYSTEMD_OFFLINE`, as goetia itself inherits it. No `systemctl` goetia runs is given it —
    /// [`DENIED`] takes it away — so this reading is the only one it has.
    offline: Option<String>,
    /// [`SD_BOOTED`] is established absent — see [`established_absent`].
    unbooted: bool,
    /// `/` is established to be a root of goetia's own rather than this system's, and how.
    chroot: Option<Chroot>,
}

impl Host {
    pub(super) fn this() -> Host {
        Host {
            offline: std::env::var("SYSTEMD_OFFLINE").ok(),
            unbooted: established_absent(std::path::Path::new(SD_BOOTED)),
            chroot: chroot_from(identity("/"), identity(INIT_ROOT), || {
                (
                    std::fs::read_to_string(OWN_MOUNTS),
                    std::fs::read_to_string(INIT_MOUNTS),
                )
            }),
        }
    }

    /// Why no manager can be asked here, from what is known without asking `systemctl`: the most
    /// specific cause first. A chroot with no `/run` bound in lacks [`SD_BOOTED`] too.
    fn evidence(&self) -> Option<Evidence> {
        if let Some(value) = self.offline.as_deref().filter(|value| is_true(value)) {
            Some(Evidence::Offline(value.to_string()))
        } else if let Some(found) = self.chroot {
            Some(Evidence::Chroot(found))
        } else {
            self.unbooted.then_some(Evidence::NotBooted)
        }
    }
}

/// `path`'s device and inode, followed through symlinks as `running_in_chroot()`'s `inode_same`
/// does: `/proc/1/root` is a link to PID 1's root.
fn identity(path: &str) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).map(|meta| (meta.dev(), meta.ino()))
}

/// How `/` is established to be a root of goetia's own, if it is. `root` and `init_root`, both
/// read, decide, either way: that is `running_in_chroot()`'s own check. A stat that failed —
/// `EACCES` on `/proc/1/root` for a caller that may not trace PID 1, or no `/proc` — establishes
/// nothing, and leaves it to the mount `tables` ([`mounts_differ`]); where they establish nothing
/// either, the other checks and systemctl's own report ([`answered`]) decide.
///
/// Read regardless of what the environment declares about a chroot: this is goetia's own evidence,
/// and a declaration is evidence of nothing. The child is not left to trust one either — [`DENIED`]
/// takes every such switch away — so neither half of the check can be told to look away.
fn chroot_from(
    root: std::io::Result<(u64, u64)>,
    init_root: std::io::Result<(u64, u64)>,
    tables: impl FnOnce() -> (std::io::Result<String>, std::io::Result<String>),
) -> Option<Chroot> {
    match (root, init_root) {
        (Ok(root), Ok(init_root)) => (root != init_root).then_some(Chroot::Root),
        _ => {
            let (own, init) = tables();
            mounts_differ(own, init)
        }
    }
}

/// Whether goetia's mount table, `own`, establishes that its `/` is a root of its own, with PID
/// 1's, `init`, where that is needed. Readable where `/proc/1/root` is not, and asked only there:
/// an unelevated `systemctl` in a chroot with `/run` bound in cannot tell it is in one, and asks
/// the host's manager about a unit it does not have. Two ways:
///
/// - `own` read, with no mount at `/`: `/` is a directory inside a mount, which only `chroot(2)`
///   makes a root — `pivot_root(2)` and a new mount namespace take a mount's. That needs no `init`,
///   so it holds where a `hidepid` `/proc` hides PID 1.
/// - `own` read with mounts at `/`, and `init` read with one there too, but none of ours is PID 1's
///   — the same mount being its device and its root within that device. A private mount namespace
///   whose `/` is PID 1's filesystem at the same root — a service's `ProtectSystem=` — lists the
///   same mount.
///
/// A table that could not be read, or a PID 1 with no mount at `/`, establishes nothing.
fn mounts_differ(own: std::io::Result<String>, init: std::io::Result<String>) -> Option<Chroot> {
    let own = own.ok()?;
    let own = root_mounts(&own);
    if own.is_empty() {
        return Some(Chroot::NoMount);
    }
    let init = init.ok()?;
    let init = root_mounts(&init);
    (!init.is_empty() && !own.iter().any(|mount| init.contains(mount))).then_some(Chroot::Mount)
}

/// The mounts a mount table lists at `/`: each one's device, `major:minor`, and its root within
/// that device — fields 3, 4 and 5 of `proc_pid_mountinfo(5)`, the fifth being the mount point.
fn root_mounts(table: &str) -> Vec<(&str, &str)> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ').skip(2);
            let (device, root, point) = (fields.next()?, fields.next()?, fields.next()?);
            (point == "/").then_some((device, root))
        })
        .collect()
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

/// A true value as systemd's `parse_boolean` reads one. That is the half of `SYSTEMD_OFFLINE`
/// goetia can read as evidence: systemd is being told to ask no manager, which is what
/// [`Evidence::Offline`] says. A *false* value is not the opposite evidence — `systemctl` acts on
/// it, returning it from `running_in_chroot_or_offline()` before the chroot check runs — and
/// goetia neither believes it nor passes it on ([`DENIED`]).
fn is_true(value: &str) -> bool {
    value == "1"
        || ["yes", "y", "true", "t", "on"]
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
}

// Requests ============================================================================================================

impl Systemctl {
    pub(super) fn daemon_reload(&self, held: &mut Held) -> Result<()> {
        let (status, capture) = self.completed(Purpose::Request, &["daemon-reload"], held)?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::command("systemctl daemon-reload", status, &capture.stderr))
        }
    }

    /// `daemon-reload` after a unit file write that itself succeeded. A failure here must not be reported
    /// as a successful install — the on-disk artifact and systemd's loaded view of it may have diverged
    /// — and says the unit was written. A reload in doubt stays in doubt, exit `4`; any other failure
    /// is exit `1`.
    pub(super) fn daemon_reload_or_report(&self, id: &str, held: &mut Held) -> Result<()> {
        self.daemon_reload(held).map_err(|e| match e {
            e @ Error::RequestInDoubt { .. } => {
                e.in_doubt_context(format!("wrote the unit for `{id}`, so systemd may not have picked it up yet"))
            }
            e => Error::Step {
                what: format!(
                    "wrote the unit for `{id}` but `systemctl daemon-reload` failed, so systemd may not have picked it up yet"
                ),
                source: Box::new(e),
            },
        })
    }
}

// The version gate ====================================================================================================

/// Refuse a systemd older than [`SYSTEMD_FLOOR`]. Asked before goetia writes a unit, and by
/// [`Systemctl::door`] before every request: an older systemd does not reject `Type=exec` but logs that it
/// cannot parse it and runs the unit as `Type=simple`, silently losing the failed-exec detection
/// the directive is there for. Asked once per [`Held`]: one pass is remembered there, so the steps of
/// one sequence share one pair of probes, and a manager that changes between two invocations is asked
/// again.
impl Systemctl {
    pub(super) fn require_supported(&self, held: &mut Held) -> Result<()> {
        if held.gate_passed {
            return Ok(());
        }
        self.supported(held)?;
        held.gate_passed = true;
        Ok(())
    }
}

/// Whose version [`Systemctl::supported`] judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The running manager's `Version` property. PID 1 is what parses `Type=exec` and answers
    /// `--show-transaction`'s `EnqueueUnitJob`.
    Manager,
    /// `systemctl --version`: the client is what parses the `--show-transaction` option, and a
    /// newer manager does not rescue an older client.
    Client,
}

impl Systemctl {
    /// The gate itself: both the running manager and the client must be new enough. Where no
    /// manager can be asked there is no gate to pass — the probes' own [`Systemctl::door`] and
    /// [`answered`] refuse there — and any other failure to read the manager's version is a
    /// refusal too: a manager that is running and could not be asked is not one goetia can vouch
    /// for.
    fn supported(&self, held: &mut Held) -> Result<()> {
        let (status, capture) = self.completed(Purpose::Probe, &["--version"], held)?;
        if !status.success() {
            return Err(Error::Step {
                what: format!("goetia cannot tell whether this is systemd {SYSTEMD_FLOOR}+"),
                source: Box::new(Error::command("systemctl --version", status, &capture.stderr)),
            });
        }
        let client = String::from_utf8_lossy(&capture.stdout);
        let client = client.lines().next().unwrap_or_default();
        let (status, manager) = self.completed(Purpose::Probe, &["show", "--property=Version", "--value"], held)?;
        if !status.success() {
            return Err(Error::Step {
                what: format!("goetia cannot tell whether the running systemd is {SYSTEMD_FLOOR}+"),
                source: Box::new(Error::command(
                    "systemctl show --property=Version --value",
                    status,
                    &manager.stderr,
                )),
            });
        }
        let reported = String::from_utf8_lossy(&manager.stdout);
        refusal(
            reading(Source::Client, client),
            reading(Source::Manager, reported.trim()),
        )
    }
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
        Source::Client => reported.strip_prefix("systemd ")?,
    };
    let digits: String = version.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// What `source` reported, as [`Error::UnsupportedManager`] keeps it.
fn reading(source: Source, reported: &str) -> VersionReading {
    match major(source, reported) {
        Some(n) => VersionReading::Major(n),
        None => VersionReading::Unreadable(reported.to_string()),
    }
}

/// `Ok` when both are at the floor; otherwise the refusal.
fn refusal(client: VersionReading, manager: VersionReading) -> Result<()> {
    let at_floor = |reading: &VersionReading| matches!(reading, VersionReading::Major(n) if *n >= SYSTEMD_FLOOR);
    if at_floor(&client) && at_floor(&manager) {
        return Ok(());
    }
    Err(Error::UnsupportedManager { client, manager })
}

// Verbs ===============================================================================================================

/// The argv for one `systemctl <verb> <unit>` under `budget`.
///
/// `--show-transaction` on every path: its announcement ([`enqueued`]) is the only evidence that
/// systemd took the request — a `systemctl` that exits `0` having done nothing says so only when it
/// knows it ([`answered`]) — and on the bounded path it is also what the wait holds out for. A
/// budget that does not wait adds `--no-block`, which still announces the job.
///
/// Its own function so that each path's flags are observable without a timing bet: no elevated test
/// can see `--no-block` (`start` returns either way). `systemctl_tests.rs` asserts both argvs.
fn verb_args<'a>(verb: &'a str, unit: &'a str, budget: Budget) -> Vec<&'a str> {
    if budget.waits() {
        unit_args(&[verb, "--show-transaction"], unit)
    } else {
        unit_args(&[verb, "--no-block", "--show-transaction"], unit)
    }
}

/// `head`, then `--`, then `unit`. An id may start with `-`, and `systemctl` reads such a unit name
/// as an option unless `--` precedes it. Every `systemctl` call that names a unit builds its argv
/// with this.
pub(super) fn unit_args<'a>(head: &[&'a str], unit: &'a str) -> Vec<&'a str> {
    head.iter().copied().chain(["--", unit]).collect()
}

/// Whether `systemctl --show-transaction`'s stderr says systemd has answered the request with a
/// job — the line it writes after `StartUnit`/`StopUnit` returns and before it waits for the job.
/// A substring of the stream, not a whole line: the job number and unit follow it, and were a
/// prefix ever to precede it the line would still be found. [`DENIED`] removes the switches that
/// add one, so no `systemctl` goetia runs can be given one; tolerating it anyway costs nothing.
fn enqueued(stderr: &[u8]) -> bool {
    const LINE: &[u8] = b"Enqueued anchor job ";
    stderr.windows(LINE.len()).any(|w| w == LINE)
}

/// The number of the job `stderr` announces: `Enqueued anchor job <N> <unit>/<verb>.`.
fn job_number(stderr: &[u8]) -> Option<u64> {
    const LINE: &[u8] = b"Enqueued anchor job ";
    let at = stderr.windows(LINE.len()).position(|w| w == LINE)? + LINE.len();
    let digits: Vec<u8> = stderr[at..].iter().copied().take_while(u8::is_ascii_digit).collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// Whether [`Systemctl::run_verb`] takes its bounded path under `budget` — the one whose `systemctl`
/// goetia watches, and which draws two listener threads from its [`Held`], one per stream
/// ([`Role::AnnouncedRequest`]).
pub(super) fn watched(budget: Budget) -> bool {
    budget.waits() && budget != Budget::Unbounded
}

impl Systemctl {
    /// One `systemctl <verb> <unit>` under `budget`.
    ///
    /// A budget that does not wait becomes `systemctl <verb> --no-block`, the exact native
    /// expression of "issue the request and return": systemd enqueues the job and `systemctl` exits
    /// without waiting for it to complete. `Budget::Unbounded` is the plain blocking call. Both run
    /// to completion ([`Wait::ToCompletion`]), because neither has anything to bound: a bound is
    /// required only on the second path.
    ///
    /// That path waits on `deadline`, which the caller derived at verb entry — but the deadline
    /// bounds only the wait for the job, never whether it is enqueued: until `systemctl` announces
    /// the job ([`enqueued`]) the wait is unbounded, so a budget a few milliseconds long still
    /// reaches systemd, and an expiry is then exactly what `budget::timed_out` says it is. `budget`
    /// only picks the path and the wording.
    fn run_verb(
        &self,
        verb: &str,
        unit: &str,
        budget: Budget,
        deadline: Deadline,
        held: &mut Held,
    ) -> Result<Finished> {
        let wait = if watched(budget) {
            Wait::Bounded {
                role: Role::AnnouncedRequest(enqueued),
                deadline,
            }
        } else {
            Wait::ToCompletion
        };
        self.run(Purpose::Request, &verb_args(verb, unit, budget), wait, held)
    }
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
fn failed(verb: &str, unit: &str, status: ExitStatus, capture: &Capture) -> Error {
    let diagnostic = diagnostic(capture);
    // Ahead of the `complete` branch, not inside the truncated one: both
    // streams can be empty either way, and trailing off after a colon is
    // just as uninformative when the read did finish. Only the *reason* it
    // is empty differs.
    let stderr = match (diagnostic.is_empty(), capture.complete) {
        (true, true) => String::new(),
        (true, false) => "no diagnostic was captured before goetia's budget expired".to_string(),
        (false, true) => diagnostic,
        (false, false) => format!(
            "{diagnostic} (goetia's budget expired before it read the diagnostic to its end, so this may be only a \
             prefix of what systemd wrote)"
        ),
    };
    Error::command(format!("systemctl {verb} {unit}"), status, stderr.as_bytes())
}

/// A `systemctl` that exited `0`: `Ok` only if systemd answered with a job. One that said it asked
/// nobody never gets here ([`answered`]); any other exit of `0` with no job is a request nothing
/// took, and reporting it as started or stopped is reporting a state nobody established.
///
/// Reliable on every path: without an announcement the bounded wait reads to EOF before it waits at
/// all, so a job line cannot be missing for want of reading.
fn took(verb: &str, unit: &str, capture: &Capture) -> Result<()> {
    if enqueued(&capture.stderr) {
        return Ok(());
    }
    let diagnostic = diagnostic(capture);
    Err(Error::NotEnqueued {
        request: format!("systemctl {verb} {unit}"),
        diagnostic: (!diagnostic.is_empty()).then_some(diagnostic),
    })
}

impl Systemctl {
    /// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
    /// needed. Idempotent: starting an already-active unit is a no-op that still exits 0. An expiry
    /// means the job was enqueued and not waited out — see [`Systemctl::run_verb`]. That holds for an active unit
    /// too: systemd answers the start with a job, not a state, so a budget shorter than that no-op job's
    /// round trip expires where launchd and SCM report started (see `ServiceManager::start`).
    ///
    /// Every generated service carries `Type=exec` (see `generate::units`'s doc comment for why), so a
    /// bounded `systemctl start` confirms the `exec` itself succeeded — not merely that systemd forked
    /// the process. A missing executable or missing user comes back as a failed start.
    pub(super) fn start_impl(&self, id: &str, budget: Budget, deadline: Deadline, held: &mut Held) -> Result<()> {
        let unit = unit_name(id, UnitKind::Service);
        match self.run_verb("start", &unit, budget, deadline, held)? {
            Finished::Exited { status, capture } if status.success() => took("start", &unit, &capture),
            Finished::Exited { status, capture } => Err(failed("start", &unit, status, &capture)),
            Finished::Expired | Finished::Left(_) => Err(budget::timed_out(id, "running", budget)),
        }
    }

    /// A task's start: `systemctl start --no-block`, which returns once the manager has enqueued the
    /// job, and the number of that job. It never waits for the run, under any budget: the run is over
    /// when its program is, which is not when a start returns.
    pub(super) fn start_job(&self, id: &str, held: &mut Held) -> Result<u64> {
        let unit = unit_name(id, UnitKind::Service);
        match self.run_verb("start", &unit, Budget::Immediate, Budget::Immediate.start(), held)? {
            Finished::Exited { status, capture } if status.success() => {
                took("start", &unit, &capture)?;
                job_number(&capture.stderr).ok_or_else(|| Error::NotEnqueued {
                    request: format!("systemctl start {unit}"),
                    diagnostic: Some("it announced a job with no number".to_string()),
                })
            }
            Finished::Exited { status, capture } => Err(failed("start", &unit, status, &capture)),
            Finished::Expired | Finished::Left(_) => {
                unreachable!("a budget that does not wait never takes the bounded path")
            }
        }
    }

    /// Idempotent per `ServiceManager::stop`'s doc comment. Exit code 5 ("unit not loaded") means there
    /// was nothing to stop, which is success here, not a failure to stop something that was never running.
    pub(super) fn stop_impl(&self, id: &str, budget: Budget, deadline: Deadline, held: &mut Held) -> Result<()> {
        let unit = unit_name(id, UnitKind::Service);
        match self.run_verb("stop", &unit, budget, deadline, held)? {
            Finished::Exited { status, capture } if status.success() => took("stop", &unit, &capture),
            Finished::Exited { status, .. } if status.code() == Some(5) => Ok(()),
            Finished::Exited { status, capture } => Err(failed("stop", &unit, status, &capture)),
            Finished::Expired | Finished::Left(_) => Err(budget::timed_out(id, "stopped", budget)),
        }
    }
}

/// `systemctl kill` of `unit`'s main process with `SIGKILL`, and nothing else of it: not the control
/// process, not the rest of the cgroup, which systemd cleans up when the main process is gone.
///
/// `--kill-who`, not `--kill-whom`: 245 and older do not know the second. `main-fail` rather than
/// `main`: `main` exits `0` having signalled nobody when there is no main process, where `main-fail`
/// fails with the answer [`no_process`] reads.
const KILL_ARGS: [&str; 3] = ["kill", "--kill-who=main-fail", "--signal=SIGKILL"];

/// Whether `stderr` is systemd saying `unit` has no process to signal, word for word: it is not
/// loaded, which is what an inactive unit is once nothing holds it, or it is loaded and its main
/// process is gone. Read off the kill's own answer, so no state read before it can race the kill.
fn no_process(stderr: &str, unit: &str) -> bool {
    stderr.contains(&format!("Unit {unit} not loaded.")) || stderr.contains("No main process to kill")
}

impl Systemctl {
    /// `Ok` means the signal was delivered: systemd sends it before it answers, and goetia does not wait
    /// for the process to be gone.
    pub(super) fn kill_impl(&self, id: &str, held: &mut Held) -> Result<()> {
        let unit = unit_name(id, UnitKind::Service);
        let (status, capture) = self.completed(Purpose::Request, &unit_args(&KILL_ARGS, &unit), held)?;
        if status.success() {
            return Ok(());
        }
        if no_process(&String::from_utf8_lossy(&capture.stderr), &unit) {
            return Err(Error::NotRunning { id: id.to_string() });
        }
        Err(failed("kill", &unit, status, &capture))
    }

    /// `systemctl enable` for `id`'s unit; a failure carries systemd's words.
    pub(super) fn enable_impl(&self, id: &str, held: &mut Held) -> Result<()> {
        self.unit_file_verb(&["enable"], id, UnitKind::Service, held)
    }

    /// `systemctl disable` for `id`'s unit, as [`Systemctl::enable_impl`]: `Systemd::disable` and `uninstall` both.
    pub(super) fn disable_impl(&self, id: &str, held: &mut Held) -> Result<()> {
        self.unit_file_verb(&["disable"], id, UnitKind::Service, held)
    }

    /// `systemctl enable --now` for `id`'s timer: the schedule is armed at boot and now. The task's
    /// own unit is never enabled: it has no `[Install]`, and boot does not start it.
    pub(super) fn enable_timer(&self, id: &str, held: &mut Held) -> Result<()> {
        self.unit_file_verb(&["enable", "--now"], id, UnitKind::Timer, held)
    }

    /// `systemctl disable --now` for `id`'s timer: the schedule is disarmed at boot and now. A run in
    /// flight is the task's unit's, not the timer's, and goes on.
    pub(super) fn disable_timer(&self, id: &str, held: &mut Held) -> Result<()> {
        self.unit_file_verb(&["disable", "--now"], id, UnitKind::Timer, held)
    }

    fn unit_file_verb(&self, head: &[&str], id: &str, kind: UnitKind, held: &mut Held) -> Result<()> {
        let unit = unit_name(id, kind);
        let (status, capture) = self.completed(Purpose::Request, &unit_args(head, &unit), held)?;
        if status.success() {
            return Ok(());
        }
        Err(Error::command(
            format!("systemctl {} {unit}", head.join(" ")),
            status,
            &capture.stderr,
        ))
    }

    /// `systemctl restart --no-block`: one job, where a stop and a separate start leave a window in
    /// which the start replaces the stop. It stops the unit and then starts it — unless the unit is
    /// still activating, when systemd folds the restart into the start job already running: nothing is
    /// stopped, and the daemon comes up fresh from that start. [`took`] as for `start`: an exit of `0`
    /// with no job is a restart nobody made.
    pub(super) fn request_restart_impl(&self, id: &str, held: &mut Held) -> Result<()> {
        let unit = unit_name(id, UnitKind::Service);
        match self.run_verb("restart", &unit, Budget::Immediate, Budget::Immediate.start(), held)? {
            Finished::Exited { status, capture } if status.success() => took("restart", &unit, &capture),
            Finished::Exited { status, capture } => Err(failed("restart", &unit, status, &capture)),
            Finished::Expired | Finished::Left(_) => {
                unreachable!("a budget that does not wait never takes the bounded path")
            }
        }
    }
}

impl Systemctl {
    /// `props` of `unit`, every one of them: systemd answers each property `status_from_unit` asks for
    /// on any unit name, loaded or not (an empty `UnitFileState=` included), so an answer without one
    /// is no answer, and never the state its absence would default to.
    fn show_properties(&self, unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
        let joined = props.join(",");
        let (status, capture) = self.completed(
            Purpose::Read,
            &unit_args(&["show", "--property", &joined], unit),
            &mut Held::default(),
        )?;
        if !status.success() {
            return Err(Error::command(
                format!("systemctl show {unit}"),
                status,
                &capture.stderr,
            ));
        }
        let stdout = String::from_utf8_lossy(&capture.stdout);
        let mut map = BTreeMap::new();
        for line in stdout.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        let missing: Vec<&str> = props.iter().copied().filter(|prop| !map.contains_key(*prop)).collect();
        if !missing.is_empty() {
            return Err(Error::IncompleteAnswer {
                command: format!("systemctl show {unit}"),
                missing: missing.into_iter().map(str::to_string).collect(),
            });
        }
        Ok(map)
    }

    /// `props` of `unit`, which goetia finds installed, and `LoadState` with them.
    ///
    /// Asked only about a unit goetia finds installed, so `LoadState=not-found` — systemd has no unit
    /// file for the name — is a unit file systemd cannot see, one goetia's own mount namespace holds,
    /// say. Not one merely written and not yet reloaded: the manager picks a unit file up on demand.
    /// What systemd reports for such a name is not that unit's state, and never read as one.
    fn show_installed(&self, unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
        let mut asked = props.to_vec();
        asked.push("LoadState");
        let shown = self.show_properties(unit, &asked)?;
        if shown.get("LoadState").map(String::as_str) == Some("not-found") {
            return Err(Error::UnitNotLoaded { unit: unit.to_string() });
        }
        Ok(shown)
    }

    /// `UnitFileState` is the same install-state systemd derives for `systemctl is-enabled` — one query
    /// covers state, pid, and boot-enablement together.
    pub(super) fn status_from_unit(&self, unit: &str) -> Result<DaemonStatus> {
        let props = self.show_installed(unit, &["ActiveState", "MainPID", "UnitFileState"])?;
        let state = match props.get("ActiveState").map(String::as_str) {
            Some("active") => State::Running,
            Some("inactive") => State::Stopped,
            Some("failed") => State::Failed,
            // `activating`/`deactivating` land here, along with anything else systemd reports.
            _ => State::Unknown,
        };
        let enabled = props.get("UnitFileState").is_some_and(|s| s == "enabled");
        Ok(DaemonStatus {
            state,
            pid: main_pid(&props),
            enabled,
        })
    }

    /// A task's state, pid and invocation, from its job `unit`, in one `systemctl show`. The run in
    /// flight is `Running` whichever of `activating`, `active` and `deactivating` it is in; between runs
    /// the job is `inactive`, or `failed` after a run that failed, which is a result and not a state.
    /// The invocation is the run's, which the manager clears once the unit has gone inactive. Whether
    /// the task is enabled is the timer's to say: see [`Systemctl::timer_enabled`].
    pub(super) fn task_state(&self, unit: &str) -> Result<TaskJob> {
        let props = self.show_installed(unit, &["ActiveState", "MainPID", "InvocationID"])?;
        let state = match props.get("ActiveState").map(String::as_str) {
            Some("active" | "activating" | "deactivating") => State::Running,
            Some("inactive" | "failed") => State::Idle,
            _ => State::Unknown,
        };
        Ok(TaskJob {
            state,
            pid: main_pid(&props),
            invocation: props.get("InvocationID").filter(|id| !id.is_empty()).cloned(),
        })
    }

    /// Whether the manager still holds the job `job`, queued or running: `systemctl list-jobs` lists
    /// every one. A job that was canceled leaves no journal entry, so this is where its end shows.
    pub(super) fn job_exists(&self, job: u64) -> Result<bool> {
        let (status, capture) = self.completed(Purpose::Read, &["list-jobs", "--no-legend"], &mut Held::default())?;
        if !status.success() {
            return Err(Error::command(
                "systemctl list-jobs".to_string(),
                status,
                &capture.stderr,
            ));
        }
        Ok(String::from_utf8_lossy(&capture.stdout)
            .lines()
            .any(|line| line.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) == Some(job)))
    }

    /// Whether `id`'s timer is enabled: its `UnitFileState` is `enabled`.
    pub(super) fn timer_enabled(&self, id: &str) -> Result<bool> {
        let props = self.show_installed(&unit_name(id, UnitKind::Timer), &["UnitFileState"])?;
        Ok(props.get("UnitFileState").is_some_and(|s| s == "enabled"))
    }
}

/// What `systemctl show` says of a task's job: see [`Systemctl::task_state`].
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TaskJob {
    pub state: State,
    pub pid: Option<u32>,
    /// The `InvocationID` of the run the unit is in, if it has one.
    pub invocation: Option<String>,
}

/// `MainPID`, when it names a process.
fn main_pid(props: &BTreeMap<String, String>) -> Option<u32> {
    props
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0)
}

#[cfg(test)]
#[path = "systemctl_tests.rs"]
pub(super) mod systemctl_tests;
