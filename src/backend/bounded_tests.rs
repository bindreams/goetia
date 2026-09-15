use std::io::{self, BufRead, ErrorKind, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::*;
use crate::manager::budget::Budget;

// wait_bounded, through `command` =====================================================================================

#[skuld::test]
fn a_child_that_exits_is_reported_with_its_output() {
    let child = command("/bin/echo", &["hi"]).unwrap().spawn().unwrap();
    let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { status, capture } => {
            assert!(status.success());
            assert_eq!(capture.stdout, b"hi\n");
            assert!(capture.complete);
        }
        Finished::Expired { .. } => panic!("expected Exited, got Expired"),
    }
}

#[skuld::test]
fn a_nonzero_exit_is_reported_not_an_error() {
    let child = command("/bin/sh", &["-c", "echo boom >&2; exit 3"])
        .unwrap()
        .spawn()
        .unwrap();
    let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { status, capture } => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(capture.stderr, b"boom\n");
            assert!(capture.complete);
        }
        Finished::Expired { .. } => panic!("expected Exited, got Expired"),
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
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired { .. }));

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
    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();
    assert!(matches!(finished, Finished::Expired { .. }));
}

/// Smoke test, not a truncation guard: `echo late` is a single write well
/// under `PIPE_BUF`, so the child can never yield a genuine split prefix,
/// and this assertion holds under any implementation of `collect`. The real
/// guard against a truncated prefix is
/// `collect_keeps_what_already_arrived_when_the_deadline_expires`.
#[skuld::test]
fn an_expiry_path_smoke_test() {
    let child = command("/bin/sh", &["-c", "echo late; exit 0"])
        .unwrap()
        .spawn()
        .unwrap();
    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();
    let capture = match finished {
        Finished::Exited { capture, .. } => capture,
        Finished::Expired { capture } => capture,
    };
    // `complete` asserts "nothing further can arrive", not "something
    // arrived": an empty-and-complete capture is correct if the spent
    // deadline killed the child before it wrote anything.
    assert!(!capture.complete || capture.stdout.is_empty() || capture.stdout == b"late\n");
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
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired { .. }));

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

    let fd3 = child.fd_read_end(3.into()).unwrap();
    let mut line = String::new();
    io::BufReader::new(fd3).read_line(&mut line).unwrap();
    let descendant: u32 = line.trim().parse().unwrap();

    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired { .. }));

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
}

#[skuld::test]
fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
    let child = command("/bin/sh", &["-c", "yes | head -c 200000"])
        .unwrap()
        .spawn()
        .unwrap();
    let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { capture, .. } => {
            assert_eq!(capture.stdout.len(), 200_000);
            assert!(capture.complete);
        }
        Finished::Expired { .. } => panic!("expected Exited, got Expired"),
    }
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

#[skuld::test]
fn collect_stops_at_the_deadline_even_while_a_reader_keeps_producing() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    // A producer that never stops on its own, and that has already queued a
    // large backlog by the time `collect` is called: the queue is never
    // empty, which is exactly the condition that makes today's `collect`
    // loop forever. No timing is involved anywhere — `ready_rx.recv()` is a
    // real event wait that unblocks the instant the backlog's last send
    // happens, not a poll against a chosen duration.
    const PREFILL: usize = 200_000;
    let keep_sending = Arc::new(AtomicBool::new(true));
    let keep_sending_producer = Arc::clone(&keep_sending);
    let producer = std::thread::spawn(move || {
        let mut sent = 0usize;
        while keep_sending_producer.load(Ordering::Relaxed) {
            if tx.send(Chunk::Bytes(Which::Stdout, vec![0u8; 8])).is_err() {
                break;
            }
            sent += 1;
            if sent == PREFILL {
                let _ = ready_tx.send(());
            }
        }
    });
    ready_rx.recv().unwrap();

    let (_stdout, _stderr, complete) = collect(rx, Budget::Immediate.start()).unwrap();
    assert!(!complete);

    keep_sending.store(false, Ordering::Relaxed);
    producer.join().unwrap();
}
