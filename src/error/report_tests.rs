use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::error::Reason;
use crate::error::error_tests::{exit_status, id, os, undetermined};

/// The failed read sits inside the sentence of the error that embeds it: its remedy must not
/// interrupt that sentence, and still prints once, last.
#[skuld::test]
fn an_embedded_errors_remedy_prints_last_not_inside_the_sentence() {
    let embedding = [
        Error::LoadedStateInDoubt {
            id: id(),
            read_failed: Box::new(undetermined("re-run as root")),
            restore_failed: Some(Box::new(os("write", "/q", "disk full"))),
        },
        Error::StartAsManaged {
            blob: Arc::new(undetermined("re-run as root")),
            source: Box::new(os("start", "/q", "stopped")),
        },
    ];
    for e in &embedding {
        let text = e.report().to_string();
        assert_eq!(text.matches("re-run as root").count(), 1, "{text}");
        assert!(text.ends_with(". re-run as root"), "{text}");
        assert!(text.contains("failed to read /p: denied)"), "{text}");
        assert!(!e.to_string().contains("re-run as root"), "Display drops it: {e}");
        assert_eq!(e.remedy(), None, "the embedding error has no remedy of its own");
    }
}

#[skuld::test]
fn a_remedy_met_both_embedded_and_down_the_chain_prints_once() {
    let e = Error::StartAsManaged {
        blob: Arc::new(undetermined("re-run as root")),
        source: Box::new(undetermined("re-run as root")),
    };
    let text = e.report().to_string();
    assert_eq!(text.matches("re-run as root").count(), 1, "{text}");
}

#[skuld::test]
fn a_chain_is_written_without_remedies() {
    let e = undetermined("re-run as root");
    assert_eq!(
        Chain(&e).to_string(),
        "cannot determine whether daemon `x` is installed: failed to read /p: denied"
    );
}

// The sink ------------------------------------------------------------------------------------------------------------

fn command(stderr: &str) -> Error {
    Error::Command {
        command: "systemctl start x".to_string(),
        status: exit_status(1),
        stderr: stderr.to_string(),
    }
}

/// Every link of a chain reaches a person through the report, so a value that came from a manifest,
/// an environment file or the command line cannot carry a control or direction character into it.
#[skuld::test]
fn a_report_escapes_control_and_direction_characters_in_every_link_of_the_chain() {
    let errors = [
        Error::Invalid {
            daemon: "x".to_string(),
            message: "field `restart` is `a\nb\u{202e}c`".to_string(),
            source: None,
        },
        Error::os_at("read", PathBuf::from("/a\nb\u{202e}c"), std::io::Error::other("x")),
        os("read", "/p", "cause\u{1b}[0m\u{202e}"),
        Error::Step {
            what: "step\r".to_string(),
            source: Box::new(os("read", "/p", "denied")),
        },
        undetermined("re-run\u{2028}as root"),
    ];
    for e in &errors {
        let text = e.report().to_string();
        assert!(
            !text
                .chars()
                .any(|c| c.is_control() || c == '\u{202e}' || c == '\u{2028}'),
            "a raw character reached the report: {text:?}"
        );
        assert!(text.contains(r"\u{"), "{text:?}");
    }
    assert!(errors[0].report().to_string().ends_with(r"`a\u{a}b\u{202e}c`"));
}

#[skuld::test]
fn a_report_leaves_a_windows_path_and_a_persian_name_as_written() {
    // `\n` and `\u` are a backslash and a letter here, not escapes.
    let path = r"C:\Users\new\u{1b}\notes.txt";
    let e = Error::os_at("read", PathBuf::from(path), std::io::Error::other("denied"));
    assert_eq!(e.report().to_string(), format!("failed to read {path}: denied"));

    let name = "می\u{200c}خواهم";
    let e = Error::Step {
        what: format!("cannot start `{name}`"),
        source: Box::new(os("read", "/p", "denied")),
    };
    assert_eq!(
        e.report().to_string(),
        format!("cannot start `{name}`: failed to read /p: denied")
    );
}

/// A child's diagnostic is multi-line by nature: the report keeps its line feeds and tabs, indents
/// the lines after the first, and the variant's own text holds none of it.
#[skuld::test]
fn a_commands_stderr_keeps_its_layout_and_loses_the_rest_of_the_set() {
    let e = command("first\n\tsecond\u{1b}[31m\u{202e}\r");
    assert_eq!(
        e.to_string(),
        format!("`systemctl start x` failed ({})", exit_status(1))
    );
    assert_eq!(
        e.report().to_string(),
        format!(
            r"`systemctl start x` failed ({}): first{}{}second\u{{1b}}[31m\u{{202e}}\u{{d}}",
            exit_status(1),
            "\n  ",
            "\t"
        )
    );
}

/// `--json` carries the exact text: serde encodes it, so escaping first would encode it twice.
#[skuld::test]
fn the_unescaped_report_carries_every_character_as_it_is() {
    let e = Error::Step {
        what: "step\u{202e}".to_string(),
        source: Box::new(command("a\u{1b}b")),
    };
    assert_eq!(
        e.report_unescaped().to_string(),
        format!(
            "step\u{202e}: `systemctl start x` failed ({}): a\u{1b}b",
            exit_status(1)
        )
    );
}

/// An error quoted inside another's sentence is text the sink will write: it stays as it is until
/// it does. A [`Reason`] is a sink of its own, which prints as the report does.
#[skuld::test]
fn a_chain_leaves_escaping_to_the_sink_and_a_reason_is_one() {
    let e = os("read", "/a\nb", "denied");
    assert!(Chain(&e).to_string().contains('\n'));
    assert!(!Reason::failed(e).to_string().contains('\n'));
}

/// sc.exe and other Windows tools write CRLF: each line ends as it was written.
#[skuld::test]
fn a_commands_stderr_keeps_a_carriage_return_that_ends_a_line() {
    let text = command("one\r\ntwo\rthree").report().to_string();
    assert!(text.ends_with(concat!(": one\r\n  two", r"\u{d}", "three")), "{text:?}");
}

/// Goetia's own lines start at column 0, so no line of a child's stderr after the first may: a
/// stderr that says `error: ...` would otherwise read as goetia saying it.
#[skuld::test]
fn no_line_of_a_commands_stderr_starts_where_goetias_own_lines_do() {
    let e = command("first\nerror: forged\n\n\tindented\r\nwarning: forged");
    let text = e.report().to_string();
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines.len(), 5, "{text:?}");
    for line in &lines[1..] {
        assert!(
            line.trim_end_matches('\r').is_empty() || line.starts_with("  "),
            "{line:?} in {text:?}"
        );
    }
    assert!(text.contains("\n  error: forged\n"), "{text:?}");
    // Written for JSON, the text is as it came.
    assert!(
        e.report_unescaped()
            .to_string()
            .ends_with("first\nerror: forged\n\n\tindented\r\nwarning: forged")
    );
}

/// A [`Reason`] prints as the report does, a child's stderr included.
#[skuld::test]
fn a_reason_wrapping_a_command_error_prints_its_stderr_as_the_report_does() {
    let e = command("first\nsecond");
    let expected = e.report().to_string();
    assert!(expected.ends_with(": first\n  second"), "{expected:?}");
    assert_eq!(Reason::failed(e).to_string(), expected);
}

/// Goetia's own text never continues on a line of a child's stderr: after a multi-line block it
/// starts a line of its own, whichever part of the report it is.
#[skuld::test]
fn goetias_text_after_a_stderr_block_starts_a_new_line() {
    let status = exit_status(1);
    let block = format!("`systemctl start x` failed ({status}): a\n  b");

    // A `Both`'s `; then:`.
    let both = Error::Both {
        first: Box::new(command("a\nb")),
        then: Box::new(os("read", "/p", "denied")),
    };
    assert_eq!(
        both.report().to_string(),
        format!("{block}\n; then: failed to read /p: denied")
    );

    // A remedy.
    let remedied = Error::Both {
        first: Box::new(undetermined("re-run as root")),
        then: Box::new(command("a\nb")),
    };
    assert!(
        remedied
            .report()
            .to_string()
            .ends_with(&format!("{block}\n. re-run as root")),
        "{}",
        remedied.report()
    );

    // An `Around` reason's `after`, and then its remedy.
    let around = Reason::around(
        "while starting: ",
        Error::Both {
            first: Box::new(undetermined("re-run as root")),
            then: Box::new(command("a\nb")),
        },
        " (see above)",
    );
    assert!(
        around
            .to_string()
            .ends_with(&format!("{block}\n (see above). re-run as root")),
        "{around}"
    );

    // The `: source` chain.
    let sourced = Error::StartAsManaged {
        blob: Arc::new(command("a\nb")),
        source: Box::new(os("start", "/q", "stopped")),
    };
    assert!(
        sourced
            .report()
            .to_string()
            .ends_with("\n): failed to start /q: stopped"),
        "{}",
        sourced.report()
    );
}

/// A one-line stderr has no indented line, so what follows it stays on its line; a block that
/// already ends in a line feed needs no second one.
#[skuld::test]
fn only_a_block_that_does_not_end_the_line_breaks_it() {
    let status = exit_status(1);
    let both = |stderr| Error::Both {
        first: Box::new(command(stderr)),
        then: Box::new(os("read", "/p", "denied")),
    };
    assert_eq!(
        both("a").report().to_string(),
        format!("`systemctl start x` failed ({status}): a; then: failed to read /p: denied")
    );
    assert_eq!(
        both("a\nb\n").report().to_string(),
        format!("`systemctl start x` failed ({status}): a\n  b\n; then: failed to read /p: denied")
    );
}

/// An error set inside another's sentence is written as the report writes any error: its stderr
/// keeps its layout, and the rest of the sentence starts a line of its own.
#[skuld::test]
fn an_embedded_errors_stderr_keeps_its_layout() {
    let status = exit_status(1);
    let e = Error::StartAsManaged {
        blob: Arc::new(command("a\nerror: forged")),
        source: Box::new(os("start", "/q", "stopped")),
    };
    let text = e.report().to_string();
    assert!(
        text.contains(&format!(
            "judged as a `type: managed` service's (`systemctl start x` failed ({status}): a\n  error: forged\n): "
        )),
        "{text:?}"
    );
    let e = Error::LoadedStateInDoubt {
        id: "x".to_string(),
        read_failed: Box::new(command("a\nb")),
        restore_failed: None,
    };
    let text = e.report().to_string();
    assert!(
        text.contains(&format!(
            "(`systemctl start x` failed ({status}): a\n  b\n), so goetia cannot tell"
        )),
        "{text:?}"
    );
    // `Display` and the report for JSON keep it as it came.
    assert!(e.to_string().contains(": a\nb), so goetia"), "{e}");
    assert!(e.report_unescaped().to_string().contains(": a\nb), so goetia"));
}

/// The report as written into a sink whose lines start with `prefix`, as the CLI's `error:` lines do.
fn under(prefix: &str, e: &Error) -> String {
    let mut text = String::new();
    e.write_report(&mut Sink::new(&mut text, true).continuing_with(prefix))
        .expect("writing to a String cannot fail");
    text
}

/// A kept error is handed out as it was: a kept `Both` prints both halves, and a remedy in either.
#[skuld::test]
fn a_kept_both_prints_both_halves_and_its_remedies() {
    let both = || Error::Both {
        first: Box::new(os("read", "/p", "denied")),
        then: Box::new(undetermined("re-run as root")),
    };
    let expected = both().report().to_string();
    assert!(expected.contains("; then: "), "{expected}");
    assert!(expected.ends_with(". re-run as root"), "{expected}");
    let kept = Error::Kept(Arc::new(both()));
    assert_eq!(kept.report().to_string(), expected);
    let twice = Error::Kept(Arc::new(kept));
    assert_eq!(twice.report().to_string(), expected);
}

/// After a stderr block, goetia's continuation starts with the prefix the message's first line has,
/// so a script that keeps the `error:` lines keeps the second failure and the remedy.
#[skuld::test]
fn goetias_continuation_after_a_stderr_block_starts_with_the_messages_prefix() {
    let status = exit_status(1);
    let block = format!("`systemctl start x` failed ({status}): a\n  b");
    let prefix = "error: frpc: ";

    let both = Error::Both {
        first: Box::new(command("a\nb")),
        then: Box::new(os("read", "/p", "denied")),
    };
    assert_eq!(
        under(prefix, &both),
        format!("{block}\n{prefix}; then: failed to read /p: denied")
    );

    let remedied = Error::Both {
        first: Box::new(undetermined("re-run as root")),
        then: Box::new(command("a\nb")),
    };
    assert!(
        under(prefix, &remedied).ends_with(&format!("{block}\n{prefix}. re-run as root")),
        "{}",
        under(prefix, &remedied)
    );

    let sourced = Error::StartAsManaged {
        blob: Arc::new(command("a\nb")),
        source: Box::new(os("start", "/q", "stopped")),
    };
    assert!(
        under(prefix, &sourced).ends_with(&format!("\n{prefix}): failed to start /q: stopped")),
        "{}",
        under(prefix, &sourced)
    );

    // A line that has no stderr block before it is not a continuation.
    let plain = os("read", "/p", "denied");
    assert_eq!(under(prefix, &plain), plain.report().to_string());

    // The unescaped writer keeps the text as it came and adds nothing.
    let mut raw = String::new();
    both.write_report(&mut Sink::new(&mut raw, false).continuing_with(prefix))
        .unwrap();
    assert_eq!(raw, both.report_unescaped().to_string());
}

/// A reason around an error is one sentence, written as an error's own sentence is.
#[skuld::test]
fn a_reason_around_an_error_continues_under_the_prefix_too() {
    let around = Reason::around(
        "while starting: ",
        Error::Both {
            first: Box::new(undetermined("re-run as root")),
            then: Box::new(command("a\nb")),
        },
        " (see above)",
    );
    let mut text = String::new();
    around
        .write_to(&mut Sink::new(&mut text, true).continuing_with("warning: x: "))
        .unwrap();
    assert!(
        text.ends_with("a\n  b\nwarning: x:  (see above). re-run as root"),
        "{text:?}"
    );
}

/// An error's `Display` is its sentence, so the report and `Display` cannot word it differently.
#[skuld::test]
fn display_of_a_sentence_variant_is_its_sentence() {
    let e = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(os("read", "/p", "denied")),
        restore_failed: None,
    };
    assert_eq!(e.to_string(), e.sentence().unwrap().to_string());
    assert_eq!(e.report().to_string(), e.to_string());
}
