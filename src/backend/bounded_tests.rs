use std::io::{self, BufRead, ErrorKind, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::manager::budget::Budget;

/// `cmd`, spawned under `role`.
fn spawned(mut cmd: cosca::Command, role: Role) -> Spawned {
    spawn(&mut cmd, role).unwrap()
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
        Finished::Expired => panic!("expected Exited, got Expired"),
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
        Finished::Expired => panic!("expected Exited, got Expired"),
    }
}

#[skuld::test]
fn an_expired_deadline_kills_and_reaps_the_child() {
    let child = spawned(command("sleep", &["2147483647"]).unwrap(), Role::Query);
    let pid = child.child.id().pid();

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

/// The one stream read as it is written, stderr under [`Role::AnnouncedRequest`], held open by a
/// descendant the kill cannot reach, still returns at the deadline.
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

#[skuld::test]
fn a_contained_childs_descendant_dies_with_it() {
    // Same script, same fd-3 handshake, but spawned through `bounded::command`
    // so the tree IS contained.
    let mut cmd = sh("sleep 2147483647 & echo $! >&3; exec sleep 2147483647");
    cmd.fd(3, cosca::Stdio::pipe_out()).unwrap();
    let mut child = spawned(cmd, Role::Query);
    #[cfg(target_os = "linux")]
    let leaf = contained_leaf(child.child.id().pid());

    let fd3 = child.child.fd_read_end(3.into()).unwrap();
    let mut line = String::new();
    io::BufReader::new(fd3).read_line(&mut line).unwrap();
    let descendant: u32 = line.trim().parse().unwrap();

    let deadline = Budget::Bounded(Duration::from_millis(50)).start();
    let finished = wait_bounded(child, deadline).unwrap();
    assert!(matches!(finished, Finished::Expired));

    // A real event-driven death-watch (pidfd on Linux, EVFILT_PROC |
    // NOTE_EXIT on macOS), with no timeout to choose: it returns when the
    // descendant dies, and hangs if the descendant is not in the child's
    // contained tree (e.g. `command()` without `.contain()`) — then neither
    // `kill_tree` nor `Drop` can reach it.
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
            Finished::Expired => panic!("expected Exited, got Expired"),
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
        Finished::Expired => panic!("expected Exited, got Expired"),
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

/// Spawn `sh -c 'read line; <then>'` as a [`Role::Request`], let it expire at once, and return how
/// it ended as its reaper reports it. It reads EOF on its stdin, and so runs `then`, only once the
/// reaper holds it and is waiting — after the expiry.
fn expired_request_released_by_its_reaper(then: &str) -> ExitStatus {
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
    let mut child = spawned(released_at_eof(then), Role::Request(reaper));
    writer_tx.send(child.child.stdin().expect("stdin is piped")).unwrap();

    let finished = wait_bounded(child, Budget::Immediate.start()).unwrap();

    assert!(matches!(finished, Finished::Expired), "{finished:?}");
    rx.recv()
        .expect("a request left running must be reaped once it exits")
        .expect("waiting on the child")
}

/// The child the reaper tests hand their reaper: runs `then` at EOF on its stdin, a pipe the test
/// holds. `command()`'s, as launchd's `launchctl` — the one production `Role::Request` — is spawned.
#[cfg(not(target_os = "linux"))]
fn released_at_eof(then: &str) -> cosca::Command {
    let mut cmd = sh(&format!("read line; {then}"));
    cmd.stdin(cosca::Stdio::pipe_in()).unwrap();
    cmd
}

/// On Linux, uncontained. cosca 0.4's cgroup containment kills a detached tree — the leaf's `Drop`
/// writes `cgroup.kill` — and leaves the leaf behind, measured; nothing on Linux detaches, since
/// `systemctl` announces its requests.
#[cfg(target_os = "linux")]
fn released_at_eof(then: &str) -> cosca::Command {
    let mut cmd = cosca::run(["/bin/sh", "-c", &format!("read line; {then}")]);
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
/// Each allowance fails a different one: the stdout file, the stderr file, the stderr thread.
#[skuld::test]
fn a_child_whose_output_nothing_could_take_is_never_run() {
    type Allowance = fn() -> test_hook::Allowance;
    type Case = (&'static str, fn() -> Role, Allowance, &'static str);
    let cases: [Case; 6] = [
        ("query stdout", || Role::Query, || test_hook::temp_files(0), "temp file"),
        ("query stderr", || Role::Query, || test_hook::temp_files(1), "temp file"),
        ("request stdout", request, || test_hook::temp_files(0), "temp file"),
        ("request stderr", request, || test_hook::temp_files(1), "temp file"),
        (
            "announced stdout",
            || Role::AnnouncedRequest(announced),
            || test_hook::threads(0),
            "thread",
        ),
        (
            "announced stderr",
            || Role::AnnouncedRequest(announced),
            || test_hook::threads(1),
            "thread",
        ),
    ];
    for (case, role, allowance, what) in cases {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("request");
        let role = role();
        let _short = allowance();
        let spawns = test_hook::spawns();

        let e = spawn(&mut marks(&marker), role).err().expect(case);

        assert!(e.to_string().contains(&format!("no {what} ")), "{case}: {e}");
        assert!(e.to_string().contains("so it was not run"), "{case}: {e}");
        assert_eq!(test_hook::spawns(), spawns, "{case}: the child was spawned");
        assert!(!marker.exists(), "{case}: the child ran");
    }
}

/// An announced request — `systemctl` — makes no temp file at all: it runs where none may be
/// writable, a chroot or an image build.
#[skuld::test]
fn an_announced_request_needs_no_temp_file() {
    let _no_file = test_hook::temp_files(0);
    let finished = wait_bounded(
        spawned(
            sh("echo out; echo 'request issued' >&2"),
            Role::AnnouncedRequest(announced),
        ),
        Budget::Unbounded.start(),
    );
    match finished {
        Ok(Finished::Exited { capture, .. }) => {
            assert_eq!(capture.stdout, b"out\n");
            assert_eq!(capture.stderr, b"request issued\n");
            assert!(capture.complete);
        }
        other => panic!("expected Exited, got {other:?}"),
    }
}

fn request() -> Role {
    Role::Request(Reaper::with(cosca::Child::wait, |_| {}).unwrap())
}

/// A spawn that failed is placed before the child ran only when cosca's error says so: an `Io`
/// carrying an OS error from a pipe, `fork` or `exec`. Everything else — cosca's identity read and
/// `SharedChild::new` after `exec`, and the variants it raises on both sides of it — may have run
/// the child, and so may have sent its request. Injected, so nothing runs.
#[skuld::test]
fn a_failed_spawn_is_not_run_only_on_evidence() {
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
        (
            "pipe: EMFILE",
            || io::Error::from_raw_os_error(libc::EMFILE).into(),
            false,
        ),
        (
            "SharedChild::new: ECHILD",
            || io::Error::from_raw_os_error(libc::ECHILD).into(),
            true,
        ),
        (
            "SharedChild::new or exec: EINVAL",
            || io::Error::from_raw_os_error(libc::EINVAL).into(),
            true,
        ),
        (
            "identity: vanished",
            || io::Error::other("spawned child vanished before its identity could be read").into(),
            true,
        ),
        (
            "identity: refused",
            || cosca::error::Error::Unassessable {
                detail: "the OS refused to report the spawned child's identity".into(),
                source: None,
            },
            true,
        ),
        (
            "attach",
            || cosca::error::Error::Containment {
                detail: "forced".into(),
            },
            true,
        ),
    ];
    for (case, error, may_have_run) in cases {
        test_hook::spawn_fails(error);
        match spawn(&mut sh("true"), Role::Query) {
            Err(SpawnError::MayHaveRun(_)) => assert!(may_have_run, "{case}: placed after exec"),
            Err(SpawnError::NotRun(_)) => assert!(!may_have_run, "{case}: placed before exec"),
            Ok(_) => panic!("{case}: the injected failure was not returned"),
        }
    }
}

/// A sequence's spares are what its children's output is given, so nothing need be made for them.
#[skuld::test]
fn a_child_takes_its_output_from_the_spares() {
    let spares = spare(Needs {
        listeners: 2,
        files: 2,
        ..Needs::default()
    })
    .unwrap();
    let _no_thread = test_hook::threads(0);
    let _no_file = test_hook::temp_files(0);

    for role in [Role::AnnouncedRequest(announced), Role::Query] {
        let finished = wait_bounded(
            spawned(sh("echo 'request issued' >&2"), role),
            Budget::Unbounded.start(),
        );
        assert!(matches!(finished, Ok(Finished::Exited { .. })), "{finished:?}");
    }
    assert_eq!(test_hook::pooled(), Needs::default(), "the spares are spent");
    drop(spares);
}

fn files(n: usize) -> Needs {
    Needs {
        files: n,
        ..Needs::default()
    }
}

/// Whether `f` panics — the debug assertion every build of the test suites carries.
fn panics(f: impl FnOnce()) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
}

/// While a reservation is live on this thread, everything its verbs need was to have been made in
/// it: a thread or file made on the spot instead means one needed more than was counted, and fails
/// loudly — every way one is made. With none live, making on the spot is how a lone verb gets what
/// it needs.
#[skuld::test]
fn making_on_the_spot_inside_a_reservation_is_loud() {
    let spent = || spare(Needs::default()).unwrap();
    assert!(panics(|| drop((spent(), spares::file()))), "a temp file");
    assert!(panics(|| drop((spent(), spares::listener()))), "a listener");
    assert!(panics(|| drop((spent(), reapers::<1>()))), "a reaper");
    assert!(
        panics(|| drop((spent(), top_up(files(1))))),
        "a top-up that falls short"
    );

    drop(spares::file().expect("no reservation is live"));
    drop(reapers::<1>().expect("no reservation is live"));
    drop(top_up(files(1)).expect("no reservation is live"));
}

// Spares ==============================================================================================================

fn reapers_only(n: usize) -> Needs {
    Needs {
        reapers: n,
        ..Needs::default()
    }
}

/// A verb's reapers are all made before any is handed out, so a verb whose third request could have
/// none never sends its first.
#[skuld::test]
fn reapers_are_all_made_or_none_are() {
    let _only_two = test_hook::threads(2);

    let made = reapers::<3>();

    let e = made.expect_err("a third reaper could not be made");
    assert!(e.to_string().contains("no thread may be made"), "{e}");
}

/// A sequence's spares are taken before anything is made: its later verbs need no new thread, so
/// cannot fail for want of one after an earlier verb has sent something.
#[skuld::test]
fn spares_are_taken_before_any_reaper_is_made() {
    let spares = spare(reapers_only(3)).unwrap();
    let _none = test_hook::threads(0);

    assert!(reapers::<1>().is_ok(), "the stop's reaper is a spare");
    assert!(reapers::<2>().is_ok(), "the start's reapers are spares");
    assert_eq!(test_hook::pooled(), Needs::default(), "the spares are spent");
    drop(spares);
}

/// Sparing is all or nothing: a guard that could not make everything leaves nothing behind.
#[skuld::test]
fn a_spare_that_cannot_be_made_whole_spares_nothing() {
    {
        let _no_file = test_hook::temp_files(0);
        let e = spare(Needs {
            reapers: 1,
            files: 1,
            ..Needs::default()
        })
        .expect_err("no file may be made");
        assert!(e.to_string().contains("no temp file may be made"), "{e}");
    }
    assert_eq!(
        test_hook::pooled(),
        Needs::default(),
        "the reaper made before the failure was pooled"
    );
}

/// Topping up makes only what the pool lacks: a verb inside a prepared sequence makes nothing, and
/// a lone verb makes all it needs.
#[skuld::test]
fn a_top_up_makes_only_what_the_pool_lacks() {
    let prepared = spare(files(4)).unwrap();
    let _no_file = test_hook::temp_files(0);
    drop(top_up(files(4)).expect("the pool holds four"));
    drop(prepared);

    let _four = test_hook::temp_files(4);
    let lone = top_up(files(4)).expect("four may be made");
    assert_eq!(test_hook::pooled(), files(4));
    drop(lone);
}

/// Dropping the guard drops what it left: nothing lingers for a later, unrelated verb.
#[skuld::test]
fn spares_are_released_with_their_guard() {
    drop(spare(reapers_only(2)).unwrap());
    let _none = test_hook::threads(0);

    assert!(reapers::<1>().is_err(), "a released spare was still handed out");
}

/// A guard releases only the spares it made: an inner one dropped first leaves the outer one's for
/// the verbs the outer one still covers.
#[skuld::test]
fn an_inner_guard_releases_only_its_own_spares() {
    let outer = spare(reapers_only(2)).unwrap();
    drop(spare(reapers_only(1)).unwrap());
    let _none = test_hook::threads(0);

    assert_eq!(
        test_hook::pooled(),
        reapers_only(2),
        "only the inner guard's spare goes with it"
    );
    drop(outer);
}

/// Guards may also end out of order: dropping the outer one first leaves the inner one's spares.
#[skuld::test]
fn an_outer_guard_dropped_first_leaves_the_inner_guards_spares() {
    let outer = spare(reapers_only(2)).unwrap();
    let inner = spare(reapers_only(1)).unwrap();
    drop(outer);
    let _none = test_hook::threads(0);

    assert_eq!(
        test_hook::pooled(),
        reapers_only(1),
        "only the outer guard's spares go with it"
    );
    drop(inner);
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
    assert!(
        e.to_string().starts_with("stderr: "),
        "the failing stream is named: {e}"
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
