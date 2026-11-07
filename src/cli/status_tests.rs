use super::*;
use crate::cli::report::{ErrorReport, Kind};

fn rendered(id: Option<&str>, message: &str) -> (String, String) {
    let report = Report {
        daemons: Vec::new(),
        errors: vec![ErrorReport {
            id: id.map(str::to_string),
            kind: Kind::InvalidId,
            message: message.to_string(),
        }],
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
