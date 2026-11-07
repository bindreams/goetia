use super::*;
use crate::error::error_tests::{not_installed, os, undetermined};

#[skuld::test]
fn a_reason_reads_as_its_words_and_its_error_report() {
    assert_eq!(Reason::said("present in both").to_string(), "present in both");
    assert_eq!(
        Reason::failed(os("read", "/p", "denied")).to_string(),
        "failed to read /p: denied"
    );
    assert_eq!(
        Reason::because("cannot regenerate", os("read", "/p", "denied")).to_string(),
        "cannot regenerate: failed to read /p: denied"
    );
    assert_eq!(
        Reason::around("scan failed (", os("read", "/p", "denied"), "). Ownership is unknown.").to_string(),
        "scan failed (failed to read /p: denied). Ownership is unknown."
    );
}

#[skuld::test]
fn a_reason_prints_the_remedy_of_the_error_it_holds_last() {
    assert_eq!(
        Reason::failed(undetermined("re-run as root")).to_string(),
        "cannot determine whether daemon `x` is installed: failed to read /p: denied. re-run as root"
    );
}

/// The error sits inside the reason's own words, so its remedy waits for the end of them.
#[skuld::test]
fn a_reason_around_an_error_prints_the_remedy_after_its_closing_words() {
    assert_eq!(
        Reason::around(
            "scan failed (",
            undetermined("re-run as root"),
            "). Ownership is unknown."
        )
        .to_string(),
        "scan failed (cannot determine whether daemon `x` is installed: failed to read /p: denied). Ownership is \
         unknown.. re-run as root"
    );
}

#[skuld::test]
fn a_reason_gives_the_error_classification_reads() {
    assert!(Reason::said("words").cause().is_none());
    let step = Reason::because("outer", not_installed());
    assert!(matches!(step.cause(), Some(Error::NotInstalled { .. })));
    let around = Reason::around("(", not_installed(), ")");
    assert!(matches!(around.cause(), Some(Error::NotInstalled { .. })));
}

#[skuld::test]
fn two_reasons_are_equal_when_they_read_the_same() {
    let reason = || Reason::failed(os("read", "/p", "denied"));
    assert_eq!(reason(), reason().clone());
    assert_ne!(reason(), Reason::said("something else"));
    assert_eq!(Reason::said("failed to read /p: denied"), reason());
}

const HOSTILE: &str = "/a\nb\u{1b}[31m\u{202e}";
const HOSTILE_ESCAPED: &str = r"/a\u{a}b\u{1b}[31m\u{202e}";

/// Whatever travels as a `Reason` is printed raw as `{reason}`, so its `Display` is the sink.
#[skuld::test]
fn a_reason_escapes_every_part_of_what_it_prints_as_the_report_does() {
    assert_eq!(
        Reason::said(format!("path {HOSTILE}")).to_string(),
        format!("path {HOSTILE_ESCAPED}")
    );
    assert_eq!(
        Reason::failed(os("read", HOSTILE, "denied")).to_string(),
        format!("failed to read {HOSTILE_ESCAPED}: denied")
    );
    assert_eq!(
        Reason::around(
            format!("scan of {HOSTILE} failed ("),
            os("read", HOSTILE, "denied"),
            format!("). Stopped at {HOSTILE}.")
        )
        .to_string(),
        format!(
            "scan of {HOSTILE_ESCAPED} failed (failed to read {HOSTILE_ESCAPED}: denied). Stopped at \
             {HOSTILE_ESCAPED}."
        )
    );
}

/// The exact text for `--json`.
#[skuld::test]
fn an_unescaped_reason_carries_every_character_as_it_is() {
    assert_eq!(Reason::said(HOSTILE).unescaped().to_string(), HOSTILE);
    assert_eq!(
        Reason::failed(os("read", HOSTILE, "denied")).unescaped().to_string(),
        format!("failed to read {HOSTILE}: denied")
    );
    assert_eq!(
        Reason::around(HOSTILE, os("read", HOSTILE, "denied"), HOSTILE)
            .unescaped()
            .to_string(),
        format!("{HOSTILE}failed to read {HOSTILE}: denied{HOSTILE}")
    );
}

#[skuld::test]
fn reasons_that_differ_only_in_what_escaping_hides_are_not_equal() {
    assert_ne!(Reason::said("a\nb"), Reason::said(r"a\u{a}b"));
}
