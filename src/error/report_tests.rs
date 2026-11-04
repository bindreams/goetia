use super::*;
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
            blob: Box::new(undetermined("re-run as root")),
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
        blob: Box::new(undetermined("re-run as root")),
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
