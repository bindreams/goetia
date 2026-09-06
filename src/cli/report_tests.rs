use super::*;

fn report(kinds: &[Kind]) -> Report {
    Report {
        daemons: Vec::new(),
        errors: kinds
            .iter()
            .map(|kind| ErrorReport {
                id: None,
                kind: *kind,
                message: "why".to_string(),
            })
            .collect(),
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
            kind: Kind::Unreadable,
            message: "why".to_string(),
        }],
    };

    let mut out = Vec::new();
    write(&report, &mut out).expect("a Vec never fails to accept a write");

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
    // Not a hand-list standing in for completeness: `Kind::code` is an
    // exhaustive match, so a kind added without a code does not compile.
    for (kinds, expected) in [
        (&[][..], 0),
        (&[Kind::Unreadable][..], 4),
        (&[Kind::NotInstalled][..], 1),
        (&[Kind::Foreign][..], 1),
        (&[Kind::Other][..], 1),
        (&[Kind::Unavailable][..], 1),
        (&[Kind::InvalidId][..], 1),
        (&[Kind::Unsupported][..], 2),
        // 1 outranks 4, in either order.
        (&[Kind::Unreadable, Kind::NotInstalled][..], 1),
        (&[Kind::NotInstalled, Kind::Unreadable][..], 1),
        // Repeats of the same kind stay that kind's code.
        (&[Kind::Unreadable, Kind::Unreadable][..], 4),
    ] {
        assert_eq!(exit_code(&report(kinds)), expected, "for {kinds:?}");
    }
}

/// The wire spelling is the stable contract, and `Serialize` goes through
/// `as_str`, so pinning `as_str` pins the JSON.
#[skuld::test]
fn every_kind_serializes_to_its_documented_wire_spelling() {
    for (kind, spelling) in [
        (Kind::NotInstalled, "not-installed"),
        (Kind::Foreign, "foreign"),
        (Kind::Unreadable, "unreadable"),
        (Kind::InvalidId, "invalid-id"),
        (Kind::Unavailable, "unavailable"),
        (Kind::Unsupported, "unsupported"),
        (Kind::Other, "other"),
    ] {
        assert_eq!(kind.as_str(), spelling);
        assert_eq!(
            serde_json::to_string(&kind).expect("a Kind serializes infallibly"),
            format!("\"{spelling}\"")
        );
    }
}

// emit ================================================================================================================

/// Refuses every write, the way a broken pipe (`goetia --json daemon list | head -1`) or a full
/// disk does.
struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected write failure",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Accepts every write and fails only on `flush`, the way a buffered writer does when the pipe
/// closes after the last `write` call returned `Ok`. This is the case the flush in [`emit`] exists
/// for: without it a document that never left the buffer would be certified as delivered.
struct FailingFlush;

impl std::io::Write for FailingFlush {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected flush failure",
        ))
    }
}

/// A buffered write that succeeds and then fails to reach the far end is still an undelivered
/// document. Without this the flush in [`emit`] would be justified only by its doc comment.
#[skuld::test]
fn emit_exits_one_when_only_the_flush_fails() {
    let report = report(&[]);
    assert_eq!(exit_code(&report), 0, "the report's own code, before the flush fails");

    let mut err = Vec::new();
    let code = emit(&report, &mut FailingFlush, &mut err);

    assert_eq!(code, 1);
    let text = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(text.contains("injected flush failure"), "{text}");
}

/// The exit code certifies that the document was delivered, so an undelivered one cannot exit `0`.
/// Deliberately built from a report whose own [`exit_code`] *is* `0`: that is the case a consumer
/// following "parse stdout first, then read `errors`" would meet as `json.loads("")`.
#[skuld::test]
fn emit_exits_one_when_the_document_cannot_be_written() {
    let report = report(&[]);
    assert_eq!(exit_code(&report), 0, "the report's own code, before the write fails");

    let mut err = Vec::new();
    let code = emit(&report, &mut FailingWriter, &mut err);

    assert_eq!(code, 1);
    let text = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(text.contains("injected write failure"), "{text}");
}

/// A write failure outranks whatever the report would have said on its own — including `2`, which
/// [`precedence`] otherwise ranks above everything.
#[skuld::test]
fn a_write_failure_replaces_the_reports_own_code_whatever_it_was() {
    for kinds in [
        &[Kind::Unsupported][..],
        &[Kind::Unreadable][..],
        &[Kind::NotInstalled][..],
    ] {
        let report = report(kinds);
        let code = emit(&report, &mut FailingWriter, &mut Vec::new());
        assert_eq!(code, 1, "{:?}", kinds[0]);
    }
}

#[skuld::test]
fn emit_returns_the_reports_own_code_when_the_write_succeeds() {
    let report = report(&[Kind::Unreadable]);
    let mut out = Vec::new();
    let mut err = Vec::new();

    let code = emit(&report, &mut out, &mut err);

    assert_eq!(code, exit_code(&report));
    assert_eq!(code, 4);
    assert!(!out.is_empty(), "the document is on stdout");
    assert!(err.is_empty(), "nothing is said about a write that worked");
}
