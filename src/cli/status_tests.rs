use super::*;
use crate::cli::report::{ErrorReport, Kind};

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

    let e = Error::command("systemctl show x", exit_status(1), b"first\nerror: forged\x1b");
    let report = Report {
        daemons: Vec::new(),
        errors: vec![report::status_error("x", &e)],
        undetermined: Vec::new(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    print_text(&report, &mut out, &mut err);
    assert_eq!(String::from_utf8(err).unwrap(), format!("error: x: {}\n", e.report()));
    assert!(
        e.report()
            .to_string()
            .ends_with(concat!(": first\n  error: forged", r"\u{1b}"))
    );
    assert_eq!(
        report.errors[0].message,
        format!(
            "`systemctl show x` failed ({}): first\nerror: forged\x1b",
            exit_status(1)
        )
    );
}
