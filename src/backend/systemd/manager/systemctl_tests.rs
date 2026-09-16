//! Unit coverage for the pure halves of `systemctl.rs`: the diagnostic
//! [`failed`] builds out of a [`Capture`], across the four shapes a capture
//! can arrive in.
//!
//! These are pure-function tests on purpose. `failed`'s whole job is the
//! wording of a message, and `tests/systemd_integration/linux.rs` can only
//! reach it through a `systemctl` that actually fails — which is how the
//! stdout fallback and both truncation messages shipped with no test at all.

use super::*;

fn capture(stdout: &str, stderr: &str, complete: bool) -> Capture {
    Capture {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        complete,
    }
}

// failed ==============================================================================================================

#[skuld::test]
fn a_failure_is_reported_with_what_systemctl_wrote_on_stderr() {
    let e = failed("start", "x.service", &capture("", "Unit x.service not found.\n", true));
    assert_eq!(
        e.to_string(),
        "systemctl start x.service failed: Unit x.service not found.\n"
    );
}

/// The stdout fallback. `systemctl` writes its failures to stderr, so a
/// capture with only stdout is the case nothing else in the tree produces —
/// and the case that used to render `failed: ` with nothing after the colon.
#[skuld::test]
fn a_failure_that_wrote_only_to_stdout_is_still_reported_with_what_it_wrote() {
    let e = failed("stop", "x.service", &capture("something on stdout\n", "", true));
    assert_eq!(e.to_string(), "systemctl stop x.service failed: something on stdout\n");
}

/// stderr wins when both streams carried something: that is where
/// `systemctl` puts the diagnostic, and the fallback exists for the other
/// case only.
#[skuld::test]
fn stderr_is_preferred_over_stdout_when_both_carry_something() {
    let e = failed("start", "x.service", &capture("noise\n", "the real diagnostic\n", true));
    assert_eq!(e.to_string(), "systemctl start x.service failed: the real diagnostic\n");
}

/// A truncated capture must say so: a prefix presented as the whole story
/// reads as "systemd said this and stopped", when what happened is that
/// goetia stopped reading.
#[skuld::test]
fn a_truncated_diagnostic_says_it_is_a_prefix() {
    let e = failed("start", "x.service", &capture("", "Unit x.ser", false));
    let msg = e.to_string();
    assert!(msg.contains("Unit x.ser"), "{msg}");
    assert!(msg.contains("truncated"), "{msg}");
    assert!(msg.contains("budget expired"), "{msg}");
}

/// Both empty, under either `complete`. An empty diagnostic renders
/// `systemctl start x.service failed: ` with nothing after the colon, which
/// is the shape the guard exists to prevent — and it must not depend on
/// *why* both streams are empty. A complete capture that is empty says
/// "systemd wrote nothing"; a truncated one says "goetia's budget expired
/// first"; neither may trail off after a colon.
#[skuld::test]
fn an_empty_diagnostic_is_explained_rather_than_trailing_off_after_a_colon() {
    for complete in [true, false] {
        let msg = failed("start", "x.service", &capture("", "", complete)).to_string();
        assert!(
            !msg.ends_with(": "),
            "an empty diagnostic must not trail off after a colon (complete={complete}): {msg:?}"
        );
        assert!(
            msg.contains("systemctl start x.service failed"),
            "complete={complete}: {msg}"
        );
    }
}
