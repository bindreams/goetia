use super::*;

fn error(kind: &'static str) -> ErrorReport {
    ErrorReport {
        id: None,
        kind,
        message: "why".to_string(),
    }
}

fn report(kinds: &[&'static str]) -> Report {
    Report {
        daemons: Vec::new(),
        errors: kinds.iter().map(|kind| error(kind)).collect(),
    }
}

#[skuld::test]
fn write_emits_one_compact_line_terminated_by_a_newline() {
    let report = Report {
        daemons: vec![DaemonReport {
            id: "frpc".to_string(),
            state: "running",
            enabled: true,
            pid: Some(1234),
        }],
        errors: vec![ErrorReport {
            id: Some("corrupt".to_string()),
            kind: kind::UNREADABLE,
            message: "why".to_string(),
        }],
    };

    let mut out = Vec::new();
    write(&report, &mut out);

    let text = String::from_utf8(out).expect("the document is UTF-8");
    assert_eq!(
        text,
        concat!(
            r#"{"daemons":[{"id":"frpc","state":"running","enabled":true,"pid":1234}],"#,
            r#""errors":[{"id":"corrupt","kind":"unreadable","message":"why"}]}"#,
            "\n"
        ),
        "compact (no spaces or indentation), one line, newline-terminated"
    );
}

/// The exit code is the precedence-max over `errors[].kind` — not "1 if
/// `errors` is non-empty", which would collapse the partial-answer case
/// (`4`) into the same code as an outright failure.
#[skuld::test]
fn exit_code_is_the_precedence_max_over_error_kinds() {
    for (kinds, expected) in [
        (&[][..], 0),
        (&[kind::UNREADABLE][..], 4),
        (&[kind::NOT_INSTALLED][..], 1),
        (&[kind::FOREIGN][..], 1),
        (&[kind::OTHER][..], 1),
        (&[kind::UNAVAILABLE][..], 1),
        (&[kind::INVALID_ID][..], 1),
        (&[kind::UNSUPPORTED][..], 2),
        // 1 outranks 4, in either order.
        (&[kind::UNREADABLE, kind::NOT_INSTALLED][..], 1),
        (&[kind::NOT_INSTALLED, kind::UNREADABLE][..], 1),
        // Repeats of the same kind stay that kind's code.
        (&[kind::UNREADABLE, kind::UNREADABLE][..], 4),
    ] {
        assert_eq!(exit_code(&report(kinds)), expected, "for {kinds:?}");
    }
}

/// Every kind must have a code: `code_for`'s fallback arm is a `debug_assert`
/// that this test would trip on a kind added without one.
#[skuld::test]
fn every_kind_has_a_code() {
    for kind in [
        kind::NOT_INSTALLED,
        kind::FOREIGN,
        kind::UNREADABLE,
        kind::INVALID_ID,
        kind::UNAVAILABLE,
        kind::UNSUPPORTED,
        kind::OTHER,
    ] {
        assert_ne!(exit_code(&report(&[kind])), 0, "`{kind}` must not exit 0");
    }
}
