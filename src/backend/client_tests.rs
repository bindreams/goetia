use std::time::Duration;

use super::*;
use crate::manager::Budget;

/// A [`Real`] running `sh -c <script>`.
fn sh(script: &str) -> Real {
    sh_with(script, |_| {})
}

fn sh_with(script: &str, environment: fn(&mut cosca::Command)) -> Real {
    Real::new(
        ["/bin/sh", "-c", script, "client"].map(String::from).to_vec(),
        environment,
    )
}

/// A reservation holding the temp files one call's streams draw.
fn files() -> Reservation {
    Reservation::make(0, bounded::STREAMS).unwrap()
}

fn under_deadline() -> Wait {
    Wait::Bounded {
        role: Role::Query,
        deadline: Budget::Unbounded.start(),
    }
}

#[skuld::test]
fn real_runs_and_captures() {
    let finished = sh("echo out; echo err >&2; exit 3")
        .run(&[], under_deadline(), &mut files())
        .expect("sh runs");
    let Finished::Exited { status, capture } = finished else {
        panic!("an unbounded run never expires: {finished:?}");
    };
    assert_eq!(status.code(), Some(3));
    assert_eq!(capture.stdout, b"out\n");
    assert_eq!(capture.stderr, b"err\n");
    assert!(capture.complete);
}

#[skuld::test]
fn real_reports_a_missing_program_as_not_run() {
    let real = Real::new(vec!["/nonexistent/client".to_string()], |_| {});
    let e = real
        .run(&["x"], under_deadline(), &mut files())
        .expect_err("no such program");
    assert!(matches!(e, ClientError::NotRun(_)), "{e:?}");
}

#[skuld::test]
fn real_puts_the_args_after_the_program() {
    let finished = Real::new(vec!["/bin/echo".to_string(), "first".to_string()], |_| {})
        .run(&["second", "third"], under_deadline(), &mut files())
        .expect("echo runs");
    let Finished::Exited { capture, .. } = finished else {
        panic!("{finished:?}");
    };
    assert_eq!(capture.stdout, b"first second third\n");
}

#[skuld::test]
fn real_applies_its_environment_to_the_child() {
    let real = sh_with("echo \"$CLIENT_MARK\"", |cmd| {
        cmd.env("CLIENT_MARK", "set");
    });
    let Finished::Exited { capture, .. } = real.run(&[], under_deadline(), &mut files()).expect("sh runs") else {
        panic!("expired");
    };
    assert_eq!(capture.stdout, b"set\n");
}

#[skuld::test]
fn real_expires_on_a_spent_deadline() {
    let wait = Wait::Bounded {
        role: Role::Query,
        deadline: Budget::Bounded(Duration::ZERO).start(),
    };
    let finished = sh("exec sleep 2147483647")
        .run(&[], wait, &mut files())
        .expect("sh runs");
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
}

const ISSUED: &[u8] = b"request issued";

fn announced(stderr: &[u8]) -> bool {
    stderr.windows(ISSUED.len()).any(|w| w == ISSUED)
}

/// A spawn that fails before the child ran says so, and one that may have run it says that: which
/// of the two is what a caller sending a request must not confuse. The cases are `bounded`'s own
/// for each side of `exec`.
#[skuld::test]
fn real_tells_a_spawn_that_failed_before_exec_from_one_that_may_have_run() {
    type Case = (&'static str, fn() -> cosca::error::Error, bool);
    let cases: [Case; 4] = [
        (
            "fork: EAGAIN",
            || std::io::Error::from_raw_os_error(libc::EAGAIN).into(),
            false,
        ),
        (
            "exec: ENOENT",
            || std::io::Error::from_raw_os_error(libc::ENOENT).into(),
            false,
        ),
        (
            "before the spawn: containment",
            || cosca::error::Error::Containment {
                detail: "forced".into(),
                source: None,
            },
            false,
        ),
        (
            "after exec: the attach",
            || {
                bounded::test_hook::may_have_started(cosca::error::Error::Containment {
                    detail: "forced".into(),
                    source: None,
                })
            },
            true,
        ),
    ];
    for (case, error, may_have_run) in cases {
        bounded::test_hook::spawn_fails(error);
        match sh("true").run(&[], under_deadline(), &mut files()) {
            Err(ClientError::MayHaveRun(_)) => assert!(may_have_run, "{case}: placed after exec"),
            Err(ClientError::NotRun(_)) => assert!(!may_have_run, "{case}: placed before exec"),
            other => panic!("{case}: the injected failure was not mapped to a spawn error: {other:?}"),
        }
    }
}

/// A wait that fails leaves the child lost, and `announced` is whether it had said the manager has
/// its request: true only for the role that listens, and only if the child said it before the loss.
#[skuld::test]
fn real_loses_a_child_and_says_whether_it_announced() {
    for (script, role, expected) in [
        (
            "echo 'request issued' >&2; exec sleep 2147483647",
            Role::AnnouncedRequest(announced),
            true,
        ),
        ("exit 0", Role::AnnouncedRequest(announced), false),
        ("echo 'request issued' >&2; exit 0", Role::Query, false),
    ] {
        bounded::test_hook::wait_fails(|| std::io::Error::other("the wait failed").into());
        let mut reservation = Reservation::for_role(&role);
        let wait = Wait::Bounded {
            role,
            deadline: Budget::Unbounded.start(),
        };
        match sh(script).run(&[], wait, &mut reservation) {
            Err(ClientError::Lost { error, announced }) => {
                assert_eq!(announced, expected, "{script}");
                assert!(
                    cosca::error::Report::new(&error)
                        .to_string()
                        .contains("the wait failed"),
                    "{script}: {error:?}"
                );
            }
            other => panic!("{script}: expected the child to be lost: {other:?}"),
        }
    }
}

// Wait::ToCompletion ==================================================================================================

#[skuld::test]
fn real_runs_to_completion_and_captures() {
    let finished = sh("echo out; echo err >&2; exit 3")
        .run(&[], Wait::ToCompletion, &mut Reservation::default())
        .expect("sh runs");
    let Finished::Exited { status, capture } = finished else {
        panic!("a run to completion never expires: {finished:?}");
    };
    assert_eq!(status.code(), Some(3));
    assert_eq!(capture.stdout, b"out\n");
    assert_eq!(capture.stderr, b"err\n");
    assert!(capture.complete);
}

/// A child that fills one pipe past its buffer before it writes the other is not blocked by the
/// stream read second: both captures arrive whole, whichever stream is read first.
#[skuld::test]
fn real_to_completion_drains_both_streams_past_a_pipe_buffer() {
    const SIZE: usize = 100_000;
    let script = format!("head -c {SIZE} /dev/zero | tr '\\0' e >&2; head -c {SIZE} /dev/zero | tr '\\0' o");
    let finished = sh(&script)
        .run(&[], Wait::ToCompletion, &mut Reservation::default())
        .expect("sh runs");
    let Finished::Exited { status, capture } = finished else {
        panic!("a run to completion never expires: {finished:?}");
    };
    assert!(status.success(), "{status:?}");
    assert_eq!(capture.stderr, vec![b'e'; SIZE]);
    assert_eq!(capture.stdout, vec![b'o'; SIZE]);
    assert!(capture.complete);
}

/// What a run to completion needs it makes itself: a reservation holding nothing is enough.
#[skuld::test]
fn real_to_completion_draws_nothing_from_the_reservation() {
    let mut reservation = Reservation::make(0, 0).unwrap();
    sh("echo out")
        .run(&[], Wait::ToCompletion, &mut reservation)
        .expect("sh runs");
    assert_eq!(bounded::test_hook::left(&reservation), (0, 0));
}

/// The child's stdin is `/dev/null`, not the test process's own: a child that inherited it would
/// read from, and could block on, whatever the caller holds open.
///
/// A test runner may already give the test process `/dev/null` (nextest does), which an inherited
/// stdin would then match. So the test runs itself again with a pipe on stdin, and the copy asks its
/// child whether its stdin is a pipe: `/dev/null` is not, an inherited stdin would be.
#[skuld::test]
fn real_to_completion_gives_the_child_no_stdin() {
    use std::os::unix::fs::FileTypeExt;
    use std::process::{Command, Stdio};

    const PROBE: &str = "GOETIA_CLIENT_STDIN_PROBE";
    const THIS_TEST: &str = "real_to_completion_gives_the_child_no_stdin";

    if std::env::var_os(PROBE).is_some() {
        let own = std::fs::metadata("/dev/stdin").expect("stat own stdin");
        assert!(own.file_type().is_fifo(), "stdin is not the probe's pipe: {own:?}");
        let Finished::Exited { capture, .. } = sh("[ -p /dev/fd/0 ] && echo pipe || echo null")
            .run(&[], Wait::ToCompletion, &mut Reservation::default())
            .expect("sh runs")
        else {
            panic!("expired");
        };
        // The leading newline puts the line on its own: the harness may have printed `test ... `
        // before it.
        println!("\n{PROBE}={}", String::from_utf8_lossy(&capture.stdout).trim());
        return;
    }

    let probe = Command::new(std::env::current_exe().expect("this test binary"))
        .args([THIS_TEST, "--nocapture"])
        .env(PROBE, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run the test binary again");
    let stdout = String::from_utf8_lossy(&probe.stdout);
    let stderr = String::from_utf8_lossy(&probe.stderr);
    assert!(
        stdout.lines().any(|line| line == format!("{PROBE}=null")),
        "the child's stdin was not /dev/null ({}):\n{stdout}\n{stderr}",
        probe.status
    );
}

#[skuld::test]
fn real_to_completion_applies_its_environment_to_the_child() {
    let real = sh_with("echo \"$CLIENT_MARK\"", |cmd| {
        cmd.env("CLIENT_MARK", "set");
    });
    let Finished::Exited { capture, .. } = real
        .run(&[], Wait::ToCompletion, &mut Reservation::default())
        .expect("sh runs")
    else {
        panic!("expired");
    };
    assert_eq!(capture.stdout, b"set\n");
}

#[skuld::test]
fn real_to_completion_reports_a_missing_program_as_not_run() {
    let real = Real::new(vec!["/nonexistent/client".to_string()], |_| {});
    let e = real
        .run(&["x"], Wait::ToCompletion, &mut Reservation::default())
        .expect_err("no such program");
    assert!(matches!(e, ClientError::NotRun(_)), "{e:?}");
}

/// A run to completion that fails after the spawn leaves the client possibly run: cosca says so with
/// [`cosca::error::Error::MayHaveStarted`], and that is what [`Real`] reports as [`ClientError::MayHaveRun`]. A
/// real child cannot be made to fail there, so the error is stated, as `bounded`'s spawn test does.
#[skuld::test]
fn real_to_completion_tells_a_failure_after_the_spawn_from_one_before_it() {
    let after = bounded::test_hook::may_have_started(std::io::Error::other("the read failed").into());
    assert!(
        matches!(of_output(Err(after)), Err(ClientError::MayHaveRun(_))),
        "a failure after the spawn"
    );
    for before in [
        cosca::error::Error::from(std::io::Error::from_raw_os_error(libc::EAGAIN)),
        cosca::error::Error::Containment {
            detail: "forced".into(),
            source: None,
        },
    ] {
        let described = format!("{before:?}");
        assert!(
            matches!(of_output(Err(before)), Err(ClientError::NotRun(_))),
            "a failure before the spawn: {described}"
        );
    }
}

// fake::Fake ==========================================================================================================

/// The test client draws what the real one draws, from the reservation it is handed: a call leaves
/// the reservation shorter by its streams' files, and one handed too few is a miscount, which fails
/// a debug assertion in every test that drives it.
#[skuld::test]
fn the_fake_draws_a_calls_files_from_the_reservation() {
    use fake::{Fake, exited, reply};

    let query = || Wait::Bounded {
        role: Role::Query,
        deadline: Budget::Unbounded.start(),
    };
    let mut reservation = Reservation::make(0, 2 * bounded::STREAMS).unwrap();

    let table = Fake::new([
        reply(&["print"], exited(0, "", "")),
        reply(&["print"], exited(0, "", "")),
    ]);
    table
        .run(&["print"], query(), &mut reservation)
        .expect("a scripted reply");
    assert_eq!(bounded::test_hook::left(&reservation), (0, bounded::STREAMS));

    table
        .run(&["print"], query(), &mut reservation)
        .expect("a scripted reply");
    assert_eq!(bounded::test_hook::left(&reservation), (0, 0));

    let short = Fake::new([reply(&["print"], exited(0, "", ""))]);
    let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(short.run(&["print"], query(), &mut reservation));
    }));
    // The reservation has nothing to give: it stays empty, whichever way the miscount went. The miscount
    // itself is a debug assertion, so a release build makes the file on the spot and the call goes on.
    assert_eq!(bounded::test_hook::left(&reservation), (0, 0));
    assert_eq!(
        drawn.is_err(),
        cfg!(debug_assertions),
        "a call past the reservation is a miscount in a debug build and is served in a release one"
    );
}

/// An announced request draws its streams' listener threads from the reservation, as the real
/// client's spawn does: a call leaves the reservation shorter by them.
#[skuld::test]
fn the_fake_draws_an_announced_requests_listeners_from_the_reservation() {
    use fake::{Fake, exited, reply};

    let announced = || Wait::Bounded {
        role: Role::AnnouncedRequest(|_| true),
        deadline: Budget::Unbounded.start(),
    };
    let mut reservation = Reservation::of_listeners(bounded::STREAMS + 1).unwrap();

    Fake::new([reply(&["start"], exited(0, "", ""))])
        .run(&["start"], announced(), &mut reservation)
        .expect("a scripted reply");

    assert_eq!(bounded::test_hook::listeners_left(&reservation), 1);
}

/// How a call was waited on is recorded, and a run to completion draws no file.
#[skuld::test]
fn the_fake_records_a_run_to_completion_and_draws_nothing_for_it() {
    use fake::{Fake, RoleKind, Waited, exited, reply};

    let mut reservation = Reservation::make(0, 0).unwrap();
    let table = Fake::new([reply(&["show"], exited(0, "", ""))]);
    table
        .run(&["show"], Wait::ToCompletion, &mut reservation)
        .expect("a scripted reply");
    assert_eq!(
        table.waits(),
        [Waited {
            role: RoleKind::ToCompletion,
            bounded: false
        }]
    );
}
