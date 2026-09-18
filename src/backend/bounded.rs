//! `wait_bounded`: block on a `cosca::Child` under a [`Deadline`], capturing
//! its stdout/stderr concurrently with the wait so a full pipe buffer can
//! never wedge it, and reporting on expiry whether the child had already
//! finished on its own.
//!
//! The deadline bounds waiting for the manager's answer, never whether a
//! request is sent — see [`Role`].
//!
//! systemd and launchd both reach their manager through a subprocess that
//! already blocks (`systemctl`/`launchctl`); this is the only machinery
//! either backend needs to bound that wait. See the crate-level design
//! notes on `daemon start --timeout`.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitStatus;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Sender, TryRecvError};

use crate::manager::budget::Deadline;

// command =============================================================================================================

/// `program` with `args`: stdin from `/dev/null`, stdout and stderr piped, and the
/// process tree **contained**, so a descendant that inherited the pipe dies with the
/// child instead of holding its write end open: [`wait_bounded`] tears the contained
/// tree down with `kill_tree` on expiry, and by dropping the child on every path.
pub(crate) fn command(program: &str, args: &[&str]) -> Result<cosca::Command, cosca::error::Error> {
    let mut cmd = cosca::run(std::iter::once(program).chain(args.iter().copied()));
    cmd.stdin(cosca::Stdio::null())?;
    cmd.stdout(cosca::Stdio::pipe_out())?;
    cmd.stderr(cosca::Stdio::pipe_out())?;
    cmd.contain();
    Ok(cmd)
}

// wait_bounded ========================================================================================================

/// What the child's pipes yielded, and whether that is all of it.
#[derive(Debug)]
pub(crate) struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Both readers reached EOF inside the deadline, so these bytes are
    /// everything the child wrote. `false` means they are a **prefix**: the
    /// deadline cut the drain short, and no consumer may treat them as whole.
    pub complete: bool,
}

#[derive(Debug)]
pub(crate) enum Finished {
    /// The child exited on its own, inside the budget.
    Exited { status: ExitStatus, capture: Capture },
    /// The deadline expired first. The child was killed and reaped — or, under
    /// [`Role::Request`], left running, to be reaped on a thread of its own once it exits.
    ///
    /// Carries no capture. Whatever the child had written by then is a
    /// prefix no consumer may treat as whole, and the one thing every
    /// consumer in this crate does with an expiry is report it through
    /// `budget::timed_out` — a single shared constructor with no room for a
    /// per-call diagnostic, deliberately, so that all three backends word an
    /// expiry identically. A field nothing may read and nothing does read is
    /// surplus.
    Expired,
}

/// What the child does for its caller, which decides what the deadline may cut short.
///
/// The rule all three share: the deadline bounds waiting for the manager's answer, and never
/// decides whether a request reaches the manager at all.
// Each backend constructs only the roles its own tool needs: `systemctl` announces its request,
// `launchctl` does not.
#[derive(Debug)]
pub(crate) enum Role {
    /// A read. Killing it on expiry loses nothing.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Query,
    /// A request that says on stderr when the manager has it: `issued` holds once it has. Until
    /// then the wait is unbounded; only after it does the deadline apply, and killing the child
    /// then cancels nothing.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AnnouncedRequest(fn(&[u8]) -> bool),
    /// A request that never says when the manager has it. The deadline applies from the spawn, but
    /// an expiry leaves the child running rather than killing it, since a kill could land before
    /// the request is out, and its [`Reaper`] reaps it once it exits. launchd's alone: on Linux,
    /// cosca 0.4's cgroup containment kills a detached tree when it drops the leaf, so a
    /// [`command`] child would not be left running.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Request(Reaper),
}

/// A child [`spawn`]ed under its [`Role`].
pub(crate) struct Spawned {
    child: cosca::Child,
    role: Role,
}

/// Spawn `cmd` under `role`. A [`Role::Request`] brings its [`Reaper`], made before the caller sent
/// anything — see [`reapers`].
pub(crate) fn spawn(cmd: &mut cosca::Command, role: Role) -> Result<Spawned, cosca::error::Error> {
    Ok(Spawned {
        child: cmd.spawn()?,
        role,
    })
}

/// Block until the child exits or `deadline` expires, under its [`Role`]'s rule for what an expiry
/// does: kill the child, tear its tree down and reap it — or, for [`Role::Request`], leave it
/// running for its [`Reaper`]. Its pipes are drained by up to two **detached** threads reporting
/// over a channel, so the return value never waits on a reader.
pub(crate) fn wait_bounded(spawned: Spawned, deadline: Deadline) -> Result<Finished, cosca::error::Error> {
    let Spawned { mut child, role } = spawned;
    let (tx, rx) = crossbeam_channel::unbounded();
    if let Some(r) = child.stdout() {
        let tx = tx.clone();
        thread::spawn(move || drain(r, Which::Stdout, tx));
    }
    if let Some(r) = child.stderr() {
        let tx = tx.clone();
        thread::spawn(move || drain(r, Which::Stderr, tx));
    }
    drop(tx); // the readers are `collect`'s only senders; its EOF is their exit

    // Unbounded, deliberately: this is the request going out, not the wait for its answer. A child
    // that ends without announcing ends here too, once its output reaches EOF.
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    if let Role::AnnouncedRequest(issued) = role {
        while !issued(&stderr) {
            match rx.recv() {
                Ok(chunk) => absorb(chunk, &mut stdout, &mut stderr).map_err(cosca::error::Error::Io)?,
                Err(RecvError) => break, // both readers at EOF
            }
        }
    }

    let waited = match deadline.at() {
        Some(at) => child.wait_deadline(at)?,
        None => Some(child.wait()?), // no timeout value is invented for an unbounded deadline
    };
    let status = match (waited, role) {
        (Some(status), _) => Some(status),
        (None, Role::Request(reaper)) => {
            // Not killed: goetia cannot tell whether the request is out yet, and a request left
            // unsent because goetia ran out of budget is the one answer worse than waiting.
            reaper.reap(child);
            return Ok(Finished::Expired);
        }
        (None, Role::Query | Role::AnnouncedRequest(_)) => {
            // The root first, and its `Ok` is what makes the reap below unable to block.
            child.kill()?;
            // Then the tree, best-effort. What this achieves that `Drop` (at the end of
            // this function) does not: it runs BEFORE the reap below, while the killed
            // root is still a zombie pinning its pid and pgid. `Drop` tears a contained
            // tree down too, but only after the reap — for a group-based containment
            // mechanism (macOS's `FdMarker` first pass, Linux's `ProcessGroup` fallback)
            // that is the reap-then-recycle hazard cosca's own `kill_tree` precondition
            // documents. An uncontained child (the descendant test spawns one) reports
            // `Unsupported` here, and a contained tree whose member refused SIGKILL
            // reports `Containment`; the `Result` is discarded either way, so nothing
            // surfaces it, and a descendant still holding the pipe is exactly what
            // `collect` reports as `complete == false`.
            let _tree_teardown = child.kill_tree();
            let reaped = child.wait()?;
            match expiry_disposition(reaped) {
                Disposition::WeKilledIt => None,
                Disposition::ItExitedOnItsOwn => Some(reaped),
            }
        }
    };
    // Drop before collecting: `kill_on_drop` tears the contained tree down, so on the
    // natural-exit path too, nothing this call spawned outlives it holding a write end.
    drop(child);

    let (rest_out, rest_err, complete) = collect(rx, deadline).map_err(cosca::error::Error::Io)?;
    stdout.extend(rest_out);
    stderr.extend(rest_err);
    let capture = Capture {
        stdout,
        stderr,
        complete,
    };
    Ok(match status {
        Some(status) => Finished::Exited { status, capture },
        None => Finished::Expired,
    })
}

/// A thread made before a [`Role::Request`] is sent, which reaps it if an expiry leaves it
/// running: it still exits, and a caller that outlives it must not collect one zombie per expiry.
/// Made first because making it can fail, and a request no thread could reap must not be sent —
/// nor, in a sequence of requests, the first of them. See [`reapers`].
///
/// The thread never kills the child, not even after the reap. It [`cosca::Child::detach`]es the
/// reaped child rather than dropping it, because `Drop` tears a contained tree down, and after the
/// reap that is the reap-then-recycle hazard [`wait_bounded`]'s kill path is ordered to avoid:
/// macOS's `FdMarker` `killpg`s the root's pgid, which the kernel may by then have handed to
/// another group. `detach` touches no pid. With nothing to reap — the request exited inside its
/// budget — the sender is dropped unsent and the thread ends.
#[derive(Debug)]
pub(crate) struct Reaper(mpsc::Sender<cosca::Child>);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Reaper {
    fn new() -> std::io::Result<Reaper> {
        Reaper::with(cosca::Child::wait, |_| {})
    }

    /// A reaper that waits with `wait` and hands the status to `reaped`: the seam a test drives, to
    /// release its child only once the child is the reaper's, and to learn how it ended.
    fn with(
        wait: impl FnOnce(&cosca::Child) -> Result<ExitStatus, cosca::error::Error> + Send + 'static,
        reaped: impl FnOnce(Result<ExitStatus, cosca::error::Error>) + Send + 'static,
    ) -> std::io::Result<Reaper> {
        #[cfg(test)]
        test_hook::make_one()?;
        let (tx, rx) = mpsc::channel::<cosca::Child>();
        thread::Builder::new()
            .name("goetia-reaper".to_string())
            .spawn(move || {
                let Ok(child) = rx.recv() else { return };
                let status = wait(&child);
                child.detach();
                reaped(status);
            })?;
        Ok(Reaper(tx))
    }

    fn reap(self, child: cosca::Child) {
        // The thread blocks in `recv` until this send, so it cannot fail. Were it to, the child is
        // detached — never killed.
        if let Err(mpsc::SendError(child)) = self.0.send(child) {
            debug_assert!(false, "the reaper thread ended before it was handed its child");
            child.detach();
        }
    }
}

thread_local! {
    /// Reapers a [`Spares`] guard on this thread made ready ahead of the verbs it covers.
    static SPARES: std::cell::RefCell<Vec<Reaper>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Every reaper a verb's requests can need, made before it sends any: a sequence of requests must
/// not fail between two of them for want of a thread, so a failure here means nothing was sent.
/// Taken from this thread's [`Spares`] first, and made for the rest.
///
/// An array, so each request is handed a reaper of its own by name, and a verb that sends one more
/// request than it reserved for does not compile.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn reapers<const N: usize>() -> std::io::Result<[Reaper; N]> {
    let mut got = SPARES.with_borrow_mut(|spares| {
        let keep = spares.len().saturating_sub(N);
        spares.split_off(keep)
    });
    while got.len() < N {
        got.push(Reaper::new()?);
    }
    Ok(got.try_into().expect("exactly N reapers were gathered"))
}

/// Make `n` reapers ready now, for the verbs this thread runs while the returned guard lives — a
/// sequence of verbs, such as `restart`'s stop and start, whose later verbs must not fail for want of
/// a thread once an earlier one has sent something. Dropping the guard drops what is left. Not
/// nested: one guard per sequence.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn spare(n: usize) -> std::io::Result<Spares> {
    let made = (0..n).map(|_| Reaper::new()).collect::<std::io::Result<Vec<_>>>()?;
    SPARES.with_borrow_mut(|spares| spares.extend(made));
    Ok(Spares(()))
}

/// See [`spare`].
#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Spares(());

impl Drop for Spares {
    fn drop(&mut self) {
        SPARES.with_borrow_mut(Vec::clear);
    }
}

/// Makes reaper creation fail on demand, on this thread only, so the paths that must refuse to send
/// anything when it does are testable.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;

    thread_local! {
        static MAKEABLE: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Let this thread make only `n` more reapers, until the returned guard drops.
    pub(crate) fn allow(n: usize) -> Allowance {
        MAKEABLE.set(Some(n));
        Allowance(())
    }

    /// See [`allow`].
    pub(crate) struct Allowance(());

    impl Drop for Allowance {
        fn drop(&mut self) {
            MAKEABLE.set(None);
        }
    }

    pub(super) fn make_one() -> std::io::Result<()> {
        match MAKEABLE.get() {
            Some(0) => Err(std::io::Error::other("no reaper may be made (test hook)")),
            Some(n) => {
                MAKEABLE.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }
}

// drain / collect =====================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum Chunk {
    Bytes(Which, Vec<u8>),
    /// The reader stopped early: a read error, or a panic caught at the thread
    /// boundary. Never silence.
    Failed(Which, std::io::Error),
}

/// Read `src` to EOF, sending each chunk. A read error, or a panic inside
/// `src`, sends `Chunk::Failed` instead of ending the stream silently.
/// Returns when `src` is exhausted, fails, or the receiver is gone.
fn drain<R: Read>(mut src: R, which: Which, tx: Sender<Chunk>) {
    // Kept outside the `catch_unwind`'d closure so a caught panic can still
    // report through it after the closure that moved `tx` is gone.
    let failure_tx = tx.clone();
    let outcome = panic::catch_unwind(AssertUnwindSafe(move || {
        let mut buf = [0u8; 8192];
        loop {
            match src.read(&mut buf) {
                Ok(0) => return None, // EOF: a normal end of stream
                Ok(n) => {
                    if tx.send(Chunk::Bytes(which, buf[..n].to_vec())).is_err() {
                        return None; // the receiver is gone; nothing more to do
                    }
                }
                Err(e) => return Some(e),
            }
        }
    }));
    match outcome {
        Ok(Some(e)) => {
            let _ = failure_tx.send(Chunk::Failed(which, e));
        }
        Ok(None) => {}
        Err(_) => {
            let _ = failure_tx.send(Chunk::Failed(which, std::io::Error::other("panicked while reading")));
        }
    }
}

/// What [`collect`] needs from its channel: a seam, so a test can hand it a
/// source that never runs dry.
trait ChunkSource {
    fn len(&self) -> usize;
    fn try_recv(&self) -> Result<Chunk, TryRecvError>;
    fn recv(&self) -> Result<Chunk, RecvError>;
    fn recv_timeout(&self, timeout: Duration) -> Result<Chunk, RecvTimeoutError>;
}

impl ChunkSource for Receiver<Chunk> {
    fn len(&self) -> usize {
        Receiver::len(self)
    }

    fn try_recv(&self) -> Result<Chunk, TryRecvError> {
        Receiver::try_recv(self)
    }

    fn recv(&self) -> Result<Chunk, RecvError> {
        Receiver::recv(self)
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Chunk, RecvTimeoutError> {
        Receiver::recv_timeout(self, timeout)
    }
}

/// Collect until every sender is dropped (both readers finished) or `deadline`
/// expires. `Ok((stdout, stderr, complete))`; `Err` iff a `Chunk::Failed`
/// arrived. Always drains what is already queued before consulting the
/// deadline, so an expiry never discards bytes that had already arrived —
/// but once expired, drains exactly what `Receiver::len()` snapshots as
/// already queued and stops there, so a reader that keeps producing past the
/// deadline cannot keep this loop fed forever (`recv_timeout(ZERO)` returns
/// a queued item rather than timing out, which is why that snapshot, not a
/// zero-duration `recv_timeout`, is what expiry drains through).
fn collect(rx: impl ChunkSource, deadline: Deadline) -> std::io::Result<(Vec<u8>, Vec<u8>, bool)> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    loop {
        match deadline.remaining() {
            None => match rx.recv() {
                Ok(chunk) => absorb(chunk, &mut stdout, &mut stderr)?,
                Err(_) => return Ok((stdout, stderr, true)), // both senders dropped
            },
            Some(Duration::ZERO) => {
                // Expired. `len()` is a race-free snapshot of the channel at
                // this instant, counting every chunk whose send has begun, so
                // `try_recv` cannot come up empty for any of them. Anything
                // sent after this snapshot is not waited for.
                for _ in 0..rx.len() {
                    let chunk = rx
                        .try_recv()
                        .expect("snapshotted length guarantees this chunk is queued");
                    absorb(chunk, &mut stdout, &mut stderr)?;
                }
                return Ok((stdout, stderr, false));
            }
            Some(remaining) => match rx.recv_timeout(remaining) {
                Ok(chunk) => absorb(chunk, &mut stdout, &mut stderr)?,
                Err(RecvTimeoutError::Disconnected) => return Ok((stdout, stderr, true)),
                Err(RecvTimeoutError::Timeout) => {} // re-check: the deadline has now expired
            },
        }
    }
}

fn absorb(chunk: Chunk, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> std::io::Result<()> {
    match chunk {
        Chunk::Bytes(Which::Stdout, bytes) => stdout.extend_from_slice(&bytes),
        Chunk::Bytes(Which::Stderr, bytes) => stderr.extend_from_slice(&bytes),
        // The failing stream is named, not dropped: "a reader failed" and
        // "the stderr reader failed" are different diagnostics, and this is
        // the only place that knows which.
        Chunk::Failed(which, e) => {
            let stream = match which {
                Which::Stdout => "stdout",
                Which::Stderr => "stderr",
            };
            return Err(std::io::Error::new(e.kind(), format!("{stream}: {e}")));
        }
    }
    Ok(())
}

// expiry_disposition ==================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    WeKilledIt,
    ItExitedOnItsOwn,
}

/// Which outcome a reaped status represents on the expiry path: `SIGKILL` is
/// ours, anything else means the child beat us to it.
fn expiry_disposition(reaped: ExitStatus) -> Disposition {
    if reaped.signal() == Some(libc::SIGKILL) {
        Disposition::WeKilledIt
    } else {
        Disposition::ItExitedOnItsOwn
    }
}

#[cfg(test)]
#[path = "bounded_tests.rs"]
mod bounded_tests;
