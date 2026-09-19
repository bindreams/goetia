//! Thin wrappers over `systemctl` subprocess invocations: reload, start/stop, and reading a unit's
//! live state back — every one through the manager boundary, [`door`] and [`answered`].

use std::cell::Cell;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::process::Command;

use crate::backend::bounded::{self, Capture, Finished, Role};
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State, Status};

// The manager boundary ================================================================================================

/// What one `systemctl` run is for: what [`door`] establishes before it runs, and what a failure to
/// run it says.
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

/// Where every `systemctl` goetia runs is refused before it is spawned: on evidence, known without
/// asking, that no manager can be asked here ([`Host::evidence`]); and for a request, until the
/// manager has passed the version gate. The gate's `show` is what finds out, before anything acts,
/// a chroot goetia could not see for itself: `enable` and `disable` act on unit files offline, in a
/// chroot and on a system systemd did not boot, and say nothing about it (measured on 257).
/// [`answered`] is the other half.
fn door(purpose: Purpose) -> Result<()> {
    if let Some(evidence) = host().evidence() {
        return Err(no_manager(evidence));
    }
    if purpose == Purpose::Request {
        require_supported()?;
    }
    Ok(())
}

/// What `systemctl` says when it asked no manager and did nothing, exiting `0`: "Running in chroot,
/// ignoring command 'start'" from systemd 246 on, "… ignoring request: start" on 242 to 245, and
/// "… ignoring request." with no verb to name, on every version (systemd's `verbs.c` and
/// `systemctl.c`; 257's binary has both).
const IGNORED: [&str; 2] = ["ignoring command", "ignoring request"];

/// Every `systemctl` answer, checked before anything reads it as one: an answer saying it ignored
/// what it was asked ([`IGNORED`]) is the refusal on every path, and never a success: the backstop
/// for what [`door`] could not see.
///
/// It is the last line in a chroot with no `/proc` at all — `/proc/1/root` and both mount tables
/// are `ENOENT`, so goetia establishes nothing, while `systemctl` reports the chroot from that same
/// absence (MEASURED, 255 and 257). Nothing in the environment can put it there instead: every
/// switch that would declare a chroot is taken off the child ([`DENIED`]), so what reaches this is
/// always the child's own detection. Where PID 1 is hidden from goetia rather than absent, as under
/// a `hidepid` `/proc`, `systemctl` runs with goetia's credentials and hits the same wall: it
/// reports nothing, and this backstop does not fire either.
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
/// because the switches are systemd's to add, not goetia's to keep up with: `systemctl` and the
/// library it links carry 45 such names on 255 and 90 on 257, and *three* of those turn off, each
/// on its own, the chroot report [`answered`] is the last line against. Naming them is what missed
/// the next one twice.
///
/// The three, MEASURED on 255 and 257 in a chroot with no `/proc`, where goetia establishes nothing
/// of its own: `SYSTEMD_IGNORE_CHROOT` true; `SYSTEMD_IN_CHROOT` false, on 257 on; and
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
/// reason rather than for anything it holds today: MEASURED on 255 and 257, both carry the same
/// five names, and none of them silences the chroot report. `SYSTEMCTL_FORCE_BUS` is nonetheless
/// the other shape goetia refuses for — it makes `systemctl` reach the manager over the bus
/// `DBUS_SYSTEM_BUS_ADDRESS` names instead of this root's private socket. MEASURED with that
/// address pointed at a path that does not exist: `show` answered `Version=…` without the switch
/// and failed to connect with it. Off the child, the address decides nothing.
///
/// What removing the rest costs: nothing any real environment carries. MEASURED on 255 and 257 — a
/// plain shell, a login shell and `sudo` carry no name under either prefix, and the one systemd
/// itself puts in a process's environment, `SYSTEMD_EXEC_PID`, changes nothing `systemctl` does.
/// Both of its streams are captured, never a terminal, so `SYSTEMD_PAGER`, `SYSTEMD_LESS` and
/// `SYSTEMD_PAGERSECURE` changed nothing either.
///
/// What they do *not* reach is a variable under neither prefix that `systemctl` also reads.
/// MEASURED on 255 and 257, each with the report in force: `PAGER=cat`, `LESS=X`,
/// `TERM=xterm-256color`, `LC_ALL=de_DE.UTF-8` and `LANG=ja_JP.UTF-8` all leave it word for word in
/// English, which is what [`IGNORED`] matches on.
const DENIED: [&str; 2] = ["SYSTEMD_", "SYSTEMCTL_"];

/// A child's environment, as each of the two `Command` types goetia spawns a `systemctl` through
/// offers it: [`environment`] is applied through this, so neither path can be given one and not the
/// other.
trait Environment {
    fn set(&mut self, key: &str, value: &str);
    fn remove(&mut self, key: &str);
}

impl Environment for Command {
    fn set(&mut self, key: &str, value: &str) {
        self.env(key, value);
    }

    fn remove(&mut self, key: &str) {
        self.env_remove(key);
    }
}

impl Environment for cosca::Command {
    fn set(&mut self, key: &str, value: &str) {
        self.env(key, value);
    }

    fn remove(&mut self, key: &str) {
        self.env_remove(key);
    }
}

/// `cmd`'s environment as every `systemctl` goetia runs gets it: every [`DENIED`] name `inherited`
/// carries removed from it, and [`ENVIRONMENT`] set on it. The two are kept disjoint here — a name
/// goetia sets is never also removed — rather than by the order the calls happen to be made in, so
/// neither `Command`'s own rule for a key both set and removed can decide what the child gets.
fn environment_from(cmd: &mut impl Environment, inherited: impl Iterator<Item = String>) {
    let given = |key: &str| ENVIRONMENT.iter().any(|(name, _)| *name == key);
    let denied = |key: &str| DENIED.iter().any(|prefix| key.starts_with(prefix));
    for key in inherited.filter(|key| denied(key) && !given(key)) {
        cmd.remove(&key);
    }
    for (key, value) in ENVIRONMENT {
        cmd.set(key, value);
    }
}

/// [`environment_from`] over the names this process would pass a child of its own. A name that is
/// not UTF-8 is skipped: `getenv` finds a switch by its exact ASCII name, so a name that does not
/// decode is not one of them.
fn environment(cmd: &mut impl Environment) {
    environment_from(cmd, std::env::vars_os().filter_map(|(key, _)| key.into_string().ok()));
}

/// `systemctl <args>` for `purpose`, through [`door`], run to completion, and [`answered`].
fn systemctl(purpose: Purpose, args: &[&str]) -> Result<std::process::Output> {
    door(purpose)?;
    let program = program();
    ran(purpose, &program.iter().map(String::as_str).collect::<Vec<_>>(), args)
}

/// [`systemctl`] past its door: `program` then `args`, run to completion, and [`answered`]. A
/// request's two ways to fail are kept apart ([`requested`]); anything else that fails to run is a
/// plain failure, since it changes nothing.
fn ran(purpose: Purpose, program: &[&str], args: &[&str]) -> Result<std::process::Output> {
    let (bin, prefix) = program.split_first().expect("a program to run");
    let mut cmd = Command::new(bin);
    cmd.args(prefix).args(args);
    environment(&mut cmd);
    let output = match purpose {
        Purpose::Request => requested(&mut cmd, "systemctl", args)?,
        Purpose::Probe | Purpose::Read => cmd
            .output()
            .map_err(|e| Error::Other(format!("failed to run `systemctl {}`: {e}", args.join(" "))))?,
    };
    answered(&output.stdout, &output.stderr)?;
    Ok(output)
}

/// The argv every `systemctl` starts with: `systemctl`, or on a test's thread its stand-in.
fn program() -> Vec<String> {
    #[cfg(test)]
    if let Some(program) = stand_in::program() {
        return program;
    }
    vec!["systemctl".to_string()]
}

/// What goetia reads of this system without asking `systemctl`, or on a test's thread what it
/// stands in.
fn host() -> Host {
    #[cfg(test)]
    if let Some(host) = stand_in::host() {
        return host;
    }
    Host::this()
}

/// A stand-in for `systemctl`, and for the host it runs on, on this thread: every `systemctl` goetia
/// runs is `sh -c <script> systemctl <args>`, so a test can drive every path without a systemd.
/// Never a file on disk — `exec`ing one just written races every `fork` another test thread makes
/// (`ETXTBSY`).
#[cfg(test)]
pub(super) mod stand_in {
    use std::cell::RefCell;

    use super::Host;

    thread_local! {
        static STAND_IN: RefCell<Option<(Option<String>, Host)>> = const { RefCell::new(None) };
    }

    /// Until the returned guard drops, run `script` in place of `systemctl` — the real one for
    /// `None` — on `host`.
    pub(in crate::backend::systemd) fn set(script: Option<&str>, host: Host) -> Guard {
        STAND_IN.set(Some((script.map(str::to_string), host)));
        Guard(())
    }

    /// See [`set`].
    pub(in crate::backend::systemd) struct Guard(());

    impl Drop for Guard {
        fn drop(&mut self) {
            STAND_IN.set(None);
        }
    }

    pub(super) fn program() -> Option<Vec<String>> {
        STAND_IN.with_borrow(|stand_in| {
            let script = stand_in.as_ref()?.0.as_ref()?;
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                script.clone(),
                "systemctl".to_string(),
            ])
        })
    }

    pub(super) fn host() -> Option<Host> {
        STAND_IN.with_borrow(|stand_in| stand_in.as_ref().map(|(_, host)| host.clone()))
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

/// `systemctl <args>`, a request — `daemon-reload`, `enable`, `disable` — run to completion. See
/// [`requested`] for what a failure to run it says.
pub(super) fn run_systemctl(args: &[&str]) -> Result<std::process::Output> {
    systemctl(Purpose::Request, args)
}

/// `cmd`, a request, run to completion, with its two ways to fail kept apart. std's `spawn` fails
/// only before the program runs — `fork`, or an `exec` the child reports back — so that is a
/// request never sent. Reading its output or waiting on it fails only after, when the request may
/// already be out: [`Error::RequestInDoubt`], never the plain failure that says it was not sent.
/// `Command::output()` would merge the two; its stdin, `/dev/null`, is kept.
fn requested(cmd: &mut Command, program: &str, args: &[&str]) -> Result<std::process::Output> {
    requested_with(cmd, program, args, |child| {
        #[cfg(test)]
        if let Some(e) = bounded::test_hook::waiting() {
            child.wait_with_output()?;
            return Err(std::io::Error::other(e.to_string()));
        }
        child.wait_with_output()
    })
}

/// [`requested`], with how the running child is waited on named, so a test can make that fail.
fn requested_with(
    cmd: &mut Command,
    program: &str,
    args: &[&str],
    wait: impl FnOnce(std::process::Child) -> std::io::Result<std::process::Output>,
) -> Result<std::process::Output> {
    let request = format!("{program} {}", args.join(" "));
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Other(format!("could not run `{request}`, so it was not sent: {e}")))?;
    wait(child).map_err(|e| Error::RequestInDoubt {
        request,
        reached: false,
        detail: e.to_string(),
    })
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
/// as a successful install — the on-disk artifact and systemd's loaded view of it may have diverged
/// — and says the unit was written. A reload in doubt stays in doubt, exit `4`; any other failure
/// is exit `1`.
pub(super) fn daemon_reload_or_report(id: &str) -> Result<()> {
    daemon_reload().map_err(|e| match e {
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

// The version gate ====================================================================================================

/// The oldest systemd goetia runs against: every start and stop runs `systemctl --show-transaction`
/// (242).
const SYSTEMD_FLOOR: u32 = 242;

/// The oldest systemd that parses the `Type=exec` generated units declare.
const TYPE_EXEC: u32 = 240;

/// Refuse a systemd older than [`SYSTEMD_FLOOR`]. Asked before goetia writes a unit, and by
/// [`door`] before every request: an older systemd does not reject `Type=exec` but logs that it
/// cannot parse it and runs the unit as `Type=simple`, silently losing the failed-exec detection
/// the directive is there for. Asked once per [`GateScope`].
pub(super) fn require_supported() -> Result<()> {
    gated(supported)
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
    /// `systemctl --version`: the client is what parses the `--show-transaction` option, and a
    /// newer manager does not rescue an older client.
    Client,
}

/// The gate itself: both the running manager and the client must be new enough. Where no manager
/// can be asked there is no gate to pass — the probes' own [`door`] and [`answered`] refuse there —
/// and any other failure to read the manager's version is a refusal too: a manager that is running
/// and could not be asked is not one goetia can vouch for.
fn supported() -> Result<()> {
    let client = systemctl(Purpose::Probe, &["--version"])?;
    if !client.status.success() {
        return Err(Error::Other(format!(
            "`systemctl --version` failed, so goetia cannot tell whether this is systemd {SYSTEMD_FLOOR}+: {}",
            String::from_utf8_lossy(&client.stderr)
        )));
    }
    let client = String::from_utf8_lossy(&client.stdout);
    let client = client.lines().next().unwrap_or_default();
    let manager = systemctl(Purpose::Probe, &["show", "--property=Version", "--value"])?;
    let reported = String::from_utf8_lossy(&manager.stdout);
    if !manager.status.success() || reported.trim().is_empty() {
        let how = if manager.status.success() {
            "printed no version".to_string()
        } else {
            format!("failed ({})", manager.status)
        };
        return Err(Error::Other(format!(
            "`systemctl show --property=Version` {how}, so goetia cannot tell whether the running systemd is \
             {SYSTEMD_FLOOR}+: {}",
            String::from_utf8_lossy(&manager.stderr)
        )));
    }
    let verdicts = [
        verdict(Source::Manager, reported.trim()),
        verdict(Source::Client, client),
    ];
    refusal(verdicts.into_iter().flatten().collect())
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
        vec![verb, "--show-transaction", unit]
    } else {
        vec![verb, "--no-block", "--show-transaction", unit]
    }
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
    door(Purpose::Request)?;
    let program = program();
    let program: Vec<&str> = program.iter().map(String::as_str).collect();
    run_verb_via(&program, &verb_args(verb, unit, budget), budget, deadline)
}

/// Whether [`run_verb`] takes its bounded path under `budget` — the one whose `systemctl` goetia
/// watches, and which needs a thread for each of its streams to do it.
pub(super) fn watched(budget: Budget) -> bool {
    budget.waits() && budget != Budget::Unbounded
}

/// [`run_verb`] past its [`door`], with the program named — the argv `args` follow — so its bounded
/// path is testable against a child that cannot exit on its own. What either path answers is
/// [`answered`].
fn run_verb_via(program: &[&str], args: &[&str], budget: Budget, deadline: Deadline) -> Result<Finished> {
    let failed = |e: String| Error::Other(format!("failed to run `systemctl {}`: {e}", args.join(" ")));
    let in_doubt = |e: String, reached: bool| Error::RequestInDoubt {
        request: format!("systemctl {}", args.join(" ")),
        reached,
        detail: e,
    };
    if !watched(budget) {
        let output = ran(Purpose::Request, program, args)?;
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

    let (bin, prefix) = program.split_first().expect("a program to run");
    let argv: Vec<&str> = prefix.iter().chain(args).copied().collect();
    let mut cmd = bounded::command(bin, &argv).map_err(|e| failed(e.to_string()))?;
    environment(&mut cmd);
    let spawned = bounded::spawn(&mut cmd, Role::AnnouncedRequest(enqueued)).map_err(|e| match e {
        bounded::SpawnError::NotRun(e) => failed(e.to_string()),
        bounded::SpawnError::MayHaveRun(e) => in_doubt(e.to_string(), false),
    })?;
    // Past the spawn, `systemctl` is running, and a failure to watch it leaves its request in doubt —
    // one it had announced reached systemd.
    let finished =
        bounded::wait_bounded(spawned, deadline).map_err(|lost| in_doubt(lost.error.to_string(), lost.announced))?;
    if let Finished::Exited { capture, .. } = &finished {
        answered(&capture.stdout, &capture.stderr)?;
    }
    Ok(finished)
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

/// `props` of `unit`, every one of them: systemd answers each property `status_from_unit` asks for
/// on any unit name, loaded or not (an empty `UnitFileState=` included), so an answer without one
/// is no answer, and never the state its absence would default to.
fn show_properties(unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
    let joined = props.join(",");
    let output = systemctl(Purpose::Read, &["show", "--property", &joined, unit])?;
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
pub(super) fn status_from_unit(unit: &str) -> Result<Status> {
    let props = show_properties(unit, &["ActiveState", "MainPID", "UnitFileState", "LoadState"])?;
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

#[cfg(test)]
#[path = "systemctl_tests.rs"]
mod systemctl_tests;
