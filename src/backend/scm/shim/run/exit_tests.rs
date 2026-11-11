use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use super::*;
use crate::backend::scm::shim::run::logging::Log;
use crate::backend::scm::shim::run::os::fake::FakeGuard;
use crate::backend::scm::shim::run::os::{Channels, Real, Severity};
use crate::backend::scm::shim::run::service::finish;

/// Names what a child of this test binary does: a code to end itself with, or an index into [`reports`].
const MODE: &str = "GOETIA_SHIM_EXIT_TEST_MODE";

/// The directory a child writes its failure log into, where the parent reads it: a child that ends
/// itself cannot report what it logged.
const LOGS: &str = "GOETIA_SHIM_EXIT_TEST_LOGS";

/// What a child that returns instead of ending exits with, for a test to tell it from every code a
/// [`Reported`] ends the process with.
const RETURNED: i32 = 77;

/// The mode that has a child exit [`RETURNED`] at once.
const PROBE: &str = "probe";

/// The exit code of a child of this test binary that runs `test` with [`MODE`] set to `mode`: the
/// tests below end their own process, so each runs itself in one.
///
/// A harness that ran no test exits 0, which is what a clean end exits with. So a probe child comes
/// first, and the test must have run in it.
fn exit_of_child(test: &str, mode: impl ToString) -> Option<i32> {
    exit_of_child_logging(test, mode, Path::new(""))
}

/// [`exit_of_child`], with the child's [`LOGS`] in `logs`.
fn exit_of_child_logging(test: &str, mode: impl ToString, logs: &Path) -> Option<i32> {
    let run = |mode: &str| {
        Command::new(std::env::current_exe().expect("the test binary"))
            .arg(test)
            .env(MODE, mode)
            .env(LOGS, logs)
            .status()
            .expect("run the child")
            .code()
    };
    assert_eq!(
        run(PROBE),
        Some(RETURNED),
        "the harness did not run `{test}` in the child"
    );
    run(&mode.to_string())
}

/// The channels of a child that is to be read after it ends itself: lines go to real files, and the
/// Event Log is left alone.
struct FileChannels;

impl Channels for FileChannels {
    fn append(&self, path: &Path, line: &str) -> io::Result<()> {
        Real.append(path, line)
    }

    fn event_log(&self, _severity: Severity, _line: &str) {}
}

/// The failure log of a child, in the directory the parent gave it.
fn logs() -> Log {
    Log::new(
        Arc::new(FileChannels),
        PathBuf::from(std::env::var_os(LOGS).expect("the logs directory")),
    )
}

/// The mode a child runs in, if this is one.
fn child_mode() -> Option<String> {
    let mode = std::env::var(MODE).ok()?;
    if mode == PROBE {
        std::process::exit(RETURNED);
    }
    Some(mode)
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
        // The process ends here, so the guard's check that the fake was spent is run by hand.
        drop(fake);
        std::process::exit(RETURNED);
    }

    assert_eq!(
        exit_of_child("finish_returns_when_the_stopped_report_succeeded", 0),
        Some(RETURNED)
    );
}

/// When SCM cannot be told `STOPPED` the dispatcher is not trusted to return, so `finish` ends the
/// process itself, with the code it was to report, after it logged why.
#[skuld::test]
fn finish_ends_the_process_with_the_reported_code_when_the_stopped_report_failed() {
    if child_mode().is_some() {
        finish(&logs(), "frpc", Reported::incomplete_stop(), |_| {
            Err(std::io::Error::other("refused"))
        });
        std::process::exit(RETURNED);
    }

    let dir = tempfile::tempdir().expect("a directory for the child's log");

    let code = exit_of_child_logging(
        "finish_ends_the_process_with_the_reported_code_when_the_stopped_report_failed",
        0,
        dir.path(),
    );

    assert_eq!(code, Some(Reported::incomplete_stop().process_exit_code()));
    let logged = std::fs::read_to_string(dir.path().join("frpc.log")).expect("the child's log");
    assert!(logged.contains("report SERVICE_STOPPED to SCM: refused"), "{logged}");
}
