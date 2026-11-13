use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::error::Reason;
use crate::error::error_tests::{exit_status, id, os, undetermined};

/// A remedy is its own sentence: it follows a period the words before it already ended with, and
/// supplies one when they did not.
#[skuld::test]
fn a_remedy_follows_the_closing_period_of_the_words_before_it_without_doubling_it() {
    for (words, expected) in [("it broke", "it broke. fix it"), ("it broke.", "it broke. fix it")] {
        let mut text = String::new();
        let mut sink = Sink::new(&mut text, true);
        sink.write_str(words).unwrap();
        sink.write_remedy("fix it").unwrap();
        assert_eq!(text, expected);
    }
}

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

/// `!` and `?` end a sentence as `.` does, and each remedy is the words the next one follows: a
/// remedy ending in a period is not followed by a doubled one.
#[skuld::test]
fn remedies_join_after_any_sentence_terminator_and_each_after_the_one_before() {
    for (words, expected) in [("what now?", "what now? fix it"), ("it broke!", "it broke! fix it")] {
        let mut text = String::new();
        let mut sink = Sink::new(&mut text, true);
        sink.write_str(words).unwrap();
        sink.write_remedy("fix it").unwrap();
        assert_eq!(text, expected);
    }
    let mut text = String::new();
    let mut sink = Sink::new(&mut text, true);
    sink.write_str("it broke").unwrap();
    write_remedies(&mut sink, &["fix it.", "then retry", "or ask?", "ok"]).unwrap();
    assert_eq!(text, "it broke. fix it. then retry. or ask? ok");
}

/// A child's stderr is the last words before the remedy, so its closing period is the sentence's.
#[skuld::test]
fn a_remedy_follows_a_childs_stderr_that_ends_in_a_period_without_doubling_it() {
    let e = Error::Undetermined {
        id: id(),
        reason: Box::new(command("boom.")),
        recovery: "re-run as root".to_string(),
    };
    let report = e.report().to_string();
    assert!(report.ends_with("boom. re-run as root"), "{report}");
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

/// A child's diagnostic is multi-line by nature: the report keeps its line feeds and tabs at
/// column 0, and the variant's own text holds none of it.
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
            r"`systemctl start x` failed ({}): first{}second\u{{1b}}[31m\u{{202e}}\u{{d}}",
            exit_status(1),
            "\n\t"
        )
    );
}

/// The exact text for `--json`.
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
    assert!(text.ends_with(concat!(": one\r\ntwo", r"\u{d}", "three")), "{text:?}");
}

/// A [`Reason`] prints as the report does, a child's stderr included.
#[skuld::test]
fn a_reason_wrapping_a_command_error_prints_its_stderr_as_the_report_does() {
    let e = command("first\nsecond");
    let expected = e.report().to_string();
    assert!(expected.ends_with(": first\nsecond"), "{expected:?}");
    assert_eq!(Reason::failed(e).to_string(), expected);
}

/// An error set inside another's sentence is written as the report writes any error: its stderr
/// keeps its layout, and the rest of the sentence continues after it.
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
            "judged as a `type: managed` service's (`systemctl start x` failed ({status}): a\nerror: forged): "
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
            "(`systemctl start x` failed ({status}): a\nb), so goetia cannot tell"
        )),
        "{text:?}"
    );
    // The report for JSON keeps it as it came.
    assert!(e.report_unescaped().to_string().contains(": a\nb), so goetia"));
}

/// Only the report writes a child's stderr: a sentence variant's `Display` leaves out the stderr
/// of the error inside it, as a failed command's own `Display` does.
#[skuld::test]
fn display_of_a_sentence_variant_leaves_out_the_embedded_errors_stderr() {
    let status = exit_status(1);
    let blob = Error::StartAsManaged {
        blob: Arc::new(command("secret\ndiagnostic")),
        source: Box::new(os("start", "/q", "stopped")),
    };
    let loaded = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(command("secret\ndiagnostic")),
        restore_failed: None,
    };
    for e in [&blob, &loaded] {
        assert!(
            e.to_string()
                .contains(&format!("`systemctl start x` failed ({status})")),
            "{e}"
        );
        assert!(!e.to_string().contains("secret"), "{e}");
        assert!(e.report().to_string().contains("secret\ndiagnostic"));
    }
    assert!(!Chain(&command("secret")).to_string().contains("secret"));
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

/// The report and `Display` word a sentence variant alike: they differ only by the embedded
/// error's stderr, which only the report writes.
#[skuld::test]
fn display_of_a_sentence_variant_and_its_report_agree_on_the_words() {
    let blob = Error::StartAsManaged {
        blob: Arc::new(os("read", "/p", "denied")),
        source: Box::new(os("start", "/q", "stopped")),
    };
    let loaded = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(os("read", "/p", "denied")),
        restore_failed: None,
    };
    for e in [&blob, &loaded] {
        // No stderr and no source in the first: the report is `Display`, then the source if any.
        let report = e.report().to_string();
        assert!(report.starts_with(&e.to_string()), "{report}");
    }
    assert_eq!(loaded.report().to_string(), loaded.to_string());

    // With stderr in the embedded error, the report is `Display` with the stderr put in.
    let loaded = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(command("diagnostic")),
        restore_failed: None,
    };
    let (display, report) = (loaded.to_string(), loaded.report().to_string());
    assert_eq!(report.replacen(": diagnostic", "", 1), display);
}

/// A goetia error holding a `cosca` error prints it as `cosca::error::Report` does.
#[skuld::test]
fn a_cosca_error_below_a_goetia_error_is_written_as_cosca_reports_it() {
    let cosca_error = || cosca::error::Error::MayHaveStarted {
        fate: cosca::error::ChildFate::Unknown,
        wrapper_elevated: false,
        detail: Some("the pipe broke".to_string()),
        source: Arc::new(cosca::error::Error::Io(std::io::Error::other("no pipe"))),
        unreaped: None,
        cleanup: None,
    };
    let expected = cosca::error::Report::new(&cosca_error()).to_string();
    let text = Error::spawn("tool", cosca_error()).report().to_string();

    assert_eq!(text, format!("failed to run `tool`: {expected}"));
}

// os_error_in ---------------------------------------------------------------------------------------------------------

/// `ERROR_ACCESS_DENIED` and `ERROR_PATH_NOT_FOUND`.
const DENIED: i32 = 5;
const PATH_NOT_FOUND: i32 = 3;

fn io_with_code(code: i32) -> cosca::error::Error {
    cosca::error::Error::Io(std::io::Error::from_raw_os_error(code))
}

fn code_less() -> cosca::error::Error {
    cosca::error::Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound))
}

#[skuld::test]
fn the_os_error_of_an_io_failure_is_kept_and_other_failures_have_none() {
    assert_eq!(os_error_in(&io_with_code(DENIED)), Some(DENIED));
    // A failure without a code has none, whatever its kind.
    assert_eq!(os_error_in(&code_less()), None);
    assert_eq!(
        os_error_in(&cosca::error::Error::Unassessable {
            detail: "query".to_string(),
            source: Some(Arc::new(code_less())),
        }),
        None
    );
    assert_eq!(
        os_error_in(&cosca::error::Error::Io(std::io::Error::other("no os code"))),
        None
    );
    assert_eq!(
        os_error_in(&cosca::error::Error::Containment {
            detail: "attach".to_string(),
            source: None,
        }),
        None
    );
}

/// `cosca` wraps every failure after `CreateProcess` in `MayHaveStarted`; the code is the wrapped
/// failure's.
#[skuld::test]
fn the_os_error_of_a_failure_after_the_spawn_is_the_one_of_its_source() {
    let after_spawn = |source: cosca::error::Error| cosca::error::Error::MayHaveStarted {
        fate: cosca::error::ChildFate::Unknown,
        wrapper_elevated: false,
        detail: None,
        source: Arc::new(source),
        unreaped: None,
        cleanup: None,
    };

    assert_eq!(os_error_in(&after_spawn(io_with_code(DENIED))), Some(DENIED));
    assert_eq!(
        os_error_in(&after_spawn(cosca::error::Error::Containment {
            detail: "attach".to_string(),
            source: None,
        })),
        None
    );
}

/// `cosca` keeps the OS error at the bottom of the `source()` chain, below however many layers a call
/// added; the code is that error's, whichever layers lie above it.
#[skuld::test]
fn the_os_error_of_a_failure_is_found_at_any_depth_of_its_causes() {
    let containment = |source| cosca::error::Error::Containment {
        detail: "kill".to_string(),
        source: Some(Arc::new(source)),
    };
    let unassessable = |source| cosca::error::Error::Unassessable {
        detail: "kill".to_string(),
        source: Some(Arc::new(source)),
    };
    let not_killed = |source| cosca::error::Error::MemberNotKilled {
        pid: 1,
        source: Arc::new(source),
    };

    assert_eq!(os_error_in(&containment(io_with_code(DENIED))), Some(DENIED));
    assert_eq!(
        os_error_in(&unassessable(not_killed(containment(io_with_code(DENIED))))),
        Some(DENIED)
    );
    let after_spawn = cosca::error::Error::MayHaveStarted {
        fate: cosca::error::ChildFate::Unknown,
        wrapper_elevated: false,
        detail: None,
        source: Arc::new(unassessable(io_with_code(DENIED))),
        unreaped: None,
        cleanup: None,
    };
    assert_eq!(os_error_in(&after_spawn), Some(DENIED));
    // A code-less layer above the code does not hide it.
    assert_eq!(
        os_error_in(&containment(io_with_code(PATH_NOT_FOUND))),
        Some(PATH_NOT_FOUND)
    );
    // A chain with no code anywhere has none.
    assert_eq!(os_error_in(&unassessable(containment(code_less()))), None);
}
