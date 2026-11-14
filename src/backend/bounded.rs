//! `wait_bounded`: block on a `cosca::Child` under a [`Deadline`], capturing its stdout and stderr
//! where no full pipe can wedge it, and reporting on expiry whether the child had already finished
//! on its own.
//!
//! The deadline bounds waiting for the manager's answer, never whether a request is sent — see
//! [`Role`]. Everything a child's output needs is made before it is spawned — see [`Reservation`].
//!
//! systemd and launchd both reach their manager through a subprocess that
//! already blocks (`systemctl`/`launchctl`); this is the only machinery
//! either backend needs to bound that wait. See the crate-level design
//! notes on `daemon start --timeout`.

mod spares;

use std::fs::File;
use std::io::{self, Read, Seek};
use std::os::unix::process::ExitStatusExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitStatus;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Sender, TryRecvError};
#[cfg_attr(not(target_os = "macos"), allow(unused_imports))]
pub(crate) use spares::Kept;
#[cfg(test)]
pub(crate) use spares::test_hook;
pub(crate) use spares::{Reaper, Reservation};

use crate::backend::unreaped::release_unreaped;
use crate::error::within;
use crate::manager::budget::Deadline;

// command =============================================================================================================

/// `program` with `args`: stdin from `/dev/null`, and the process tree **contained**, so a
/// descendant that inherited a pipe dies with the child instead of holding its write end open:
/// [`wait_bounded`] tears the contained tree down with `send_kill_tree` on expiry, and by dropping the
/// child on every path. Where its output goes is [`spawn`]'s to decide, by [`Role`].
pub(crate) fn command(program: &str, args: &[&str]) -> Result<cosca::Command, cosca::error::Error> {
    let mut cmd = cosca::run(std::iter::once(program).chain(args.iter().copied()));
    cmd.stdin(cosca::Stdio::null())?;
    cmd.contain();
    Ok(cmd)
}

// wait_bounded ========================================================================================================

/// The streams a child's output is captured from, and so the temp files a [`Role::Query`] or
/// [`Role::Request`] draws: stdout and stderr. What a role draws is [`Role::files`].
pub(crate) const STREAMS: usize = 2;

/// What the child wrote, and whether that is all of it.
#[derive(Debug)]
pub(crate) struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// These bytes are everything the child wrote. `false` means they may be only a **prefix**, which
    /// no consumer may treat as whole: the deadline ended the reading of a stream read as it was
    /// written before its end was seen — perhaps with nothing left to read, which is not waited for.
    /// A stream written to a file is whole once the child has exited and its contained tree is
    /// torn down, which is before it is read.
    pub complete: bool,
}

#[derive(Debug)]
pub(crate) enum Finished {
    /// The child exited on its own, inside the budget.
    Exited { status: ExitStatus, capture: Capture },
    /// The deadline expired first. The child was killed and reaped.
    ///
    /// Carries no capture. Whatever the child had written by then is a
    /// prefix no consumer may treat as whole, and the one thing every
    /// consumer in this crate does with an expiry is report it through
    /// `budget::timed_out` — a single shared constructor with no room for a
    /// per-call diagnostic, deliberately, so that all three backends word an
    /// expiry identically. A field nothing may read and nothing does read is
    /// surplus.
    Expired,
    /// The deadline expired first, and the child is a [`Role::Request`] left running: the caller
    /// may end it ([`Left::kill`]) or, by dropping this, leave it to its [`Reaper`].
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Left(Left),
}

/// Carries a [`Fate`] out on the child a [`Left`] holds.
type Settle = Box<dyn FnOnce(Fate) + Send>;

/// How a request left running by an expiry ends up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fate {
    /// The caller ended the child.
    Kill,
    /// The child runs on, and its [`Reaper`] reaps it once it exits.
    Release,
}

/// A [`Role::Request`] the deadline left running, held until its caller decides its [`Fate`].
///
/// goetia cannot tell from the client whether the request is out, and a kill before it is would
/// lose it, so nothing here kills unasked: dropping a `Left` releases the child to its reaper. A
/// caller that learns by other means that the manager has the request may [`kill`](Left::kill) the
/// child, which then costs the request nothing.
pub(crate) struct Left(Option<Settle>);

impl Left {
    pub(crate) fn new(settle: impl FnOnce(Fate) + Send + 'static) -> Left {
        Left(Some(Box::new(settle)))
    }

    /// End the child. One that already exited is not an error; one that cannot be signalled runs
    /// on, which is [`Fate::Release`], and says so on stderr.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn kill(mut self) {
        self.settle(Fate::Kill);
    }

    fn settle(&mut self, fate: Fate) {
        if let Some(settle) = self.0.take() {
            settle(fate);
        }
    }
}

impl Drop for Left {
    fn drop(&mut self) {
        self.settle(Fate::Release);
    }
}

impl std::fmt::Debug for Left {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Left")
    }
}

/// What the child does for its caller, which decides what the deadline may cut short, and where its
/// output goes.
///
/// The rule all three share: the deadline bounds waiting for the manager's answer, and never
/// decides whether a request reaches the manager at all.
// Each backend constructs only the roles its own tool needs: `systemctl` announces its request,
// `launchctl` does not.
#[derive(Debug)]
pub(crate) enum Role {
    /// A read a verb counted among its calls. Killing it on expiry loses nothing. Its output goes to
    /// temp files, read once it has exited, taken from the verb's reservation.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Query,
    /// A read no verb counted — launchd's `status` and `list`. As [`Role::Query`], but its files are
    /// made on the spot, never drawn from a reservation; failing to make one sends nothing.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    UncountedQuery,
    /// A request that says on stderr when the manager has it: `issued` holds once it has. Until
    /// then the wait is unbounded; only after it does the deadline apply, and killing the child
    /// then cancels nothing. Both its streams are pipes, each read as it is written by a thread
    /// made before the spawn: no temp file, since `systemctl` runs where none may be writable.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AnnouncedRequest(fn(&[u8]) -> bool),
    /// A request that never says when the manager has it. The deadline applies from the spawn, but
    /// an expiry leaves the child running rather than killing it, since a kill could land before
    /// the request is out, and its [`Reaper`] reaps it once it exits. The caller gets the child
    /// back as a [`Left`]. launchd's alone.
    ///
    /// Its output goes to temp files, never pipes: a pipe goetia stopped reading — on expiry, or by
    /// exiting — kills the writer with `SIGPIPE`, and a file does not.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Request(Reaper),
}

impl Role {
    /// The temp files each call that draws from a reservation ([`Role::Query`] or [`Role::Request`])
    /// draws: a verb's call count is turned into files by it.
    pub(crate) const COUNTED_FILES: usize = STREAMS;

    /// The temp files a [`spawn`] under this role draws from its reservation. The one definition:
    /// `spawn`, the verbs' call counts and the test `Fake` all read it.
    pub(crate) fn files(&self) -> usize {
        match self {
            Role::Query | Role::Request(_) => Self::COUNTED_FILES,
            Role::UncountedQuery | Role::AnnouncedRequest(_) => 0,
        }
    }

    /// The listener threads a [`spawn`] under this role draws from its reservation: one for each
    /// stream of an announced request, which is read as it is written. The one definition, as for
    /// [`Role::files`].
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn listeners(&self) -> usize {
        match self {
            Role::AnnouncedRequest(_) => STREAMS,
            Role::Query | Role::UncountedQuery | Role::Request(_) => 0,
        }
    }
}

/// A child [`spawn`]ed under its [`Role`], with where its output went.
pub(crate) struct Spawned {
    child: cosca::Child,
    role: Role,
    stdout: Stream,
    stderr: Stream,
}

/// Where one of a spawned child's streams went.
enum Stream {
    /// A temp file, read once the child has exited.
    File(File),
    /// A pipe a thread reads as it is written, heard here as it does.
    Heard(Receiver<Chunk>),
}

/// What a stream needs, made before the spawn.
enum Ready {
    File(File),
    Listener(spares::Listener),
}

impl Ready {
    /// A temp file, or for a [`Role::AnnouncedRequest`] a thread to read a pipe, drawn from
    /// `reservation`: see [`Role`].
    fn for_role(role: &Role, reservation: &mut Reservation) -> Result<Ready, cosca::error::Error> {
        Ok(match role {
            Role::AnnouncedRequest(_) => {
                Ready::Listener(reservation.listener().map_err(not_run("thread to read its output"))?)
            }
            _ if role.files() > 0 => Ready::File(reservation.file().map_err(not_run("temp file for its output"))?),
            _ => Ready::File(spares::make_file().map_err(not_run("temp file for its output"))?),
        })
    }

    /// Where the child writes this stream.
    fn stdio(&self) -> Result<cosca::Stdio, cosca::error::Error> {
        Ok(match self {
            Ready::File(file) => {
                cosca::Stdio::from_file(file.try_clone().map_err(not_run("descriptor for its output"))?)
            }
            Ready::Listener(_) => cosca::Stdio::pipe_out(),
        })
    }

    /// The stream, once the child is spawned: a listener is handed the pipe it was made for.
    fn spawned(self, pipe: Option<cosca::PipeReader>) -> Stream {
        match self {
            Ready::File(file) => Stream::File(file),
            Ready::Listener(listener) => Stream::Heard(listener.listen(pipe)),
        }
    }
}

/// How a [`spawn`] failed: whether the child may have run, which for a request is whether it may
/// have reached the manager.
#[derive(Debug)]
pub(crate) enum SpawnError {
    /// Before the child ran: nothing it would have sent was sent.
    NotRun(cosca::error::Error),
    /// Perhaps after it ran: cosca's [`MayHaveStarted`](cosca::error::Error::MayHaveStarted), which
    /// every other error of a spawn says it was not.
    MayHaveRun(cosca::error::Error),
}

impl SpawnError {
    /// `e`, as a spawn failed with it: placed by whether cosca says the program may have started.
    pub(crate) fn of_spawn(e: cosca::error::Error) -> SpawnError {
        if e.fate().is_some() {
            SpawnError::MayHaveRun(e)
        } else {
            SpawnError::NotRun(e)
        }
    }
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::NotRun(e) | SpawnError::MayHaveRun(e) => write!(f, "{}", cosca::error::Report::new(e)),
        }
    }
}

/// Spawn `cmd` under `role`, having first made everything its output needs: a temp file for each
/// stream — or, for a [`Role::AnnouncedRequest`], a thread to read each as it is written, since it
/// announces on stderr and must not depend on a writable temp directory. A failure to make one is
/// [`SpawnError::NotRun`], and says so. A [`Role::Request`] brings its [`Reaper`], made before the
/// caller sent anything — see [`Reservation`]. Its files and threads are drawn from `reservation`:
/// [`Role::files`] and [`Role::listeners`]; a call that draws none is handed an empty one.
pub(crate) fn spawn(
    cmd: &mut cosca::Command,
    role: Role,
    reservation: &mut Reservation,
) -> Result<Spawned, SpawnError> {
    let prepared = (|| {
        let streams = (0..STREAMS)
            .map(|_| Ready::for_role(&role, reservation))
            .collect::<Result<Vec<_>, _>>()?;
        let Ok([stdout, stderr]) = <[Ready; STREAMS]>::try_from(streams) else {
            unreachable!("STREAMS is the number of streams a child has");
        };
        cmd.stdout(stdout.stdio()?)?;
        cmd.stderr(stderr.stdio()?)?;
        Ok((stdout, stderr))
    })();
    let (stdout, stderr) = prepared.map_err(SpawnError::NotRun)?;
    let mut child = start(cmd).map_err(SpawnError::of_spawn)?;
    let (out, err) = (child.stdout(), child.stderr());
    Ok(Spawned {
        child,
        role,
        stdout: stdout.spawned(out),
        stderr: stderr.spawned(err),
    })
}

/// The spawn itself: [`spawn`]'s last step, once nothing more can fail for want of what its output
/// needs.
fn start(cmd: &mut cosca::Command) -> Result<cosca::Child, cosca::error::Error> {
    #[cfg(test)]
    if let Some(e) = test_hook::spawning() {
        return Err(e);
    }
    cmd.spawn().map_err(release_unreaped)
}

/// The failure to make `what` before a spawn, which is therefore never attempted.
fn not_run(what: &'static str) -> impl Fn(io::Error) -> cosca::error::Error {
    move |e| cosca::error::Error::Io(within(format!("no {what} could be made, so it was not run"), e))
}

/// How [`wait_bounded`] lost its child, and whether the child had announced first.
#[derive(Debug)]
pub(crate) struct Lost {
    pub error: cosca::error::Error,
    /// A [`Role::AnnouncedRequest`] had said the manager has its request, so the request reached
    /// it. `false` establishes nothing.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub announced: bool,
}

/// Block until the child exits or `deadline` expires, under its [`Role`]'s rule for what an expiry
/// does: kill the child, tear its tree down and reap it ([`Finished::Expired`]) — or, for
/// [`Role::Request`], leave it running and hand it back as [`Finished::Left`], which releases it to
/// its [`Reaper`] unless the caller kills it. Nothing here makes a thread or a file: [`spawn`] made
/// them all.
pub(crate) fn wait_bounded(spawned: Spawned, deadline: Deadline) -> Result<Finished, Lost> {
    let Spawned {
        child,
        role,
        stdout,
        stderr,
    } = spawned;

    // Unbounded, deliberately: this is the request going out, not the wait for its answer. A child
    // that ends without announcing ends here too, once its stderr reaches EOF.
    let mut heard = Vec::new();
    if let (Role::AnnouncedRequest(issued), Stream::Heard(rx)) = (&role, &stderr) {
        while !issued(&heard) {
            match rx.recv() {
                Ok(chunk) => absorb(chunk, &mut heard, "stderr").map_err(|e| Lost {
                    error: cosca::error::Error::Io(e),
                    announced: false,
                })?,
                Err(RecvError) => break, // EOF
            }
        }
    }
    let announced = matches!(&role, Role::AnnouncedRequest(issued) if issued(&heard));
    waited_out(child, role, stdout, stderr, heard, deadline).map_err(|error| Lost { error, announced })
}

/// [`wait_bounded`] once the request is out: the wait for its answer, and the output it wrote.
fn waited_out(
    child: cosca::Child,
    role: Role,
    stdout: Stream,
    stderr: Stream,
    heard: Vec<u8>,
    deadline: Deadline,
) -> Result<Finished, cosca::error::Error> {
    #[cfg(test)]
    if let Some(e) = test_hook::waiting() {
        return Err(e);
    }
    let waited = match deadline.at() {
        Some(at) => child.wait_deadline(at)?,
        None => Some(child.wait()?), // no timeout value is invented for an unbounded deadline
    };
    let status = match (waited, role) {
        (Some(status), _) => status,
        (None, Role::Request(reaper)) => {
            // Left to the caller: see `Left`. Its output files stay open in it, and a file never
            // breaks under its writer.
            return Ok(Finished::Left(Left::new(move |fate| {
                if fate == Fate::Kill
                    && let Err(e) = child.kill()
                {
                    eprintln!(
                        "warning: could not kill a request left running, which runs on: {}",
                        cosca::error::Report::new(&e)
                    );
                }
                reaper.reap(child);
            })));
        }
        (None, Role::Query | Role::UncountedQuery | Role::AnnouncedRequest(_)) => {
            // The root first, and its `Ok` is what makes the reap below unable to block.
            child.kill()?;
            // Then the tree, best-effort. This runs BEFORE the reap below, while the killed
            // root is still a zombie pinning its pid and pgid, so a kill that names the tree
            // by the root's number cannot hit another process on any platform. `Drop` (at the
            // end of this function) runs after the reap, where cosca may skip such a kill. An
            // uncontained child (the descendant test spawns one) reports `Unsupported` here, and a
            // contained tree whose member refused SIGKILL reports `Containment`; the `Result` is
            // discarded either way, so nothing surfaces it, and a descendant still holding a pipe
            // is exactly what `collect` reports as `complete == false`.
            let _tree_teardown = child.send_kill_tree();
            let reaped = child.wait()?;
            match expiry_disposition(reaped) {
                Disposition::WeKilledIt => return Ok(Finished::Expired),
                Disposition::ItExitedOnItsOwn => reaped,
            }
        }
    };
    // Drop before reading: `kill_on_drop` tears the contained tree down, so on the
    // natural-exit path too, nothing this call spawned outlives it holding a write end.
    drop(child);

    let (stdout, stdout_whole) = read(stdout, Vec::new(), "stdout", deadline)?;
    let (stderr, stderr_whole) = read(stderr, heard, "stderr", deadline)?;
    let capture = Capture {
        stdout,
        stderr,
        complete: stdout_whole && stderr_whole,
    };
    Ok(Finished::Exited { status, capture })
}

/// The rest of `stream`, after `heard`: a file read back whole, or a pipe collected until its EOF or
/// `deadline`. `(bytes, whole)`.
fn read(
    stream: Stream,
    mut heard: Vec<u8>,
    name: &str,
    deadline: Deadline,
) -> Result<(Vec<u8>, bool), cosca::error::Error> {
    match stream {
        Stream::File(file) => Ok((read_back(file, name)?, true)),
        Stream::Heard(rx) => {
            let (rest, whole) = collect(rx, deadline, name).map_err(cosca::error::Error::Io)?;
            heard.extend(rest);
            Ok((heard, whole))
        }
    }
}

/// Everything written to `file`, a stream's temp file, read from its start: the child wrote through
/// a descriptor sharing its offset.
fn read_back(mut file: File, stream: &str) -> Result<Vec<u8>, cosca::error::Error> {
    let mut bytes = Vec::new();
    file.rewind()
        .and_then(|()| file.read_to_end(&mut bytes))
        .map_err(|e| cosca::error::Error::Io(within(stream, e)))?;
    Ok(bytes)
}

// drain / collect =====================================================================================================

#[derive(Debug)]
enum Chunk {
    Bytes(Vec<u8>),
    /// The reader stopped early: a read error, or a panic caught at the thread
    /// boundary. Never silence.
    Failed(std::io::Error),
}

/// Read `src` to EOF, sending each chunk. A read error, or a panic inside
/// `src`, sends `Chunk::Failed` instead of ending the stream silently.
/// Returns when `src` is exhausted, fails, or the receiver is gone.
fn drain<R: Read>(mut src: R, tx: Sender<Chunk>) {
    // Kept outside the `catch_unwind`'d closure so a caught panic can still
    // report through it after the closure that moved `tx` is gone.
    let failure_tx = tx.clone();
    let outcome = panic::catch_unwind(AssertUnwindSafe(move || {
        let mut buf = [0u8; 8192];
        loop {
            match src.read(&mut buf) {
                Ok(0) => return None, // EOF: a normal end of stream
                Ok(n) => {
                    if tx.send(Chunk::Bytes(buf[..n].to_vec())).is_err() {
                        return None; // the receiver is gone; nothing more to do
                    }
                }
                Err(e) => return Some(e),
            }
        }
    }));
    match outcome {
        Ok(Some(e)) => {
            let _ = failure_tx.send(Chunk::Failed(e));
        }
        Ok(None) => {}
        Err(_) => {
            let _ = failure_tx.send(Chunk::Failed(std::io::Error::other("panicked while reading")));
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

/// Collect until the reader is finished (its sender dropped) or `deadline`
/// expires. `Ok((bytes, complete))`; `Err` iff a `Chunk::Failed`
/// arrived. Always drains what is already queued before consulting the
/// deadline, so an expiry never discards bytes that had already arrived —
/// but once expired, drains exactly what `Receiver::len()` snapshots as
/// already queued and stops there, so a reader that keeps producing past the
/// deadline cannot keep this loop fed forever (`recv_timeout(ZERO)` returns
/// a queued item rather than timing out, which is why that snapshot, not a
/// zero-duration `recv_timeout`, is what expiry drains through).
fn collect(rx: impl ChunkSource, deadline: Deadline, stream: &str) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();

    loop {
        match deadline.remaining() {
            None => match rx.recv() {
                Ok(chunk) => absorb(chunk, &mut bytes, stream)?,
                Err(_) => return Ok((bytes, true)), // the sender dropped
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
                    absorb(chunk, &mut bytes, stream)?;
                }
                return Ok((bytes, false));
            }
            Some(remaining) => match rx.recv_timeout(remaining) {
                Ok(chunk) => absorb(chunk, &mut bytes, stream)?,
                Err(RecvTimeoutError::Disconnected) => return Ok((bytes, true)),
                Err(RecvTimeoutError::Timeout) => {} // re-check: the deadline has now expired
            },
        }
    }
}

/// A reader's failure names its `stream`: "a reader failed" and "the stderr reader failed" are
/// different diagnostics.
fn absorb(chunk: Chunk, bytes: &mut Vec<u8>, stream: &str) -> std::io::Result<()> {
    match chunk {
        Chunk::Bytes(chunk) => bytes.extend_from_slice(&chunk),
        Chunk::Failed(e) => return Err(within(stream, e)),
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
