//! `wait_bounded`: block on a `cosca::Child` under a [`Deadline`], capturing
//! its stdout/stderr concurrently with the wait so a full pipe buffer can
//! never wedge it, and reporting on expiry whether the killed-and-reaped
//! child had already finished on its own.
//!
//! systemd and launchd both reach their manager through a subprocess that
//! already blocks (`systemctl`/`launchctl`); this is the only machinery
//! either backend needs to bound that wait. See the crate-level design
//! notes on `daemon start --timeout`.
//!
//! Both backends' calls into this module land in a later task, so outside
//! `#[cfg(test)]` nothing calls in here yet — mirrors `src/main.rs`'s
//! `#[cfg_attr(test, allow(dead_code))]` for the same shape of problem, with
//! the condition flipped: there it is the non-test path that is unused,
//! here it is the reverse.
#![cfg_attr(not(test), allow(dead_code))]

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitStatus;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::manager::budget::Deadline;

// command =============================================================================================================

/// `program` with `args`: stdin from `/dev/null`, stdout and stderr piped, and the
/// process tree **contained**, so a descendant that inherited the pipe dies with the
/// child instead of holding its write end open (`kill_tree`, in [`wait_bounded`]).
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
    Expired { capture: Capture },
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
            // Then the tree, best-effort: an uncontained child (the descendant test spawns
            // one) reports `Unsupported`, and a contained tree whose member refused SIGKILL
            // reports `Containment`. Neither can hang this call — the root is already dead —
            // and neither is silenced: a descendant still holding the pipe is exactly what
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
        None => Finished::Expired { capture },
    })
}

// drain / collect =====================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Which {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub(crate) enum Chunk {
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

/// Collect until every sender is dropped (both readers finished) or `deadline`
/// expires. `Ok((stdout, stderr, complete))`; `Err` iff a `Chunk::Failed`
/// arrived. Always drains what is already queued before consulting the
/// deadline, so an expiry never discards bytes that had already arrived —
/// but once expired, drains exactly what `Receiver::len()` snapshots as
/// already queued and stops there, so a reader that keeps producing past the
/// deadline cannot keep this loop fed forever (`recv_timeout(ZERO)` returns
/// a queued item rather than timing out, which is why that snapshot, not a
/// zero-duration `recv_timeout`, is what expiry drains through).
fn collect(rx: Receiver<Chunk>, deadline: Deadline) -> std::io::Result<(Vec<u8>, Vec<u8>, bool)> {
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
                // this instant; every one of those `n` chunks is already
                // queued, so `try_recv` cannot block or come up empty.
                // Anything sent after this snapshot is not waited for.
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
        Chunk::Failed(_, e) => return Err(e),
    }
    Ok(())
}

// expiry_disposition ==================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
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
