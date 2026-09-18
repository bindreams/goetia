use std::io::{self, BufRead, ErrorKind, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use super::*;
use crate::manager::budget::Budget;

/// `child`, already spawned, under `role`.
fn armed(child: cosca::Child, role: Role) -> Spawned {
    Spawned { child, role }
}

// wait_bounded, through `command` =====================================================================================

#[skuld::test]
fn a_child_that_exits_is_reported_with_its_output() {
    let child = command("/bin/echo", &["hi"]).unwrap().spawn().unwrap();
    let finished = wait_bounded(armed(child, Role::Query), Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { status, capture } => {
            assert!(status.success());
            assert_eq!(capture.stdout, b"hi\n");
            assert!(capture.complete);
        }
        Finished::Expired => panic!("expected Exited, got Expired"),
    }
}

#[skuld::test]
fn a_nonzero_exit_is_reported_not_an_error() {
    let child = command("/bin/sh", &["-c", "echo boom >&2; exit 3"])
        .unwrap()
        .spawn()
        .unwrap();
    let finished = wait_bounded(armed(child, Role::Query), Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { status, capture } => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(capture.stderr, b"boom\n");
            assert!(capture.complete);
        }
        Finished::Expired => panic!("expected Exited, got Expired"),
    }
}

#[skuld::test]
fn an_expired_deadline_kills_and_reaps_the_child() {
    let child = command("sleep", &["2147483647"]).unwrap().spawn().unwrap();
    let pid = child.id().pid();

    // `sleep 2147483647` cannot exit on its own for 68 years, so the deadline
    // is provably the only way out and no budget value can make this flaky;
    // 50ms is chosen only to exercise a real block rather than the
    // already-expired path.
    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(armed(child, Role::Query), deadline).unwrap();
    assert!(matches!(finished, Finished::Expired));

    // A zombie would still answer `kill(pid, 0)` successfully; `ESRCH` is
    // what proves the reap, not merely the kill.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let err = io::Error::last_os_error();
    assert_eq!(rc, -1);
    assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
}

#[skuld::test]
fn an_already_expired_deadline_does_not_wait() {
    let child = command("sleep", &["2147483647"]).unwrap().spawn().unwrap();
    let finished = wait_bounded(armed(child, Role::Query), Budget::Immediate.start()).unwrap();
    assert!(matches!(finished, Finished::Expired));
}

/// The expiry path with output already in flight — the case the two
/// sleep-only expiry tests above leave uncovered, and the one this test has
/// always been about.
///
/// Deterministic by construction: the child writes and then `exec`s a
/// 68-year sleep, so it cannot exit on its own and the deadline is provably
/// the only way out. `Expired` holds whether or not the write landed before
/// the kill, so no part of this is a race. The previous `echo late; exit 0`
/// child could not be asserted on at all — a child that exits at once may
/// legitimately beat the kill and answer `Exited` — which is how this test
/// drifted into asserting nothing that could fail.
///
/// Nothing is claimed about the bytes: an expiry carries no capture. The
/// truncation rule is
/// `collect_keeps_what_already_arrived_when_the_deadline_expires`.
#[skuld::test]
fn an_expiry_with_output_in_flight_is_reported_as_expired() {
    let child = command("/bin/sh", &["-c", "echo late; exec sleep 2147483647"])
        .unwrap()
        .spawn()
        .unwrap();
    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(armed(child, Role::Query), deadline).unwrap();
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
}

#[skuld::test]
fn a_child_whose_descendant_holds_the_pipe_still_returns_on_expiry() {
    // Deliberately built WITHOUT `command()`'s containment: a contained
    // descendant is killed, which is the other test below. `exec` makes the
    // second `sleep` *be* the direct child, so the kill lands on it and not
    // on `sh`; the backgrounded first `sleep` inherits stdout and is not
    // killed, so its write end stays open for 68 years.
    let mut cmd = cosca::run(["/bin/sh", "-c", "sleep 2147483647 & echo $! >&3; exec sleep 2147483647"]);
    cmd.stdin(cosca::Stdio::null()).unwrap();
    cmd.stdout(cosca::Stdio::pipe_out()).unwrap();
    cmd.stderr(cosca::Stdio::pipe_out()).unwrap();
    cmd.fd(3, cosca::Stdio::pipe_out()).unwrap();
    let mut child = cmd.spawn().unwrap();

    // A blocking read that ends when the shell writes, so the descendant's
    // pid is learned without racing the capture for it.
    let fd3 = child.fd_read_end(3.into()).unwrap();
    let mut line = String::new();
    io::BufReader::new(fd3).read_line(&mut line).unwrap();
    let descendant: libc::pid_t = line.trim().parse().unwrap();

    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(armed(child, Role::Query), deadline).unwrap();
    assert!(matches!(finished, Finished::Expired));

    // Leave nothing behind.
    unsafe {
        libc::kill(descendant, libc::SIGKILL);
    }
}

#[skuld::test]
fn a_contained_childs_descendant_dies_with_it() {
    // Same script, same fd-3 handshake, but spawned through `bounded::command`
    // so the tree IS contained.
    let mut cmd = command(
        "/bin/sh",
        &["-c", "sleep 2147483647 & echo $! >&3; exec sleep 2147483647"],
    )
    .unwrap();
    cmd.fd(3, cosca::Stdio::pipe_out()).unwrap();
    let mut child = cmd.spawn().unwrap();
    #[cfg(target_os = "linux")]
    let leaf = contained_leaf(child.id().pid());

    let fd3 = child.fd_read_end(3.into()).unwrap();
    let mut line = String::new();
    io::BufReader::new(fd3).read_line(&mut line).unwrap();
    let descendant: u32 = line.trim().parse().unwrap();

    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(armed(child, Role::Query), deadline).unwrap();
    assert!(matches!(finished, Finished::Expired));

    // A real event-driven death-watch (pidfd on Linux, EVFILT_PROC |
    // NOTE_EXIT on macOS), with no timeout to choose: it returns when the
    // descendant dies, and hangs if the descendant is not in the child's
    // contained tree (e.g. `command()` without `.contain()`) — then neither
    // `kill_tree` nor `Drop` can reach it. `capture.complete` is
    // deliberately not asserted here — the kill is asynchronous and
    // `collect` runs under an already-spent deadline, so whether the
    // readers observe EOF within that instant varies run to run.
    match cosca::Process::from_pid(descendant) {
        cosca::identity::Resolved::Found(p) => p.wait().unwrap(),
        cosca::identity::Resolved::Gone => {}
        cosca::identity::Resolved::Unknown => panic!("could not resolve the descendant's identity"),
    }
    #[cfg(target_os = "linux")]
    remove_drained_leaf(leaf);
}

/// The cgroup leaf cosca placed `pid` in, or `None` where containment fell back to a process
/// group — whose cgroup is this test process's own, and must not be touched.
#[cfg(target_os = "linux")]
fn contained_leaf(pid: u32) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let leaf = std::path::Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    leaf.file_name()?.to_str()?.starts_with("cosca-").then_some(leaf)
}

/// Remove a leaf every member of which has exited. cosca's own `Drop` removes it when the killed
/// tree has already drained by then, and leaves it behind when a member is still dying — half the
/// runs of the test above, measured — so the test removes what it made.
#[cfg(target_os = "linux")]
fn remove_drained_leaf(leaf: Option<std::path::PathBuf>) {
    let Some(leaf) = leaf else { return };
    match std::fs::remove_dir(&leaf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => panic!("remove the drained leaf {}: {e}", leaf.display()),
    }
}

#[skuld::test]
fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
    let child = command("/bin/sh", &["-c", "yes | head -c 200000"])
        .unwrap()
        .spawn()
        .unwrap();
    let finished = wait_bounded(armed(child, Role::Query), Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { capture, .. } => {
            assert_eq!(capture.stdout.len(), 200_000);
            assert!(capture.complete);
        }
        Finished::Expired => panic!("expected Exited, got Expired"),
    }
}

// Role ================================================================================================================

/// What [`announced`] reports issued: the marker the scripts below write once their "request" is out.
const ISSUED: &[u8] = b"request issued";

fn announced(stderr: &[u8]) -> bool {
    stderr.windows(ISSUED.len()).any(|w| w == ISSUED)
}

/// The budget never decides whether a request is sent. The child's "request" is the file it creates
/// before announcing; the deadline is spent before the child even starts, so a wait that applied
/// the deadline from the spawn would kill it first. The child cannot exit on its own afterwards, so
/// `Expired` is the only answer and no timing is bet on.
#[skuld::test]
fn an_announced_request_is_issued_whatever_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    let child = command(
        "/bin/sh",
        &[
            "-c",
            "touch \"$0\"; echo 'request issued' >&2; exec sleep 2147483647",
            request.to_str().unwrap(),
        ],
    )
    .unwrap()
    .spawn()
    .unwrap();

    let finished = wait_bounded(
        armed(child, Role::AnnouncedRequest(announced)),
        Budget::Immediate.start(),
    )
    .unwrap();

    assert!(matches!(finished, Finished::Expired), "{finished:?}");
    assert!(request.exists(), "the request must be out before the deadline applies");
}

/// A child that ends without announcing — `systemctl` refusing a unit it cannot load — is reported
/// with its own exit, not as an expiry. Its output reaching EOF means it is already exiting, so the
/// kill a spent deadline sends cannot overwrite that exit.
#[skuld::test]
fn an_announced_request_that_ends_unannounced_is_reported_exited() {
    let child = command("/bin/sh", &["-c", "echo refused >&2; exit 3"])
        .unwrap()
        .spawn()
        .unwrap();

    let finished = wait_bounded(
        armed(child, Role::AnnouncedRequest(announced)),
        Budget::Immediate.start(),
    )
    .unwrap();

    match finished {
        Finished::Exited { status, capture } => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(capture.stderr, b"refused\n");
        }
        Finished::Expired => panic!("expected Exited, got Expired"),
    }
}

/// What the child wrote while issuing its request is kept: it is the start of the diagnostic.
#[skuld::test]
fn what_an_announced_request_wrote_before_announcing_is_kept() {
    let child = command("/bin/sh", &["-c", "echo early >&2; echo 'request issued' >&2; exit 1"])
        .unwrap()
        .spawn()
        .unwrap();

    let finished = wait_bounded(
        armed(child, Role::AnnouncedRequest(announced)),
        Budget::Unbounded.start(),
    )
    .unwrap();

    match finished {
        Finished::Exited { capture, .. } => assert_eq!(capture.stderr, b"early\nrequest issued\n"),
        Finished::Expired => panic!("expected Exited, got Expired"),
    }
}

/// A request goetia cannot see arrive is never killed on expiry — the kill could land before the
/// manager has the request — and its reaper reaps it once it exits, so a long-lived caller is left
/// no zombie per expired request.
///
/// The child exits `7` at EOF on its stdin, and the test's write end reaches the reaper along with
/// the child: the reaper's `wait` drops it, so the child can exit only once it is the reaper's and
/// the reaper is waiting. Whatever killed it before then — the expiry, or the reaper itself — shows
/// as a signal instead of `7`, with no race. A child nobody reaps reports nothing: the reaper's
/// sender is dropped unsent, which `recv` returns as an error.
#[skuld::test]
fn an_unannounced_request_is_left_running_on_expiry_and_reaped_once_it_exits() {
    let (writer_tx, writer_rx) = mpsc::channel::<io::PipeWriter>();
    let (tx, rx) = mpsc::channel();
    let reaper = Reaper::with(
        move |child| {
            drop(writer_rx.recv());
            child.wait()
        },
        move |status| tx.send(status).expect("the test is still receiving"),
    )
    .unwrap();
    let mut spawned = spawn(&mut exits_7_at_eof(), Role::Request(reaper)).unwrap();
    writer_tx.send(spawned.child.stdin().expect("stdin is piped")).unwrap();

    let finished = wait_bounded(spawned, Budget::Immediate.start()).unwrap();

    assert!(matches!(finished, Finished::Expired), "{finished:?}");
    let status = rx
        .recv()
        .expect("a request left running must be reaped once it exits")
        .expect("waiting on the child");
    assert_eq!(
        status.code(),
        Some(7),
        "nothing may kill a request it cannot see arrive: {status:?}"
    );
}

/// A verb's reapers are all made before any is handed out, so a verb whose third request could have
/// none never sends its first.
#[skuld::test]
fn reapers_are_all_made_or_none_are() {
    let _only_two = test_hook::allow(2);

    let made = reapers::<3>();

    let e = made.expect_err("a third reaper could not be made");
    assert!(e.to_string().contains("no reaper may be made"), "{e}");
}

/// A sequence's spares are taken before anything is made: its later verbs need no new thread, so
/// cannot fail for want of one after an earlier verb has sent something.
#[skuld::test]
fn spares_are_taken_before_any_reaper_is_made() {
    let spares = spare(3).unwrap();
    let _none = test_hook::allow(0);

    assert!(reapers::<1>().is_ok(), "the stop's reaper is a spare");
    assert!(reapers::<2>().is_ok(), "the start's reapers are spares");
    assert!(reapers::<1>().is_err(), "the spares are spent, and nothing may be made");
    drop(spares);
}

/// Dropping the guard drops what it left: nothing lingers for a later, unrelated verb.
#[skuld::test]
fn spares_are_released_with_their_guard() {
    drop(spare(2).unwrap());
    let _none = test_hook::allow(0);

    assert!(reapers::<1>().is_err(), "a released spare was still handed out");
}

/// A reaper whose request exited inside its budget has nothing to reap, and its thread ends: the
/// thread's closures are dropped with it, and the channel one of them held reports that.
#[skuld::test]
fn a_reaper_with_nothing_to_reap_ends() {
    let (tx, rx) = mpsc::channel::<()>();
    let reaper = Reaper::with(cosca::Child::wait, move |_| drop(tx)).unwrap();

    drop(reaper);

    assert!(rx.recv().is_err(), "the reaper reaped something it was never handed");
}

/// The child the reaper test hands its reaper: exits `7` at EOF on its stdin, a pipe the test holds.
/// `command()`'s, as launchd's `launchctl` — the one production `Role::Request` — is spawned.
#[cfg(not(target_os = "linux"))]
fn exits_7_at_eof() -> cosca::Command {
    let mut cmd = command("/bin/sh", &["-c", "read line; exit 7"]).unwrap();
    cmd.stdin(cosca::Stdio::pipe_in()).unwrap();
    cmd
}

/// On Linux, uncontained. cosca 0.4's cgroup containment kills a detached tree — the leaf's `Drop`
/// writes `cgroup.kill` — and leaves the leaf behind, measured; nothing on Linux detaches, since
/// `systemctl` announces its requests.
#[cfg(target_os = "linux")]
fn exits_7_at_eof() -> cosca::Command {
    let mut cmd = cosca::run(["/bin/sh", "-c", "read line; exit 7"]);
    cmd.stdin(cosca::Stdio::pipe_in()).unwrap();
    cmd.stdout(cosca::Stdio::pipe_out()).unwrap();
    cmd.stderr(cosca::Stdio::pipe_out()).unwrap();
    cmd
}

// expiry_disposition ==================================================================================================

#[skuld::test]
fn a_child_that_exits_in_the_kill_window_is_reported_not_expired() {
    assert_eq!(
        expiry_disposition(ExitStatus::from_raw(libc::SIGKILL)),
        Disposition::WeKilledIt
    );
    assert_eq!(
        expiry_disposition(ExitStatus::from_raw(7 << 8)),
        Disposition::ItExitedOnItsOwn
    );
}

// drain ===============================================================================================================

struct ErrorOnFirstRead;

impl Read for ErrorOnFirstRead {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::from(ErrorKind::BrokenPipe))
    }
}

#[skuld::test]
fn a_reader_that_errors_reports_the_error_instead_of_ending_silently() {
    let (tx, rx) = crossbeam_channel::unbounded();
    drain(ErrorOnFirstRead, Which::Stdout, tx);
    match rx.recv().unwrap() {
        Chunk::Failed(Which::Stdout, e) => assert_eq!(e.kind(), ErrorKind::BrokenPipe),
        other => panic!("expected Chunk::Failed, got {other:?}"),
    }
}

struct PanicOnRead;

impl Read for PanicOnRead {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        panic!("boom");
    }
}

#[skuld::test]
fn a_reader_that_panics_reports_it_instead_of_ending_silently() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let handle = std::thread::spawn(move || drain(PanicOnRead, Which::Stdout, tx));
    match rx.recv().unwrap() {
        Chunk::Failed(Which::Stdout, _) => {}
        other => panic!("expected Chunk::Failed, got {other:?}"),
    }
    assert!(handle.join().is_ok());
}

// collect =============================================================================================================

#[skuld::test]
fn collect_turns_a_reader_failure_into_an_error() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(Which::Stdout, b"partial".to_vec())).unwrap();
    tx.send(Chunk::Failed(Which::Stderr, io::Error::from(ErrorKind::BrokenPipe)))
        .unwrap();
    drop(tx);

    assert!(collect(rx, Budget::Unbounded.start()).is_err());
}

#[skuld::test]
fn collect_keeps_what_already_arrived_when_the_deadline_expires() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(Which::Stdout, b"partial".to_vec())).unwrap();
    // `tx` kept alive deliberately: nothing but the deadline can end this
    // collect, pinning the "expiry, not disconnect" path without a real
    // child.
    let (stdout, _stderr, complete) = collect(rx, Budget::Immediate.start()).unwrap();
    assert_eq!(stdout, b"partial");
    assert!(!complete);
    drop(tx);
}

#[skuld::test]
fn a_complete_drain_reports_complete() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(Which::Stdout, b"all of it".to_vec())).unwrap();
    drop(tx);

    let (stdout, _stderr, complete) = collect(rx, Budget::Unbounded.start()).unwrap();
    assert_eq!(stdout, b"all of it");
    assert!(complete);
}

/// A channel that never runs dry: every receive yields a chunk at once, and
/// `len()` always reports [`QUEUED`] of them.
struct Bottomless;

const QUEUED: usize = 3;

impl ChunkSource for Bottomless {
    fn len(&self) -> usize {
        QUEUED
    }

    fn try_recv(&self) -> Result<Chunk, TryRecvError> {
        Ok(Chunk::Bytes(Which::Stdout, b"x".to_vec()))
    }

    fn recv(&self) -> Result<Chunk, RecvError> {
        Ok(Chunk::Bytes(Which::Stdout, b"x".to_vec()))
    }

    fn recv_timeout(&self, _timeout: Duration) -> Result<Chunk, RecvTimeoutError> {
        Ok(Chunk::Bytes(Which::Stdout, b"x".to_vec()))
    }
}

#[skuld::test]
fn collect_stops_at_the_deadline_even_while_a_reader_keeps_producing() {
    // A `collect` that drains until the channel is empty never returns here.
    let (stdout, _stderr, complete) = collect(Bottomless, Budget::Immediate.start()).unwrap();
    assert_eq!(stdout, b"x".repeat(QUEUED), "exactly the chunks queued at expiry");
    assert!(!complete);
}
