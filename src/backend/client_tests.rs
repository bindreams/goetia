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
            "SharedChild::new: ECHILD",
            || std::io::Error::from_raw_os_error(libc::ECHILD).into(),
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
        let wait = Wait::Bounded {
            role,
            deadline: Budget::Unbounded.start(),
        };
        match sh(script).run(&[], wait, &mut files()) {
            Err(ClientError::Lost { error, announced }) => {
                assert_eq!(announced, expected, "{script}");
                assert!(error.to_string().contains("the wait failed"), "{script}: {error:?}");
            }
            other => panic!("{script}: expected the child to be lost: {other:?}"),
        }
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
    assert!(drawn.is_err(), "a call past the reservation went unnoticed");
}
