//! `wait_bounded`: block on a `cosca::Child` under a [`Deadline`], capturing
//! its stdout/stderr concurrently with the wait so a full pipe buffer can
//! never wedge it, and reporting on expiry whether the killed-and-reaped
//! child had already finished on its own.
//!
//! systemd and launchd both reach their manager through a subprocess that
//! already blocks (`systemctl`/`launchctl`); this is the only machinery
//! either backend needs to bound that wait. See the crate-level design
//! notes on `daemon start --timeout`.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitStatus;
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
    /// The deadline expired first; the child was killed and reaped.
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

/// Block until `child` exits or `deadline` expires; on expiry, kill the child, tear
/// its tree down and reap it. Its pipes are drained by up to two **detached** threads
/// reporting over a channel, so the return value never waits on a reader.
pub(crate) fn wait_bounded(mut child: cosca::Child, deadline: Deadline) -> Result<Finished, cosca::error::Error> {
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

    let waited = match deadline.at() {
        Some(at) => child.wait_deadline(at)?,
        None => Some(child.wait()?), // no timeout value is invented for an unbounded deadline
    };
    let status = match waited {
        Some(status) => Some(status),
        None => {
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

    let (stdout, stderr, complete) = collect(rx, deadline).map_err(cosca::error::Error::Io)?;
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
