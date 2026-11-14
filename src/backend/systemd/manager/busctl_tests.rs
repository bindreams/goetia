//! `busctl`'s two calls and what each answers, through a [`Fake`] table of stated replies.

use super::*;
use crate::backend::client::fake::{Fake, exited, fail, reply};
use crate::backend::systemd::manager::systemctl::systemctl_tests::{booted, unbooted};

const PATH: &str = "/org/freedesktop/systemd1/unit/x_2dy_2etimer";

fn over(fake: Fake) -> Busctl {
    Busctl::new(Box::new(fake), booted)
}

/// A bus that answers `LoadUnit` with `unit_path` and the property with `property`.
fn bus(unit_path: &str, property: &str) -> Fake {
    Fake::new([
        reply(&["--system", "call"], exited(0, unit_path, "")),
        reply(&["--system", "get-property"], exited(0, property, "")),
    ])
}

fn path_reply() -> String {
    format!("o \"{PATH}\"\n")
}

#[skuld::test]
fn next_run_parses_the_bus_reply() {
    let fake = bus(&path_reply(), "t 1794626807000000\n");

    let at = over(fake.clone()).next_elapse("x-y.timer").expect("next_elapse");

    assert_eq!(at, Some(UNIX_EPOCH + Duration::from_micros(1_794_626_807_000_000)));
    fake.assert_spent("both calls");
}

/// What the manager answers for a timer that is not armed.
#[skuld::test]
fn a_timer_that_is_not_armed_has_no_next_run() {
    let at = over(bus(&path_reply(), "t 0\n")).next_elapse("x-y.timer");

    assert_eq!(at.expect("next_elapse"), None);
}

/// The property is read at the path the manager answered, however that path is escaped: goetia
/// derives none (`-` is `_2d`, `.` is `_2e`, and the rule has more cases than these).
#[skuld::test]
fn the_property_is_read_at_the_path_the_manager_answered() {
    let fake = bus(&path_reply(), "t 1\n");

    over(fake.clone()).next_elapse("x-y.timer").expect("next_elapse");

    assert_eq!(
        fake.calls(),
        [
            vec![
                "--system",
                "call",
                "org.freedesktop.systemd1",
                "/org/freedesktop/systemd1",
                "org.freedesktop.systemd1.Manager",
                "LoadUnit",
                "s",
                "x-y.timer",
            ],
            vec![
                "--system",
                "get-property",
                "org.freedesktop.systemd1",
                PATH,
                "org.freedesktop.systemd1.Timer",
                "NextElapseUSecRealtime",
            ],
        ]
    );
}

/// A reply that holds neither the path nor the number asked for is no answer, and never a timer
/// that is not armed.
#[skuld::test]
fn a_reply_that_is_not_what_was_asked_for_is_no_answer() {
    for unit_path in ["", "garbage\n", "s \"/x\"\n", "o \"relative\"\n", "o \"/x\" extra\n"] {
        let e = over(Fake::new([reply(&["--system", "call"], exited(0, unit_path, ""))]))
            .next_elapse("x.timer")
            .expect_err(unit_path);
        assert!(matches!(e, Error::IncompleteAnswer { .. }), "{unit_path:?}: {e:?}");
    }
    for property in ["", "t\n", "t x\n", "s \"1\"\n", "u 1\n"] {
        let e = over(bus(&path_reply(), property))
            .next_elapse("x.timer")
            .expect_err(property);
        assert!(matches!(e, Error::IncompleteAnswer { .. }), "{property:?}: {e:?}");
    }
}

#[skuld::test]
fn a_busctl_that_failed_names_the_command_and_its_words() {
    let fake = Fake::new([reply(
        &["--system", "call"],
        exited(1, "", "Failed to connect to bus\n"),
    )]);

    let msg = over(fake)
        .next_elapse("x.timer")
        .expect_err("busctl failed")
        .report()
        .to_string();

    assert!(msg.contains("busctl --system call"), "{msg}");
    assert!(msg.contains("Failed to connect to bus"), "{msg}");
}

#[skuld::test]
fn a_busctl_that_could_not_run_is_a_failure_to_spawn() {
    let fake = Fake::new([fail(
        &["--system"],
        ClientError::NotRun(std::io::Error::other("no busctl").into()),
    )]);

    let e = over(fake).next_elapse("x.timer").expect_err("not run");

    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
}

/// The same door as `systemctl`'s: evidence that no manager can be asked refuses before `busctl` runs.
#[skuld::test]
fn a_host_with_no_manager_is_refused_before_busctl_runs() {
    let fake = Fake::new([]);

    let e = Busctl::new(Box::new(fake.clone()), unbooted)
        .next_elapse("x.timer")
        .expect_err("no manager");

    assert!(matches!(e, Error::NoManager { .. }), "{e:?}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}
