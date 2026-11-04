use std::time::Duration;

use super::fake::{Fake, exited, fail, reply};
use super::*;
use crate::manager::Budget;

/// A [`Real`] whose program is `sh -c <script>`, as no service manager's client is.
fn sh(script: &str) -> Real {
    sh_with(script, |_| {})
}

fn sh_with(script: &str, environment: fn(&mut cosca::Command)) -> Real {
    Real::new(
        ["/bin/sh", "-c", script, "client"].map(String::from).to_vec(),
        environment,
    )
}

fn under_deadline() -> Wait {
    Wait::Bounded {
        role: Role::Query,
        deadline: Budget::Unbounded.start(),
    }
}

// Real ================================================================================================================

#[skuld::test]
fn real_runs_and_captures() {
    let finished = sh("echo out; echo err >&2; exit 3")
        .run(&[], under_deadline())
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
    let e = real.run(&["x"], under_deadline()).expect_err("no such program");
    assert!(matches!(e, ClientError::NotRun(_)), "{e:?}");
}

#[skuld::test]
fn real_puts_the_args_after_the_program() {
    let finished = Real::new(vec!["/bin/echo".to_string(), "first".to_string()], |_| {})
        .run(&["second", "third"], under_deadline())
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
    let Finished::Exited { capture, .. } = real.run(&[], under_deadline()).expect("sh runs") else {
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
    let finished = sh("exec sleep 2147483647").run(&[], wait).expect("sh runs");
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
}

// Real, failures ------------------------------------------------------------------------------------------------------

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
        match sh("true").run(&[], under_deadline()) {
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
        match sh(script).run(&[], wait) {
            Err(ClientError::Lost { error, announced }) => {
                assert_eq!(announced, expected, "{script}");
                assert!(error.to_string().contains("the wait failed"), "{script}: {error:?}");
            }
            other => panic!("{script}: expected the child to be lost: {other:?}"),
        }
    }
}

// Fake ================================================================================================================

#[skuld::test]
fn fake_answers_in_order_and_records_calls() {
    let fake = Fake::new([
        reply(&["show"], exited(0, "a", "")),
        fail(
            &["start", "--"],
            ClientError::NotRun(cosca::error::Error::Io(std::io::Error::other("no"))),
        ),
        reply(&["stop"], exited(5, "", "b")),
    ]);
    let handle = fake.clone();

    let Finished::Exited { capture, .. } = fake.run(&["show", "--property", "X"], under_deadline()).unwrap() else {
        panic!("expired");
    };
    assert_eq!(capture.stdout, b"a");
    assert!(matches!(
        fake.run(&["start", "--", "x"], under_deadline()),
        Err(ClientError::NotRun(_))
    ));
    let Finished::Exited { status, capture } = fake.run(&["stop"], under_deadline()).unwrap() else {
        panic!("expired");
    };
    assert_eq!(status.code(), Some(5));
    assert_eq!(capture.stderr, b"b");

    assert_eq!(
        handle.calls(),
        [vec!["show", "--property", "X"], vec!["start", "--", "x"], vec!["stop"]]
    );
    handle.assert_spent("every entry was called");
}

#[skuld::test]
fn fake_runs_the_entry_action_at_the_call() {
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ran.clone();
    let fake = Fake::new([reply(&["print"], exited(0, "", "")).then(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    })]);
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    fake.run(&["print"], under_deadline()).unwrap();
    assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[skuld::test]
#[should_panic(expected = "unscripted")]
fn fake_panics_on_an_unscripted_call() {
    let fake = Fake::new([reply(&["show"], exited(0, "", ""))]);
    fake.run(&["show"], under_deadline()).unwrap();
    let _ = fake.run(&["show"], under_deadline());
}

#[skuld::test]
#[should_panic(expected = "expected a call starting")]
fn fake_panics_on_a_call_with_another_argv() {
    let fake = Fake::new([reply(&["show"], exited(0, "", ""))]);
    let _ = fake.run(&["stop"], under_deadline());
}

#[skuld::test]
#[should_panic(expected = "never called")]
fn fake_panics_when_an_entry_was_never_called() {
    let fake = Fake::new([reply(&["show"], exited(0, "", "")), reply(&["stop"], exited(0, "", ""))]);
    fake.run(&["show"], under_deadline()).unwrap();
    fake.assert_spent("the stop was skipped");
}
