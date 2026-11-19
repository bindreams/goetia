use super::*;
use crate::cli::report::{ErrorReport, Kind};
use crate::manager::{DaemonStatus, Exit, State, Status, TaskStatus};

fn rendered(id: Option<&str>, message: &str) -> (String, String) {
    let report = Report {
        daemons: Vec::new(),
        errors: vec![ErrorReport::said(
            id.map(str::to_string),
            Kind::InvalidId,
            message.to_string(),
        )],
        undetermined: Vec::new(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    print_text(&report, &mut out, &mut err);
    (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn line_of(status: &Status) -> String {
    format_status_line(&report::daemon("job", status, &jiff::tz::TimeZone::UTC))
}

/// A service's line is as it was before tasks.
#[skuld::test]
fn status_text_of_a_service_has_no_task_suffix() {
    let status = Status::Service(DaemonStatus {
        state: State::Running,
        pid: Some(1234),
        enabled: true,
    });

    assert_eq!(line_of(&status), "job: running (enabled=true, pid=1234)");
}

#[skuld::test]
fn status_text_of_a_task_names_its_type_results_and_times() {
    use std::time::{Duration, UNIX_EPOCH};

    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    let task = |last_result, last_run, next_run| {
        Status::Task(TaskStatus {
            common: DaemonStatus {
                state: State::Idle,
                pid: None,
                enabled: true,
            },
            last_result,
            last_run,
            next_run,
        })
    };

    assert_eq!(
        line_of(&task(
            Some(Exit::Code(0)),
            Some(at(1_768_478_400)),
            Some(at(1_768_478_460))
        )),
        concat!(
            "job: idle (task, enabled=true, pid=-, last-result=exit code 0, ",
            "last-run=2026-01-15T12:00:00+00:00, next-run=2026-01-15T12:01:00+00:00)"
        )
    );
    assert_eq!(
        line_of(&task(Some(Exit::Signal(15)), None, None)),
        "job: idle (task, enabled=true, pid=-, last-result=signal 15, last-run=-, next-run=-)"
    );
    assert_eq!(
        line_of(&task(None, None, None)),
        "job: idle (task, enabled=true, pid=-, last-result=-, last-run=-, next-run=-)"
    );
}

/// The report carries the exact text for `--json`; this is the human sink, which escapes it, and
/// the id the line is prefixed with, which was typed on the command line.
#[skuld::test]
fn an_error_line_is_printed_with_control_and_direction_characters_escaped() {
    let (out, err) = rendered(Some("a\nb"), "id `a\nb\u{202e}` is refused");
    assert_eq!(out, "");
    assert_eq!(err, concat!(r"error: a\u{a}b: id `a\u{a}b\u{202e}` is refused", "\n"));

    let (_, err) = rendered(None, "no\u{1b}[2Jpe");
    assert_eq!(err, concat!(r"error: no\u{1b}[2Jpe", "\n"));
}

/// `status <id>` prints a child's stderr as the report does, and `--json` carries it as it came.
#[skuld::test]
fn a_failed_commands_stderr_is_printed_as_the_report_prints_it() {
    use crate::error::Error;
    use crate::error::error_tests::exit_status;

    let failed = || Error::command("systemctl show x", exit_status(1), b"first\nerror: forged\x1b");
    let e = failed();
    let report = Report {
        daemons: Vec::new(),
        errors: vec![report::status_error("x", failed())],
        undetermined: Vec::new(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    print_text(&report, &mut out, &mut err);
    assert_eq!(String::from_utf8(err).unwrap(), format!("error: x: {}\n", e.report()));
    assert!(
        e.report()
            .to_string()
            .ends_with(concat!(": first\nerror: forged", r"\u{1b}"))
    );
    assert_eq!(
        report.errors[0].message,
        format!(
            "`systemctl show x` failed ({}): first\nerror: forged\x1b",
            exit_status(1)
        )
    );
}
