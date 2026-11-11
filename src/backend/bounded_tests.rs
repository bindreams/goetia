use std::error::Error as _;
use std::io::{self, BufRead, ErrorKind, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::backend::standby;
use crate::backend::test_allowance;
use crate::manager::budget::Budget;

/// A reservation holding the temp files one child's streams draw.
fn files() -> Reservation {
    Reservation::make(0, STREAMS).unwrap()
}

/// `cmd`, spawned under `role`, handed what its streams draw.
fn spawned(mut cmd: cosca::Command, role: Role) -> Spawned {
    let mut reservation = Reservation::for_role(&role);
    spawn(&mut cmd, role, &mut reservation).unwrap()
}

/// `/bin/sh -c <script>`, as [`command`] builds it.
fn sh(script: &str) -> cosca::Command {
    command("/bin/sh", &["-c", script]).unwrap()
}

// wait_bounded, through `command` =====================================================================================

#[skuld::test]
fn a_child_that_exits_is_reported_with_its_output() {
    let child = spawned(command("/bin/echo", &["hi"]).unwrap(), Role::Query);
    let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { status, capture } => {
            assert!(status.success());
            assert_eq!(capture.stdout, b"hi\n");
            assert!(capture.complete);
        }
        Finished::Expired | Finished::Left(_) => panic!("expected Exited, got an expiry"),
    }
}

#[skuld::test]
fn a_nonzero_exit_is_reported_not_an_error() {
    let child = spawned(sh("echo boom >&2; exit 3"), Role::Query);
    let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();
    match finished {
        Finished::Exited { status, capture } => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(capture.stderr, b"boom\n");
            assert!(capture.complete);
        }
        Finished::Expired | Finished::Left(_) => panic!("expected Exited, got an expiry"),
    }
}

#[skuld::test]
fn an_expired_deadline_kills_and_reaps_the_child() {
    let child = spawned(command("sleep", &["2147483647"]).unwrap(), Role::Query);
    let pid = child.child.pid();

    // `sleep 2147483647` cannot exit on its own for 68 years, so the deadline
    // is provably the only way out and no budget value can make this flaky;
    // 50ms is chosen only to exercise a real block rather than the
    // already-expired path.
    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(child, deadline).unwrap();
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
    let child = spawned(command("sleep", &["2147483647"]).unwrap(), Role::Query);
    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();
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
    let child = spawned(sh("echo late; echo late >&2; exec sleep 2147483647"), Role::Query);
    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
}

/// The kill path, with a descendant the kill cannot reach holding both of an announced request's
/// pipes open: it returns `Expired` at the deadline, and reads neither stream. The read of a stream
/// a descendant holds is [`a_heard_stream_a_descendant_holds_is_read_only_until_the_deadline`]'s.
#[skuld::test]
fn a_child_whose_descendant_holds_the_pipe_still_returns_on_expiry() {
    // Deliberately built WITHOUT `command()`'s containment: a contained
    // descendant is killed, which is the other test below. `exec` makes the
    // second `sleep` *be* the direct child, so the kill lands on it and not
    // on `sh`; the backgrounded first `sleep` inherits stderr and is not
    // killed, so its write end stays open for 68 years.
    let mut cmd = cosca::run([
        "/bin/sh",
        "-c",
        "sleep 2147483647 & echo $! >&3; echo 'request issued' >&2; exec sleep 2147483647",
    ]);
    cmd.stdin(cosca::Stdio::null()).unwrap();
    cmd.fd(3, cosca::Stdio::pipe_out()).unwrap();
    let mut child = spawned(cmd, Role::AnnouncedRequest(announced));

    // A blocking read that ends when the shell writes, so the descendant's
    // pid is learned without racing the capture for it.
    let fd3 = child.child.fd_read_end(3.into()).unwrap();
    let mut line = String::new();
    io::BufReader::new(fd3).read_line(&mut line).unwrap();
    let descendant: libc::pid_t = line.trim().parse().unwrap();

    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired));

    // Leave nothing behind.
    unsafe {
        libc::kill(descendant, libc::SIGKILL);
    }
}

/// SIGKILLs a pid on drop, so a descendant a test let escape dies however the test ends.
struct KillOnDrop(libc::pid_t);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

/// A child that exits on its own while a descendant it let escape holds both its pipes: each heard
/// stream is read only until the deadline, and comes back marked not whole — never waited on for
/// an EOF that descendant would give in 68 years.
///
/// Deterministic, with no timing bet: the test reaps the child itself before the wait, so the wait
/// finds it exited whatever the deadline, and the deadline is spent before the wait begins, so the
/// read ends at once. A spent deadline alone marks the capture not whole
/// ([`a_heard_stream_is_read_only_until_a_spent_deadline_even_at_its_end`]), so what this test adds
/// is the descendant: once the read is over, the test asks it, over fd 4, and it answers on fd 3
/// that its stdout and stderr — the child's, which it inherited — are still pipes. A read that
/// ignored the deadline never ends here.
/// Built without `command()`'s containment, as the test above is: a contained descendant dies with
/// the child.
#[skuld::test]
fn a_heard_stream_a_descendant_holds_is_read_only_until_the_deadline() {
    let mut cmd = cosca::run([
        "/bin/sh",
        "-c",
        "( read asked <&4; \
           if [ -p /dev/fd/1 ] && [ -p /dev/fd/2 ]; then echo holds >&3; else echo free >&3; fi; \
           exec sleep 2147483647 ) & \
         echo $! >&3; echo 'request issued' >&2; exit 0",
    ]);
    cmd.stdin(cosca::Stdio::null()).unwrap();
    cmd.fd(3, cosca::Stdio::pipe_out()).unwrap();
    cmd.fd(4, cosca::Stdio::pipe_in()).unwrap();
    let mut child = spawned(cmd, Role::AnnouncedRequest(announced));

    let mut ask = child.child.fd_write_end(4.into()).unwrap();
    let mut answers = io::BufReader::new(child.child.fd_read_end(3.into()).unwrap());
    let mut line = String::new();
    answers.read_line(&mut line).unwrap();
    let _descendant = KillOnDrop(line.trim().parse().unwrap());
    let status = child.child.wait().unwrap();
    assert!(status.success(), "{status:?}");

    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();

    match finished {
        Finished::Exited { status, capture } => {
            assert!(status.success(), "{status:?}");
            assert!(!capture.complete, "the spent deadline ended the read: {capture:?}");
            assert!(announced(&capture.stderr), "{capture:?}");
        }
        Finished::Expired | Finished::Left(_) => panic!("the child exited on its own: expected Exited, got an expiry"),
    }
    ask.write_all(b"now\n").unwrap();
    let mut held = String::new();
    answers.read_line(&mut held).unwrap();
    assert_eq!(
        held.trim(),
        "holds",
        "the descendant held both of the child's pipes through the read"
    );
}

/// A heard stream is read only until the deadline, even at its end: once the deadline is spent,
/// what is queued is taken and nothing is waited for — the end of the stream included, since
/// waiting for it is what a descendant holding the pipe would make last forever. So the bytes are
/// kept, after what was heard, and not established whole. A read that ignored the deadline would
/// wait, find the sender gone, and call them whole.
#[skuld::test]
fn a_heard_stream_is_read_only_until_a_spent_deadline_even_at_its_end() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(b" said".to_vec())).unwrap();
    drop(tx);

    let (bytes, whole) = read(
        Stream::Heard(rx),
        b"heard".to_vec(),
        "stderr",
        Budget::Immediate.start(),
    )
    .unwrap();
    assert_eq!(bytes, b"heard said");
    assert!(!whole, "a spent deadline establishes no end");
}

#[skuld::test]
fn a_contained_childs_descendant_dies_with_it() {
    // Same script, same fd-3 handshake, but spawned through `bounded::command`
    // so the tree IS contained.
    let mut cmd = sh("sleep 2147483647 & echo $! >&3; exec sleep 2147483647");
    cmd.fd(3, cosca::Stdio::pipe_out()).unwrap();
    let mut child = spawned(cmd, Role::Query);

    let fd3 = child.child.fd_read_end(3.into()).unwrap();
    let mut line = String::new();
    io::BufReader::new(fd3).read_line(&mut line).unwrap();
    let descendant: u32 = line.trim().parse().unwrap();
    // Opened while it is alive: after the kill its pid may name another process.
    let descendant = cosca::Process::open_pid(descendant)
        .unwrap()
        .expect("the descendant is alive");

    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired));

    // A real event-driven death-watch (pidfd on Linux, EVFILT_PROC |
    // NOTE_EXIT on macOS), with no timeout to choose: it returns when the
    // descendant dies, and hangs if the descendant is not in the child's
    // contained tree (e.g. `command()` without `.contain()`) — then neither
    // `kill_tree` nor `Drop` can reach it.
    descendant.wait().unwrap();
}

/// More than a pipe holds, on every stream in every form it takes: a temp file, and the pipe
/// [`Role::AnnouncedRequest`] reads its stderr from as it is written.
#[skuld::test]
fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
    let script = "echo 'request issued' >&2; yes | head -c 200000; yes | head -c 200000 >&2";
    for role in [Role::Query, Role::AnnouncedRequest(announced)] {
        let child = spawned(sh(script), role);
        let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();
        match finished {
            Finished::Exited { capture, .. } => {
                assert_eq!(capture.stdout.len(), 200_000);
                assert_eq!(capture.stderr.len(), ISSUED.len() + 1 + 200_000);
                assert!(capture.complete);
            }
            Finished::Expired | Finished::Left(_) => panic!("expected Exited, got an expiry"),
        }
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
    let child = spawned(
        command(
            "/bin/sh",
            &[
                "-c",
                "touch \"$0\"; echo 'request issued' >&2; exec sleep 2147483647",
                request.to_str().unwrap(),
            ],
        )
        .unwrap(),
        Role::AnnouncedRequest(announced),
    );

    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();

    assert!(matches!(finished, Finished::Expired), "{finished:?}");
    assert!(request.exists(), "the request must be out before the deadline applies");
}

/// A wait lost after the announcement says the announcement came: that is what tells a request
/// that reached the manager from one that may not have. Lost before it — here the child ends
/// without announcing — or under a role that announces nothing, it says nothing.
#[skuld::test]
fn a_lost_wait_says_whether_the_request_was_announced() {
    for (script, role, expected) in [
        (
            "echo 'request issued' >&2; exec sleep 2147483647",
            Role::AnnouncedRequest(announced),
            true,
        ),
        ("exit 0", Role::AnnouncedRequest(announced), false),
        ("echo 'request issued' >&2; exit 0", Role::Query, false),
    ] {
        let child = spawned(sh(script), role);
        test_hook::wait_fails(|| io::Error::other("the wait failed").into());
        let lost = wait_bounded(child, Budget::Unbounded.start()).expect_err(script);
        assert_eq!(lost.announced, expected, "{script}");
        assert!(lost.error.to_string().contains("the wait failed"), "{script}: {lost:?}");
    }
}

/// A child that ends without announcing — `systemctl` refusing a unit it cannot load — is reported
/// with its own exit, not as an expiry. Its output reaching EOF means it is already exiting, so the
/// kill a spent deadline sends cannot overwrite that exit.
#[skuld::test]
fn an_announced_request_that_ends_unannounced_is_reported_exited() {
    let child = spawned(sh("echo refused >&2; exit 3"), Role::AnnouncedRequest(announced));

    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();

    match finished {
        Finished::Exited { status, capture } => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(capture.stderr, b"refused\n");
        }
        Finished::Expired | Finished::Left(_) => panic!("expected Exited, got an expiry"),
    }
}

/// What the child wrote while issuing its request is kept: it is the start of the diagnostic.
#[skuld::test]
fn what_an_announced_request_wrote_before_announcing_is_kept() {
    let child = spawned(
        sh("echo out; echo early >&2; echo 'request issued' >&2; exit 1"),
        Role::AnnouncedRequest(announced),
    );

    let finished = wait_bounded(child, Budget::Unbounded.start()).unwrap();

    match finished {
        Finished::Exited { capture, .. } => {
            assert_eq!(capture.stderr, b"early\nrequest issued\n");
            assert_eq!(capture.stdout, b"out\n");
        }
        Finished::Expired | Finished::Left(_) => panic!("expected Exited, got an expiry"),
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
    let status = expired_request_released_by_its_reaper("exit 7");
    assert_eq!(
        status.code(),
        Some(7),
        "nothing may kill a request it cannot see arrive: {status:?}"
    );
}

/// A request left running on expiry writes whatever it writes after goetia stopped waiting — and
/// after goetia is gone — and dies of none of it: its output goes to files, which never break under
/// a writer the way a pipe nobody reads does, with `SIGPIPE`. The child writes a megabyte to each
/// stream only once the test releases it, which is after the expiry, and more than any pipe holds;
/// a write to either that broke ends it with other than `0`.
#[skuld::test]
fn an_expired_request_that_writes_afterwards_is_not_killed_for_it() {
    let status =
        expired_request_released_by_its_reaper("head -c 1048576 /dev/zero >&2 && exec head -c 1048576 /dev/zero");
    assert_eq!(
        (status.code(), status.signal()),
        (Some(0), None),
        "a request left running must not die of its own output: {status:?}"
    );
}

/// The caller of an expired request may end the child it was left holding, and its reaper still
/// reaps it. The child would exit `7` at EOF, so only the kill ends it with `SIGKILL`.
#[skuld::test]
fn a_left_request_the_caller_kills_dies_of_it_and_is_reaped() {
    let status = expired_request_settled_by("exit 7", |finished| match finished {
        Finished::Left(left) => left.kill(),
        other => panic!("expected Left, got {other:?}"),
    });
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
}

/// Spawn `sh -c 'read line; <then>'` as a [`Role::Request`], let it expire at once, and return how
/// it ended as its reaper reports it. It reads EOF on its stdin, and so runs `then`, only once the
/// reaper holds it and is waiting — after the expiry.
fn expired_request_released_by_its_reaper(then: &str) -> ExitStatus {
    expired_request_settled_by(then, |finished| {
        assert!(matches!(finished, Finished::Left(_)), "{finished:?}");
    })
}

/// [`expired_request_released_by_its_reaper`], with what the caller does with the expiry given as
/// `settle`: it holds the child until `settle` returns, and a [`Left`] it drops is released.
fn expired_request_settled_by(then: &str, settle: impl FnOnce(Finished)) -> ExitStatus {
    let (writer_tx, writer_rx) = mpsc::channel::<cosca::PipeWriter>();
    let (tx, rx) = mpsc::channel();
    let reaper = Reaper::with(
        move |child| {
            drop(writer_rx.recv());
            child.wait()
        },
        move |status| tx.send(status).expect("the test is still receiving"),
    )
    .unwrap();
    let mut child = spawned(released_at_eof(then), Role::Request(reaper));
    writer_tx.send(child.child.stdin().expect("stdin is piped")).unwrap();

    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();

    settle(finished);
    rx.recv()
        .expect("a request left running must be reaped once it exits")
        .expect("waiting on the child")
}

/// The child the reaper tests hand their reaper: runs `then` at EOF on its stdin, a pipe the test
/// holds. `command()`'s, as launchd's `launchctl` — the one production `Role::Request` — is spawned.
fn released_at_eof(then: &str) -> cosca::Command {
    let mut cmd = sh(&format!("read line; {then}"));
    cmd.stdin(cosca::Stdio::pipe_in()).unwrap();
    cmd
}

// Nothing is made after the spawn =====================================================================================

/// `sh -c 'touch <marker>'`: a child whose "request" is the marker it creates, so whether it ran
/// is a file's existence.
fn marks(marker: &std::path::Path) -> cosca::Command {
    command("/bin/sh", &["-c", "touch \"$0\"", marker.to_str().unwrap()]).unwrap()
}

/// Whatever a role's output needs is made before the child is spawned, and a failure to make it
/// means the child never ran: never a panic, and never a request out with nothing to watch it.
/// An uncounted read's stdout and stderr files are made on the spot, and one that cannot be made
/// fails it before it runs. A counted read or request has its files, and an announced request its
/// threads, before it is spawned: see [`a_reservation_is_made_whole_or_not_at_all`].
#[skuld::test]
fn an_uncounted_query_whose_output_nothing_could_take_is_never_run() {
    for (case, allowance) in [("stdout", 0), ("stderr", 1)] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("request");
        let _short = test_hook::temp_files(allowance);
        let spawns = test_hook::spawns();

        let e = spawn(&mut marks(&marker), Role::UncountedQuery, &mut Reservation::default())
            .err()
            .expect(case);

        assert!(e.to_string().contains("no temp file "), "{case}: {e}");
        assert!(e.to_string().contains("so it was not run"), "{case}: {e}");
        assert_eq!(test_hook::spawns(), spawns, "{case}: the child was spawned");
        assert!(!marker.exists(), "{case}: the child ran");
    }
}

/// An announced request — `systemctl` — makes no temp file at all: it runs where none may be
/// writable. Its threads come from the reservation it is handed.
#[skuld::test]
fn an_announced_request_needs_no_temp_file() {
    let _no_file = test_hook::temp_files(0);
    let spawned = spawn(
        &mut sh("echo out; echo 'request issued' >&2"),
        Role::AnnouncedRequest(announced),
        &mut Reservation::of_listeners(STREAMS).unwrap(),
    )
    .unwrap();
    let finished = wait_bounded(spawned, Budget::Unbounded.start());
    match finished {
        Ok(Finished::Exited { capture, .. }) => {
            assert_eq!(capture.stdout, b"out\n");
            assert_eq!(capture.stderr, b"request issued\n");
            assert!(capture.complete);
        }
        other => panic!("expected Exited, got {other:?}"),
    }
}

/// Where a failed `cosca::error::Error::Io` is unwrapped in the two tests below.
fn io_of(e: cosca::error::Error) -> io::Error {
    match e {
        cosca::error::Error::Io(e) => e,
        other => panic!("expected Io, got {other:?}"),
    }
}

/// A failure to make a stream's file keeps the OS error's kind, says only that the child was not
/// run, and has the OS error as its source: `report` prints each once.
#[skuld::test]
fn a_failure_to_make_what_a_spawn_needs_says_it_was_not_run_and_keeps_its_cause() {
    let cause = io::Error::from_raw_os_error(libc::EACCES);
    let cause_text = cause.to_string();

    let e = io_of(not_run("temp file")(cause));

    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(e.to_string(), "no temp file could be made, so it was not run");
    assert_eq!(e.source().expect("the cause is the source").to_string(), cause_text);
    assert_eq!(
        crate::error::Error::spawn("cmd", e).report().to_string(),
        format!("failed to run `cmd`: no temp file could be made, so it was not run: {cause_text}")
    );
}

/// A temp file that cannot be read back fails as the stream it holds, with the OS error's kind and
/// the OS error as its source: `report` prints each once.
#[skuld::test]
fn a_stream_file_that_cannot_be_read_back_names_the_stream_and_keeps_its_cause() {
    let dir = tempfile::tempdir().unwrap();
    // Opened for writing only, so reading it fails with `EBADF`.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.path().join("stdout"))
        .unwrap();
    let cause = io::Error::from_raw_os_error(libc::EBADF);
    let cause_text = cause.to_string();

    let e = io_of(read_back(file, "stdout").unwrap_err());

    assert_eq!(e.kind(), cause.kind());
    assert_eq!(e.to_string(), "stdout");
    assert_eq!(e.source().expect("the cause is the source").to_string(), cause_text);
    assert_eq!(
        crate::error::Error::spawn("cmd", e).report().to_string(),
        format!("failed to run `cmd`: stdout: {cause_text}")
    );
}

/// A spawn that failed is placed before the child ran unless cosca's error says the program may have
/// started: [`cosca::error::Error::MayHaveStarted`], and no other variant, whatever its OS error.
/// Injected, so nothing runs.
#[skuld::test]
fn a_failed_spawn_is_not_run_unless_cosca_says_it_may_have_started() {
    type Case = (&'static str, fn() -> cosca::error::Error, bool);
    let cases: [Case; 8] = [
        (
            "fork: EAGAIN",
            || io::Error::from_raw_os_error(libc::EAGAIN).into(),
            false,
        ),
        (
            "exec: ENOENT",
            || io::Error::from_raw_os_error(libc::ENOENT).into(),
            false,
        ),
        // Whatever the OS error, only the variant decides.
        ("ECHILD", || io::Error::from_raw_os_error(libc::ECHILD).into(), false),
        ("EINVAL", || io::Error::from_raw_os_error(libc::EINVAL).into(), false),
        (
            "before the spawn: containment",
            || cosca::error::Error::Containment {
                detail: "forced".into(),
            },
            false,
        ),
        (
            "macOS: its unique id unreadable",
            || cosca::error::Error::Unassessable {
                detail: "the child could not read its own unique id".into(),
                source: None,
            },
            false,
        ),
        (
            "after exec: the identity read",
            || test_hook::may_have_started(io::Error::other("spawned child vanished").into()),
            true,
        ),
        (
            "after exec: the attach",
            || {
                test_hook::may_have_started(cosca::error::Error::Containment {
                    detail: "forced".into(),
                })
            },
            true,
        ),
    ];
    for (case, error, may_have_run) in cases {
        test_hook::spawn_fails(error);
        match spawn(&mut sh("true"), Role::Query, &mut files()) {
            Err(SpawnError::MayHaveRun(_)) => assert!(may_have_run, "{case}: placed after exec"),
            Err(SpawnError::NotRun(_)) => assert!(!may_have_run, "{case}: placed before exec"),
            Ok(_) => panic!("{case}: the injected failure was not returned"),
        }
    }
}

// Reservation =========================================================================================================

/// Whether `f` panics — the debug assertion every build of the test suites carries.
fn panics(f: impl FnOnce()) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
}

/// A reservation is the whole of what a child draws: its files are taken from the reservation it is
/// handed, so nothing need be made for them, and a spawn leaves what it did not take.
#[skuld::test]
fn a_child_takes_its_output_from_the_reservation_it_is_handed() {
    let mut reservation = Reservation::make(0, 3 * STREAMS).unwrap();
    let _no_file = test_hook::temp_files(0);

    for role in [Role::Query, Role::Query] {
        let mut cmd = sh("echo hi");
        let finished = wait_bounded(
            spawn(&mut cmd, role, &mut reservation).unwrap(),
            Budget::Unbounded.start(),
        );
        assert!(
            matches!(&finished, Ok(Finished::Exited { capture, .. }) if capture.stdout == b"hi\n"),
            "{finished:?}"
        );
    }

    assert_eq!(test_hook::left(&reservation), (0, STREAMS));
}

/// A reservation hands out what it made: each reaper, listener and file, with nothing made
/// meanwhile.
#[skuld::test]
fn a_reservation_hands_out_what_it_made() {
    let mut reservation = Reservation::make(2, 2).unwrap();
    let mut listening = Reservation::of_listeners(2).unwrap();
    let _no_thread = standby::test_hook::threads(0);
    let _no_file = test_hook::temp_files(0);

    drop((reservation.reaper().unwrap(), reservation.reaper().unwrap()));
    drop((reservation.file().unwrap(), reservation.file().unwrap()));
    drop((listening.listener().unwrap(), listening.listener().unwrap()));

    assert_eq!(test_hook::left(&reservation), (0, 0));
    assert_eq!(test_hook::listeners_left(&listening), 0);
}

/// A reservation is made whole or not at all, so a verb whose last request could have no reaper
/// never sends its first: `make` and `of_listeners` fail as a whole when any one thread or file
/// cannot be made.
#[skuld::test]
fn a_reservation_is_made_whole_or_not_at_all() {
    type Case = (
        &'static str,
        fn() -> test_allowance::Allowance,
        fn() -> io::Result<Reservation>,
        &'static str,
    );
    let cases: [Case; 5] = [
        (
            "third reaper",
            || standby::test_hook::threads(2),
            || Reservation::make(3, 0),
            "no thread",
        ),
        (
            "first reaper",
            || standby::test_hook::threads(0),
            || Reservation::make(1, 2),
            "no thread",
        ),
        (
            "second file",
            || test_hook::temp_files(1),
            || Reservation::make(0, 2),
            "no temp file",
        ),
        (
            "file after the reapers",
            || test_hook::temp_files(0),
            || Reservation::make(1, 1),
            "no temp file",
        ),
        (
            "second listener",
            || standby::test_hook::threads(1),
            || Reservation::of_listeners(2),
            "no thread",
        ),
    ];
    for (case, allowance, make, what) in cases {
        let _short = allowance();

        let e = make().expect_err(case);

        assert!(e.to_string().contains(what), "{case}: {e}");
    }
}

/// A draw past what the reservation made is a miscount: loud in every debug build, the test
/// suites' included, so an undercount fails them.
#[skuld::test]
fn a_draw_past_the_reservation_is_loud() {
    assert!(panics(|| drop(Reservation::default().reaper())), "a reaper");
    assert!(panics(|| drop(Reservation::default().file())), "a temp file");
    assert!(panics(|| drop(Reservation::default().listener())), "a listener");
    assert!(
        panics(|| drop(spawn(
            &mut sh("true"),
            Role::AnnouncedRequest(announced),
            &mut Reservation::of_listeners(1).unwrap()
        ))),
        "the second stream's listener"
    );
    assert!(
        panics(|| drop(spawn(
            &mut sh("true"),
            Role::Query,
            &mut Reservation::make(0, 1).unwrap()
        ))),
        "the second stream's file"
    );
}

/// `Role::files` and `Role::listeners` are what a spawn under that role draws from its reservation,
/// for every role: the one number the verbs' counts and the test `Fake` read, so none of them can
/// drift from the real spawn.
#[skuld::test]
fn a_role_names_what_its_spawn_draws() {
    let roles: [fn() -> Role; 4] = [
        || Role::Query,
        || Role::UncountedQuery,
        || Role::AnnouncedRequest(|_| true),
        || Role::Request(Reservation::make(1, 0).unwrap().reaper().unwrap()),
    ];
    for make in roles {
        let role = make();
        let name = format!("{role:?}");
        let (files, listeners) = (role.files(), role.listeners());
        // A role draws one of the two, so each is handed a reservation of the one it draws, and one
        // more than that.
        let mut reservation = if listeners > 0 {
            Reservation::of_listeners(listeners + 1).unwrap()
        } else {
            Reservation::make(0, files + 1).unwrap()
        };

        let spawned = spawn(&mut sh("true"), role, &mut reservation).unwrap();
        drop(spawned);

        assert_eq!(
            (test_hook::left(&reservation), test_hook::listeners_left(&reservation)),
            ((0, usize::from(listeners == 0)), usize::from(listeners > 0)),
            "{name}: one more than it draws was left"
        );
    }
}

/// A read no verb counted — launchd's `status` and `list` — makes its own files on the spot,
/// whatever it is handed: it draws nothing from the reservation, trips no assertion, and a file it
/// cannot make fails it before it runs.
#[skuld::test]
fn an_uncounted_query_never_draws_on_a_reservation() {
    for files_left in [STREAMS, 0] {
        let mut reservation = Reservation::make(0, files_left).unwrap();

        let mut cmd = sh("echo hi");
        let spawned = spawn(&mut cmd, Role::UncountedQuery, &mut reservation).unwrap();
        let finished = wait_bounded(spawned, Budget::Unbounded.start()).unwrap();

        assert!(
            matches!(&finished, Finished::Exited { capture, .. } if capture.stdout == b"hi\n"),
            "{files_left}: {finished:?}"
        );
        assert_eq!(
            test_hook::left(&reservation),
            (0, files_left),
            "{files_left}: the read drew on the reservation"
        );
    }
}

/// A read repeated as often as it takes (a wait for a job to be gone) takes its files from the
/// reservation once, and serves every repetition from them: none is made after the first, and each
/// read starts on an empty file, so it sees what its own child wrote and no earlier child's output.
#[skuld::test]
fn kept_files_serve_a_read_again_and_again_each_starting_empty() {
    let mut reservation = Reservation::make(0, STREAMS).unwrap();
    let kept = reservation.keep(STREAMS).unwrap();
    assert_eq!(test_hook::left(&reservation), (0, 0), "the files were taken once");
    let _no_file = test_hook::temp_files(0);

    // Each read writes less than the one before, so a file not emptied shows the longer one's tail.
    for (round, (out, err)) in [("hello", "world"), ("hi", "yo"), ("h", "y")].into_iter().enumerate() {
        let mut again = kept.again().unwrap();

        let spawned = spawn(
            &mut sh(&format!("printf %s {out}; printf %s {err} >&2")),
            Role::Query,
            &mut again,
        )
        .unwrap();
        let finished = wait_bounded(spawned, Budget::Unbounded.start()).unwrap();

        assert!(
            matches!(&finished, Finished::Exited { capture, .. }
                if capture.stdout == out.as_bytes() && capture.stderr == err.as_bytes()),
            "round {round}: {finished:?}"
        );
    }
}

// Listeners ===========================================================================================================

/// An announced request takes the threads that read its streams from the reservation it is handed,
/// so none need be made for it.
#[skuld::test]
fn an_announced_request_takes_its_threads_from_the_reservation_it_is_handed() {
    let mut reservation = Reservation::of_listeners(STREAMS).unwrap();
    let _no_thread = standby::test_hook::threads(0);

    let finished = wait_bounded(
        spawn(
            &mut sh("echo 'request issued' >&2"),
            Role::AnnouncedRequest(announced),
            &mut reservation,
        )
        .unwrap(),
        Budget::Unbounded.start(),
    );

    assert!(matches!(finished, Ok(Finished::Exited { .. })), "{finished:?}");
    assert_eq!(test_hook::listeners_left(&reservation), 0, "the listeners are spent");
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
    drain(ErrorOnFirstRead, tx);
    match rx.recv().unwrap() {
        Chunk::Failed(e) => assert_eq!(e.kind(), ErrorKind::BrokenPipe),
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
    let handle = std::thread::spawn(move || drain(PanicOnRead, tx));
    match rx.recv().unwrap() {
        Chunk::Failed(_) => {}
        other => panic!("expected Chunk::Failed, got {other:?}"),
    }
    assert!(handle.join().is_ok());
}

// collect =============================================================================================================

#[skuld::test]
fn collect_turns_a_reader_failure_into_an_error() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(b"partial".to_vec())).unwrap();
    tx.send(Chunk::Failed(io::Error::from(ErrorKind::BrokenPipe))).unwrap();
    drop(tx);

    let e = collect(rx, Budget::Unbounded.start(), "stderr").unwrap_err();
    assert_eq!(e.to_string(), "stderr", "the failing stream is named");
    assert_eq!(e.kind(), ErrorKind::BrokenPipe, "the kind of the failure is kept");
    let reported = crate::Error::spawn("client", e).report().to_string();
    assert!(
        reported.starts_with("failed to run `client`: stderr: "),
        "the failure under the stream's name is the source: {reported}"
    );
}

#[skuld::test]
fn collect_keeps_what_already_arrived_when_the_deadline_expires() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(b"partial".to_vec())).unwrap();
    // `tx` kept alive deliberately: nothing but the deadline can end this
    // collect, pinning the "expiry, not disconnect" path without a real
    // child.
    let (bytes, complete) = collect(rx, Budget::Immediate.start(), "stderr").unwrap();
    assert_eq!(bytes, b"partial");
    assert!(!complete);
    drop(tx);
}

#[skuld::test]
fn a_complete_drain_reports_complete() {
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(Chunk::Bytes(b"all of it".to_vec())).unwrap();
    drop(tx);

    let (bytes, complete) = collect(rx, Budget::Unbounded.start(), "stderr").unwrap();
    assert_eq!(bytes, b"all of it");
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
        Ok(Chunk::Bytes(b"x".to_vec()))
    }

    fn recv(&self) -> Result<Chunk, RecvError> {
        Ok(Chunk::Bytes(b"x".to_vec()))
    }

    fn recv_timeout(&self, _timeout: Duration) -> Result<Chunk, RecvTimeoutError> {
        Ok(Chunk::Bytes(b"x".to_vec()))
    }
}

#[skuld::test]
fn collect_stops_at_the_deadline_even_while_a_reader_keeps_producing() {
    // A `collect` that drains until the channel is empty never returns here.
    let (bytes, complete) = collect(Bottomless, Budget::Immediate.start(), "stderr").unwrap();
    assert_eq!(bytes, b"x".repeat(QUEUED), "exactly the chunks queued at expiry");
    assert!(!complete);
}
