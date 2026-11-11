use std::process::Command;

use super::*;
use crate::backend::scm::shim::run::os::fake::FakeGuard;
use crate::backend::scm::shim::run::service::finish;

/// Names what a child of this test binary does: a code to end itself with, or an index into [`reports`].
const MODE: &str = "GOETIA_SHIM_EXIT_TEST_MODE";

/// What a child that returns instead of ending exits with, for a test to tell it from every code a
/// [`Reported`] ends the process with.
const RETURNED: i32 = 77;

/// The exit code of a child of this test binary that runs `test` with [`MODE`] set to `mode`: the
/// tests below end their own process, so each runs itself in one.
fn exit_of_child(test: &str, mode: impl ToString) -> Option<i32> {
    Command::new(std::env::current_exe().expect("the test binary"))
        .arg(test)
        .env(MODE, mode.to_string())
        .status()
        .expect("run the child")
        .code()
}

/// The mode a child runs in, if this is one.
fn child_mode() -> Option<String> {
    std::env::var(MODE).ok()
}

fn reports() -> [Reported; 4] {
    [
        Reported::Clean,
        Reported::incomplete_stop(),
        Reported::Refused,
        Reported::shim_failure(Some(5)),
    ]
}

/// The process ends with exactly the code, whatever its bits: a daemon's NTSTATUS exit is negative as
/// an `i32`. Runs itself in a child that ends itself, as the shim does.
#[skuld::test]
fn end_process_ends_the_process_with_exactly_the_code() {
    if let Some(code) = child_mode() {
        end_process(code.parse().expect("a code"));
    }

    for code in [7, 1003, -1073741819, i32::MIN] {
        assert_eq!(
            exit_of_child("end_process_ends_the_process_with_exactly_the_code", code),
            Some(code)
        );
    }
}

/// The main thread ends the process with the code the service thread recorded before it reported
/// `STOPPED`, whatever it was.
#[skuld::test]
fn the_main_thread_ends_the_process_with_the_recorded_code() {
    if let Some(index) = child_mode() {
        stopping(reports()[index.parse::<usize>().expect("an index")]);
        end_when_dispatched();
    }

    for (index, reported) in reports().into_iter().enumerate() {
        assert_eq!(
            exit_of_child("the_main_thread_ends_the_process_with_the_recorded_code", index),
            Some(reported.process_exit_code()),
            "{reported:?}"
        );
    }
}

/// A dispatcher that returned with no service having run has recorded nothing, and that is a clean end.
#[skuld::test]
fn the_main_thread_ends_clean_when_no_service_recorded_anything() {
    if child_mode().is_some() {
        end_when_dispatched();
    }

    assert_eq!(
        exit_of_child("the_main_thread_ends_clean_when_no_service_recorded_anything", 0),
        Some(0)
    );
}

/// `finish` records what it reports and returns when SCM took the `STOPPED` report: the dispatcher
/// returns, and the main thread ends the process.
#[skuld::test]
fn finish_returns_when_the_stopped_report_succeeded() {
    if child_mode().is_some() {
        let fake = FakeGuard::new();
        finish(&fake.log(), "frpc", Reported::incomplete_stop(), |_| Ok(()));
        std::process::exit(RETURNED);
    }

    assert_eq!(
        exit_of_child("finish_returns_when_the_stopped_report_succeeded", 0),
        Some(RETURNED)
    );
}

/// When SCM cannot be told `STOPPED` the dispatcher is not trusted to return, so `finish` ends the
/// process itself, with the code it was to report.
#[skuld::test]
fn finish_ends_the_process_with_the_reported_code_when_the_stopped_report_failed() {
    if child_mode().is_some() {
        let fake = FakeGuard::new();
        finish(&fake.log(), "frpc", Reported::incomplete_stop(), |_| {
            Err(std::io::Error::other("refused"))
        });
        std::process::exit(RETURNED);
    }

    assert_eq!(
        exit_of_child(
            "finish_ends_the_process_with_the_reported_code_when_the_stopped_report_failed",
            0
        ),
        Some(Reported::incomplete_stop().process_exit_code())
    );
}
