//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back — every one through the manager boundary, [`Systemctl::door`] and [`answered`].

use std::cell::Cell;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::process::ExitStatus;

use crate::backend::bounded::{Capture, Finished, Role};
use crate::backend::client::{Client, ClientError, Real, Wait};
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State, Status};

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
pub(super) struct Systemctl {
    client: Box<dyn Client>,
    /// Read at every [`Systemctl::door`], never kept: the evidence is the system as it is at the run.
    host: Box<dyn Fn() -> Host + Send + Sync>,
}

impl Systemctl {
    pub(super) fn new(client: Box<dyn Client>, host: Box<dyn Fn() -> Host + Send + Sync>) -> Systemctl {
        Systemctl { client, host }
    }

    /// The real `systemctl`, on this system.
    pub(super) fn native() -> Systemctl {
        Systemctl::new(
            Box::new(Real::new(vec!["systemctl".to_string()], environment)),
            Box::new(Host::this),
        )
    }

    /// Where every `systemctl` goetia runs is refused before it is spawned: on evidence, known
    /// without asking, that no manager can be asked here ([`Host::evidence`]); and for a request,
    /// until the manager has passed the version gate. The gate's `show` is what finds out, before
    /// anything acts, a chroot goetia could not see for itself: `enable` and `disable` act on unit
    /// files offline, in a chroot and on a system systemd did not boot, and say nothing about it
    /// (measured on 257). [`answered`] is the other half.
    fn door(&self, purpose: Purpose) -> Result<()> {
        if let Some(evidence) = (self.host)().evidence() {
            return Err(no_manager(evidence));
        }
        if purpose == Purpose::Request {
            self.require_supported()?;
        }
        Ok(())
    }

    /// `systemctl <args>` for `purpose`, through [`Systemctl::door`], waited on as `wait`, and
    /// [`answered`]. A failure to run it is [`unran`].
    fn run(&self, purpose: Purpose, args: &[&str], wait: Wait) -> Result<Finished> {
        self.door(purpose)?;
        let finished = self
            .client
            .run(args, wait)
            .map_err(|error| unran(purpose, args, error))?;
        if let Finished::Exited { capture, .. } = &finished {
            answered(&capture.stdout, &capture.stderr)?;
        }
        Ok(finished)
    }

    /// [`Systemctl::run`] to completion, which never expires.
    fn completed(&self, purpose: Purpose, args: &[&str]) -> Result<(ExitStatus, Capture)> {
        match self.run(purpose, args, Wait::ToCompletion)? {
            Finished::Exited { status, capture } => Ok((status, capture)),
            Finished::Expired => unreachable!("a run to completion does not expire"),
        }
    }
}

/// What a `systemctl` that failed to complete says, by what it was for. A request's two ways to
/// fail are kept apart: one never run was never sent, and one that may have run, or was lost after
/// it did, may have reached the manager ([`Error::RequestInDoubt`], with `reached` as far as the
/// client announced it). Anything else that fails to run is a plain failure, since it changes
/// nothing.
fn unran(purpose: Purpose, args: &[&str], error: ClientError) -> Error {
    let request = format!("systemctl {}", args.join(" "));
    match (purpose, error) {
        (Purpose::Request, ClientError::NotRun(e)) => {
            Error::Other(format!("could not run `{request}`, so it was not sent: {e}"))
        }
        (Purpose::Request, ClientError::MayHaveRun(e)) => Error::RequestInDoubt {
            request,
            reached: false,
            detail: e.to_string(),
        },
        (Purpose::Request, ClientError::Lost { error, announced }) => Error::RequestInDoubt {
            request,
            reached: announced,
            detail: error.to_string(),
        },
        (
            Purpose::Probe | Purpose::Read,
            ClientError::NotRun(e) | ClientError::MayHaveRun(e) | ClientError::Lost { error: e, .. },
        ) => Error::Other(format!("failed to run `{request}`: {e}")),
    }
}

/// What `systemctl` says when it asked no manager and did nothing, exiting `0`: "Running in chroot,
/// ignoring command 'start'" from systemd 246 on, "… ignoring request: start" on 242 to 245, and
/// "… ignoring request." with no verb to name, on every version (systemd's `verbs.c` and
/// `systemctl.c`; 257's binary has both).
const IGNORED: [&str; 2] = ["ignoring command", "ignoring request"];

/// Every `systemctl` answer, checked before anything reads it as one: an answer saying it ignored
/// what it was asked ([`IGNORED`]) is the refusal on every path, and never a success: the backstop
/// for what [`Systemctl::door`] could not see.
///
/// It is the last line against a refusal `systemctl` reaches by its own detection and
/// [`Host::evidence`] does not: a chroot with no `/proc` is no longer one of them, since goetia
/// reads that absence itself ([`Chroot::NoProc`]) and refuses before it spawns. Nothing in the
/// environment can put the report there instead: every switch that would declare a chroot is taken
/// off the child ([`DENIED`]), so what reaches this is always the child's own detection. Where PID 1
/// is hidden from goetia rather than absent, as under a `hidepid` `/proc`, `systemctl` runs with
/// goetia's credentials and hits the same wall: it reports nothing, and this backstop does not fire
/// either.
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

/// What every `systemctl` goetia runs is given. An inherited `SYSTEMD_COLORS` wraps what it prints
/// in escapes, and an inherited `SYSTEMD_LOG_LEVEL=warning` or `SYSTEMD_LOG_TARGET=null` silences
/// the `log_info` lines goetia reads: the job `--show-transaction` announces ([`enqueued`]), and
/// the report that it ignored the request ([`IGNORED`]).
const ENVIRONMENT: [(&str, &str); 3] = [
    ("SYSTEMD_COLORS", "0"),
    ("SYSTEMD_LOG_LEVEL", "info"),
    ("SYSTEMD_LOG_TARGET", "console"),
];

/// The two prefixes systemd's own switches for this binary carry, and which every `systemctl`
/// goetia runs is denied: an inherited name under either is removed from the child whatever it is,
/// and [`ENVIRONMENT`] alone is then given. Named by prefix rather than one switch at a time
/// because the switches are systemd's to add, not goetia's to keep up with. MEASURED, `strings`
/// anchored on each prefix: 255's `systemctl` links no systemd library — it is built against
/// `libsystemd-shared` statically — and carries **44** `SYSTEMD_*` names by itself; 257's carries
/// **90** with `libsystemd-shared-257.so`, which it does link; both carry the same **5**
/// `SYSTEMCTL_*`. *Three* of the `SYSTEMD_*` turn off, each on its own, the chroot report
/// [`answered`] is the last line against. Naming them is what missed the next one twice.
///
/// The three, MEASURED on 255 and 257 in a chroot with no `/proc`, where `systemctl` ran and goetia
/// established nothing of its own (it refuses there now, before a spawn, so these names matter only
/// where `systemctl` still runs and detects a chroot itself): `SYSTEMD_IGNORE_CHROOT` true; `SYSTEMD_IN_CHROOT` false, on 257 on; and
/// `SYSTEMD_OFFLINE` false, on every supported version — `running_in_chroot_or_offline()`
/// (systemd's `src/shared/verbs.c`) consults that one first and returns it whenever it *parses*, so
/// a false value discards the chroot check as surely as `SYSTEMD_IGNORE_CHROOT=1` does. With any of
/// the three inherited, `install` from such a chroot exited `0` and wrote the unit into it.
///
/// Removed rather than set, because every value asserts something goetia is in no position to
/// assert. `SYSTEMD_IN_CHROOT=0` asserts there is no chroot — measured on 257, it silences the
/// report in one — and `=1` asserts there is one everywhere, which on a 257 host that is no chroot
/// makes `systemctl` ignore every command. Detecting it is the child's job; goetia only declines to
/// prejudge it.
///
/// `SYSTEMCTL_*` is systemd's second switch namespace for this binary, and is covered for the same
/// reason rather than for anything it holds today: MEASURED, none of its five silences the chroot
/// report on either version. `SYSTEMCTL_FORCE_BUS` is nonetheless the other shape goetia refuses
/// for — it makes `systemctl` reach the manager over the bus
/// `DBUS_SYSTEM_BUS_ADDRESS` names instead of this root's private socket. MEASURED as root, that
/// address pointed at a path that does not exist: `show` answered `Version=…` without the switch
/// and failed to connect with it.
///
/// The switch decides that only for the root `systemctl` every mutating verb runs. MEASURED at uid
/// 502 on both versions: a non-root `systemctl` honours the address with NO switch set at all, this
/// root's private socket not being its to open, and an unelevated `status` is then answered by
/// whichever manager the address names. [`status_from_unit`]'s `LoadState=not-found` arm makes that
/// exit `4` rather than a state, so it is refused rather than believed — but the address itself is
/// outside this prefix's reach, and whether a read's child should lose `DBUS_*` too is an open
/// question, not a settled one.
///
/// What removing the rest costs: nothing any real environment carries. MEASURED on 255 and 257 — a
/// plain shell, a login shell and `sudo` carry no name under either prefix, and the one systemd
/// itself puts in a process's environment, `SYSTEMD_EXEC_PID`, changes nothing `systemctl` does.
/// Both of its streams are captured, never a terminal, so `SYSTEMD_PAGER`, `SYSTEMD_LESS` and
/// `SYSTEMD_PAGERSECURE` changed nothing either.
///
/// What they do *not* reach is a variable under neither prefix that `systemctl` also reads:
/// `PAGER`, `LESS`, `TERM`, and the locale. MEASURED on 255 and 257 with the report in force,
/// `PAGER=cat`, `LESS=X` and `TERM=xterm-256color` leave it word for word in English, which is what
/// [`IGNORED`] matches on. The locale cannot reach it at all, which is the stronger claim and the
/// one that matters, because a locale a user has generated is not something a sweep can enumerate:
/// the report is not a translatable string. MEASURED — 255 ships no systemd message catalog; 257
/// ships 41, none of which contains `Running in chroot` (its `de` holds 672 strings and its `ja`
/// 300, all polkit action descriptions), and the report itself sits in English in
/// `libsystemd-shared-257.so`.
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
fn environment(cmd: &mut cosca::Command) {
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
    /// and which PID 1's table has no say in — or there is no `/proc` to read any of it from.
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
            Evidence::Chroot(Chroot::NoProc) => write!(
                f,
                "`{PROC_SELF}` does not exist (no `/proc` is mounted), so goetia runs in a chroot"
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

/// Present in every mounted `/proc`, so `ENOENT` on it means none is mounted — an empty `/proc`
/// directory with nothing on it included.
const PROC_SELF: &str = "/proc/self";
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
    /// [`PROC_SELF`] is established absent: no `/proc` is mounted, which is how a chroot without one
    /// looks. Nothing else can then be read, and cosca kills any child whose identity it cannot read
    /// from `/proc`, so no `systemctl` could run to report the chroot itself.
    NoProc,
}

/// What goetia reads of this system without asking `systemctl`: a seam, so a test can stand any
/// system in.
#[derive(Debug, Clone)]
pub(super) struct Host {
    /// `SYSTEMD_OFFLINE`, as goetia itself inherits it. No `systemctl` goetia runs is given it —
    /// [`DENIED`] takes it away — so this reading is the only one it has.
    offline: Option<String>,
    /// [`SD_BOOTED`] is established absent — see [`established_absent`].
    unbooted: bool,
    /// `/` is established to be a root of goetia's own rather than this system's, and how.
    chroot: Option<Chroot>,
}

impl Host {
    fn this() -> Host {
        Host {
            offline: std::env::var("SYSTEMD_OFFLINE").ok(),
            unbooted: established_absent(std::path::Path::new(SD_BOOTED)),
            chroot: chroot_from(
                identity("/"),
                identity(INIT_ROOT),
                established_absent(std::path::Path::new(PROC_SELF)),
                || {
                    (
                        std::fs::read_to_string(OWN_MOUNTS),
                        std::fs::read_to_string(INIT_MOUNTS),
                    )
                },
            ),
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

/// How `/` is established to be a root of goetia's own, if it is. `proc_absent` decides first:
/// with no `/proc` mounted, none of the rest can be read. Then `root` and `init_root`, both read,
/// decide, either way: that is `running_in_chroot()`'s own check. A stat that failed — `EACCES` on
/// `/proc/1/root` for a caller that may not trace PID 1 — establishes nothing, and leaves it to the
/// mount `tables` ([`mounts_differ`]); where they establish nothing either, the other checks and
/// systemctl's own report ([`answered`]) decide.
///
/// Read regardless of what the environment declares about a chroot: this is goetia's own evidence,
/// and a declaration is evidence of nothing. The child is not left to trust one either — [`DENIED`]
/// takes every such switch away — so neither half of the check can be told to look away.
fn chroot_from(
    root: std::io::Result<(u64, u64)>,
    init_root: std::io::Result<(u64, u64)>,
    proc_absent: bool,
    tables: impl FnOnce() -> (std::io::Result<String>, std::io::Result<String>),
) -> Option<Chroot> {
    if proc_absent {
        return Some(Chroot::NoProc);
    }
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
    pub(super) fn daemon_reload(&self) -> Result<()> {
        let (status, capture) = self.completed(Purpose::Request, &["daemon-reload"])?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "systemctl daemon-reload failed: {}",
                String::from_utf8_lossy(&capture.stderr)
            )))
        }
    }

    /// `daemon-reload` after a unit file write that itself succeeded. A failure here must not be
    /// reported as a successful install — the on-disk artifact and systemd's loaded view of it may
    /// have diverged — and says the unit was written. A reload in doubt stays in doubt, exit `4`;
    /// any other failure is exit `1`.
    pub(super) fn daemon_reload_or_report(&self, id: &str) -> Result<()> {
        self.daemon_reload().map_err(|e| match e {
            Error::RequestInDoubt {
                request,
                reached,
                detail,
            } => Error::RequestInDoubt {
                request,
                reached,
                detail: format!("wrote the unit for `{id}`, so systemd may not have picked it up yet: {detail}"),
            },
            e => Error::Other(format!(
                "wrote the unit for `{id}` but `systemctl daemon-reload` failed, so systemd may not have \
                 picked it up yet: {e}"
            )),
        })
    }
}

// The version gate ====================================================================================================

/// The oldest systemd goetia runs against: every start and stop runs `systemctl --show-transaction`
/// (242).
const SYSTEMD_FLOOR: u32 = 242;

/// The oldest systemd that parses the `Type=exec` generated units declare.
const TYPE_EXEC: u32 = 240;

impl Systemctl {
    /// Refuse a systemd older than [`SYSTEMD_FLOOR`]. Asked before goetia writes a unit, and by
    /// [`Systemctl::door`] before every request: an older systemd does not reject `Type=exec` but
    /// logs that it cannot parse it and runs the unit as `Type=simple`, silently losing the
    /// failed-exec detection the directive is there for. Asked once per [`GateScope`].
    pub(super) fn require_supported(&self) -> Result<()> {
        gated(|| self.supported())
    }
}

thread_local! {
    /// How many [`GateScope`]s are open on this thread, and whether the version gate passed inside
    /// them.
    static GATE: Cell<(usize, bool)> = const { Cell::new((0, false)) };
}

/// While one lives on this thread, a version gate that passed there is not asked again: the steps of
/// one verb for one id — `restart`'s two legs, `install --start`'s install and start, `uninstall`'s
/// stop, `disable` and `daemon-reload` — share one answer, and one pair of probes spends one share
/// of the budget. Scopes nest and end in any order; the answer is forgotten when the last one ends,
/// so nothing outlives the invocation that opened them, and a manager that changes between two is
/// asked again.
pub(crate) struct GateScope {
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
    fn supported(&self) -> Result<()> {
        let (status, capture) = self.completed(Purpose::Probe, &["--version"])?;
        if !status.success() {
            return Err(Error::Other(format!(
                "`systemctl --version` failed, so goetia cannot tell whether this is systemd {SYSTEMD_FLOOR}+: {}",
                String::from_utf8_lossy(&capture.stderr)
            )));
        }
        let client = String::from_utf8_lossy(&capture.stdout);
        let client = client.lines().next().unwrap_or_default();
        let (status, capture) = self.completed(Purpose::Probe, &["show", "--property=Version", "--value"])?;
        let reported = String::from_utf8_lossy(&capture.stdout);
        if !status.success() || reported.trim().is_empty() {
            let how = if status.success() {
                "printed no version".to_string()
            } else {
                format!("failed ({status})")
            };
            return Err(Error::Other(format!(
                "`systemctl show --property=Version` {how}, so goetia cannot tell whether the running systemd is \
                 {SYSTEMD_FLOOR}+: {}",
                String::from_utf8_lossy(&capture.stderr)
            )));
        }
        let verdicts = [
            verdict(Source::Manager, reported.trim()),
            verdict(Source::Client, client),
        ];
        refusal(verdicts.into_iter().flatten().collect())
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

/// Why the version `source` reported is refused, as one sentence; `None` if it is not.
fn verdict(source: Source, reported: &str) -> Option<String> {
    const ANSWER: &str = "cannot answer the `systemctl --show-transaction` every start and stop runs";
    const TYPE_SIMPLE: &str = "runs goetia's `Type=exec` units as `Type=simple`, which cannot report a failed exec";
    let Some(n) = major(source, reported) else {
        let asked = match source {
            Source::Manager => "the running systemd reports",
            Source::Client => "`systemctl --version` reports",
        };
        return Some(format!(
            "goetia cannot read the version {asked} ({reported:?}), so cannot tell whether it is."
        ));
    };
    if n >= SYSTEMD_FLOOR {
        return None;
    }
    Some(match source {
        Source::Client => format!(
            "The `systemctl` client is {n}, which does not accept the `--show-transaction` option every start and \
             stop passes it."
        ),
        Source::Manager if n >= TYPE_EXEC => format!("The running systemd is {n}, which {ANSWER}."),
        Source::Manager => format!("The running systemd is {n}, which {ANSWER}, and {TYPE_SIMPLE}."),
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

// Verbs ===============================================================================================================

/// The argv for one `systemctl <verb> <unit>` under `budget`.
///
/// `--show-transaction` on every path: its announcement ([`enqueued`]) is the only evidence that
/// systemd took the request — a `systemctl` that exits `0` having done nothing says so only when it
/// knows it ([`answered`]) — and on the bounded path it is also what the wait holds out for. A
/// budget that does not wait adds `--no-block`, which still announces the job (measured on systemd
/// 257).
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
/// add one — `SYSTEMD_LOG_TIME`, `SYSTEMD_LOG_LOCATION`, `SYSTEMD_LOG_TID` — so no `systemctl`
/// goetia runs can be given one; tolerating it anyway costs nothing.
fn enqueued(stderr: &[u8]) -> bool {
    const LINE: &[u8] = b"Enqueued anchor job ";
    stderr.windows(LINE.len()).any(|w| w == LINE)
}

/// Whether [`Systemctl::run_verb`] takes its bounded path under `budget` — the one whose
/// `systemctl` goetia watches, and which needs a thread for each of its streams to do it.
pub(super) fn watched(budget: Budget) -> bool {
    budget.waits() && budget != Budget::Unbounded
}

impl Systemctl {
    /// One `systemctl <verb> <unit>` under `budget`, as two deliberately different paths.
    ///
    /// A budget that does not wait becomes `systemctl <verb> --no-block`, the exact native
    /// expression of "issue the request and return": systemd enqueues the job and `systemctl` exits
    /// without waiting for it to complete. `Budget::Unbounded` is the plain blocking call. Both run
    /// to completion, because neither has anything to bound: a bound is required only on the
    /// second path.
    ///
    /// That path waits on `deadline`, which the caller derived at verb entry — but the deadline
    /// bounds only the wait for the job, never whether it is enqueued: until `systemctl` announces
    /// the job ([`enqueued`]) the wait is unbounded, so a budget a few milliseconds long still
    /// reaches systemd, and an expiry is then exactly what `budget::timed_out` says it is. `budget`
    /// only picks the path and the wording.
    fn run_verb(&self, verb: &str, unit: &str, budget: Budget, deadline: Deadline) -> Result<Finished> {
        let wait = if watched(budget) {
            Wait::Bounded {
                role: Role::AnnouncedRequest(enqueued),
                deadline,
            }
        } else {
            Wait::ToCompletion
        };
        self.run(Purpose::Request, &verb_args(verb, unit, budget), wait)
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
        "systemctl {verb} {unit} failed: {diagnostic} (goetia's budget expired before it read the \
         diagnostic to its end, so this may be only a prefix of what systemd wrote)"
    ))
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
    let why = if diagnostic.is_empty() {
        " and said nothing about why".to_string()
    } else {
        format!(": {diagnostic}")
    };
    Err(Error::Other(format!(
        "systemctl {verb} {unit} exited 0 without enqueuing a job, so systemd did not act on it{why}"
    )))
}

impl Systemctl {
    /// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
    /// needed. Idempotent: starting an already-active unit is a no-op that still exits 0. An expiry
    /// means the job was enqueued and not waited out — see [`Systemctl::run_verb`]. That holds for an active unit
    /// too: systemd answers the start with a job, not a state, so a budget shorter than that no-op job's
    /// round trip expires where launchd and SCM report started (see `ServiceManager::start`).
    ///
    /// Every generated unit carries `Type=exec` (see `generate::unit`'s doc comment for why), so a
    /// bounded `systemctl start` confirms the `exec` itself succeeded — not merely that systemd forked
    /// the process. A missing executable or missing user comes back as a failed start.
    pub(super) fn start_impl(&self, id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
        let unit = super::unit_name(id);
        match self.run_verb("start", &unit, budget, deadline)? {
            Finished::Exited { status, capture } if status.success() => took("start", &unit, &capture),
            Finished::Exited { capture, .. } => Err(failed("start", &unit, &capture)),
            Finished::Expired => Err(budget::timed_out(id, "running", budget)),
        }
    }

    /// Idempotent per `ServiceManager::stop`'s doc comment. Exit code 5 ("unit not loaded") means there
    /// was nothing to stop — the same convention `tests/support/service_guard.rs` already uses for
    /// cleanup — which is success here, not a failure to stop something that was never running.
    pub(super) fn stop_impl(&self, id: &str, budget: Budget, deadline: Deadline) -> Result<()> {
        let unit = super::unit_name(id);
        match self.run_verb("stop", &unit, budget, deadline)? {
            Finished::Exited { status, capture } if status.success() => took("stop", &unit, &capture),
            Finished::Exited { status, .. } if status.code() == Some(5) => Ok(()),
            Finished::Exited { capture, .. } => Err(failed("stop", &unit, &capture)),
            Finished::Expired => Err(budget::timed_out(id, "stopped", budget)),
        }
    }
}

/// `systemctl kill` of `unit`'s main process with `SIGKILL`, and nothing else of it: not the control
/// process, not the rest of the cgroup, which systemd cleans up when the main process is gone.
///
/// `--kill-who`, not `--kill-whom`: 245 and older do not know the second (MEASURED, 245), and the
/// first is still accepted by 255 and 257. `main-fail` rather than `main`: `main` exits `0` having
/// signalled nobody when there is no main process (MEASURED, 245 and 255), where `main-fail` fails
/// with the answer [`no_process`] reads.
const KILL_ARGS: [&str; 3] = ["kill", "--kill-who=main-fail", "--signal=SIGKILL"];

/// Whether `stderr` is systemd saying `unit` has no process to signal, word for word (MEASURED, 245
/// and 255, exit `1` both): it is not loaded, which is what an inactive unit is once nothing holds
/// it, or it is loaded and its main process is gone. Read off the kill's own answer, so no state
/// read before it can race the kill.
fn no_process(stderr: &str, unit: &str) -> bool {
    stderr.contains(&format!("Unit {unit} not loaded.")) || stderr.contains("No main process to kill")
}

impl Systemctl {
    /// `Ok` means the signal was delivered: systemd sends it before it answers, and goetia does not wait
    /// for the process to be gone.
    pub(super) fn kill_impl(&self, id: &str) -> Result<()> {
        let unit = super::unit_name(id);
        let (status, capture) = self.completed(Purpose::Request, &unit_args(&KILL_ARGS, &unit))?;
        if status.success() {
            return Ok(());
        }
        if no_process(&String::from_utf8_lossy(&capture.stderr), &unit) {
            return Err(Error::NotRunning { id: id.to_string() });
        }
        Err(failed("kill", &unit, &capture))
    }

    /// `systemctl enable` for `id`'s unit; a failure carries systemd's words.
    pub(super) fn enable_impl(&self, id: &str) -> Result<()> {
        self.unit_file_verb("enable", id)
    }

    /// `systemctl disable` for `id`'s unit, as [`enable_impl`]: `Systemd::disable` and `uninstall` both.
    pub(super) fn disable_impl(&self, id: &str) -> Result<()> {
        self.unit_file_verb("disable", id)
    }

    fn unit_file_verb(&self, verb: &str, id: &str) -> Result<()> {
        let unit = super::unit_name(id);
        let (status, capture) = self.completed(Purpose::Request, &unit_args(&[verb], &unit))?;
        if status.success() {
            return Ok(());
        }
        Err(Error::Other(format!(
            "systemctl {verb} {unit} failed: {}",
            String::from_utf8_lossy(&capture.stderr)
        )))
    }

    /// `systemctl restart --no-block`: one job, where a stop and a separate start leave a window in
    /// which the start replaces the stop. It stops the unit and then starts it — unless the unit is
    /// still activating, when systemd folds the restart into the start job already running: nothing is
    /// stopped, and the daemon comes up fresh from that start. [`took`] as for `start`: an exit of `0`
    /// with no job is a restart nobody made.
    pub(super) fn request_restart_impl(&self, id: &str) -> Result<()> {
        let unit = super::unit_name(id);
        match self.run_verb("restart", &unit, Budget::Immediate, Budget::Immediate.start())? {
            Finished::Exited { status, capture } if status.success() => took("restart", &unit, &capture),
            Finished::Exited { capture, .. } => Err(failed("restart", &unit, &capture)),
            Finished::Expired => unreachable!("a budget that does not wait never takes the bounded path"),
        }
    }
}

impl Systemctl {
    /// `props` of `unit`, every one of them: systemd answers each property `status_from_unit` asks for
    /// on any unit name, loaded or not (an empty `UnitFileState=` included), so an answer without one
    /// is no answer, and never the state its absence would default to.
    fn show_properties(&self, unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
        let joined = props.join(",");
        let (status, capture) = self.completed(Purpose::Read, &unit_args(&["show", "--property", &joined], unit))?;
        if !status.success() {
            return Err(Error::Other(format!(
                "systemctl show {unit} failed: {}",
                String::from_utf8_lossy(&capture.stderr)
            )));
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
            return Err(Error::Other(format!(
                "`systemctl show {unit}` answered without {}, so goetia cannot tell its live state",
                missing.join(", ")
            )));
        }
        Ok(map)
    }

    /// `UnitFileState` is the same install-state systemd derives for `systemctl is-enabled` — one query
    /// covers state, pid, and boot-enablement together.
    ///
    /// Asked only about a unit goetia finds installed, so `LoadState=not-found` — systemd has no unit
    /// file for the name — is a unit file systemd cannot see, one goetia's own mount namespace holds,
    /// say. Not one merely written and not yet reloaded: the manager picks a unit file up on demand and
    /// reports `loaded` for one written a moment earlier (MEASURED, 255). What systemd reports for such
    /// a name is not that unit's state, and never read as one.
    pub(super) fn status_from_unit(&self, unit: &str) -> Result<Status> {
        let props = self.show_properties(unit, &["ActiveState", "MainPID", "UnitFileState", "LoadState"])?;
        if props.get("LoadState").map(String::as_str) == Some("not-found") {
            return Err(Error::Other(format!(
                "systemd has not loaded the unit file goetia finds installed for `{unit}` (`LoadState=not-found`), so \
                 what it reports is not that unit's state"
            )));
        }
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
}

#[cfg(test)]
#[path = "systemctl_tests.rs"]
mod systemctl_tests;
