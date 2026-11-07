use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::error::Reason;
use crate::error::error_tests::{id, os, undetermined};

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

/// Every error the CLI prints reaches the terminal through the report, so a value that came from a
/// manifest, an environment file or the command line cannot carry a control or format character
/// into it: not a newline, an escape sequence or a bidi override.
#[skuld::test]
fn a_report_escapes_control_and_format_characters_in_every_link_of_the_chain() {
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
    ];
    for e in &errors {
        let text = e.report().to_string();
        assert!(
            !text.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "a raw control or format character reached the report: {text:?}"
        );
        assert!(text.contains(r"\u{"), "{text:?}");
    }
    assert!(errors[0].report().to_string().ends_with(r"`a\u{a}b\u{202e}c`"));
}

#[skuld::test]
fn a_report_leaves_an_ordinary_windows_path_as_written() {
    // `\n` and `\u` here are a backslash and a letter, not escapes; they must stay as they are.
    let path = r"C:\Users\new\u{1b}\notes.txt";
    let e = Error::os_at("read", PathBuf::from(path), std::io::Error::other("denied"));
    assert_eq!(e.report().to_string(), format!("failed to read {path}: denied"));
}

#[skuld::test]
fn a_chain_and_a_reason_escape_as_a_report_does() {
    let e = os("read", "/a\nb", "denied");
    assert!(!Chain(&e).to_string().contains('\n'));
    let reason = Reason::failed(e);
    assert!(!reason.to_string().contains('\n'), "{reason}");
}
