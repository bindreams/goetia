//! Unit coverage for the pure launchd state classification in
//! `state.rs` — the whole reason this module is not `#[cfg]`-gated: these
//! tests run on every host this crate builds on, not only macOS.

use super::*;
use crate::manager::State;

/// A `launchctl print` body shaped like the real thing: tab-indented
/// `key = value` lines, with header fields (`active count`, `path`, `type`)
/// ahead of `state`, and `pid` further down still — [`classify`]'s two
/// fields of interest are not adjacent in the real output.
fn print_body(state: &str, pid: Option<u32>) -> String {
    let pid_line = match pid {
        Some(pid) => format!("\tpid = {pid}\n"),
        None => String::new(),
    };
    format!(
        "system/com.example.daemon = {{\n\
         \tactive count = 1\n\
         \tpath = /Library/LaunchDaemons/com.example.daemon.plist\n\
         \ttype = LaunchDaemon\n\
         \tstate = {state}\n\
         \n\
         \tprogram = /usr/local/bin/example-daemon\n\
         {pid_line}\
         }}\n"
    )
}

// classify ============================================================================================================

#[skuld::test]
fn running_is_running() {
    assert_eq!(classify(&print_body("running", Some(4242))).0, State::Running);
}

#[skuld::test]
fn xpcproxy_with_a_pid_is_running() {
    // The exact shape all 10 observed CI failures had.
    let body = print_body("xpcproxy", Some(39459));
    assert!(body.contains("active count = 1"));
    assert!(body.contains("state = xpcproxy"));
    assert!(body.contains("pid = 39459"));
    assert_eq!(classify(&body).0, State::Running);
}

#[skuld::test]
fn trampoline_with_a_pid_is_running() {
    assert_eq!(classify(&print_body("trampoline", Some(9001))).0, State::Running);
}

#[skuld::test]
fn xpcproxy_without_a_pid_is_unknown() {
    // Never observed in the 580-iteration sample, so `Running` would claim
    // more than was measured.
    assert_eq!(classify(&print_body("xpcproxy", None)).0, State::Unknown);
}

#[skuld::test]
fn not_running_is_stopped() {
    assert_eq!(classify(&print_body("not running", None)).0, State::Stopped);
}

#[skuld::test]
fn exited_is_stopped() {
    assert_eq!(classify(&print_body("exited", None)).0, State::Stopped);
}

#[skuld::test]
fn spawn_scheduled_is_unknown() {
    assert_eq!(classify(&print_body("spawn scheduled", None)).0, State::Unknown);
}

#[skuld::test]
fn an_unrecognised_state_is_unknown_not_stopped() {
    // A future macOS growing a new `state = ` value must only make goetia
    // vaguer, never wrong.
    assert_eq!(classify(&print_body("someday-new-apple-state", None)).0, State::Unknown);
}

#[skuld::test]
fn a_body_with_no_state_line_is_unknown() {
    let body = "system/com.example.daemon = {\n\tactive count = 1\n}\n";
    assert_eq!(classify(body).0, State::Unknown);
}

#[skuld::test]
fn the_pid_is_returned_alongside_every_state() {
    assert_eq!(classify(&print_body("not running", Some(4242))).1, Some(4242));
    assert_eq!(
        classify(&print_body("someday-new-apple-state", Some(4242))).1,
        Some(4242)
    );
}

// find_field ==========================================================================================================

#[skuld::test]
fn find_field_extracts_a_launchctl_print_style_line() {
    let text = "system/foo = {\n\tstate = running\n\tpid = 4242\n}\n";
    assert_eq!(find_field(text, "state"), Some("running"));
    assert_eq!(find_field(text, "pid"), Some("4242"));
}

#[skuld::test]
fn find_field_returns_none_for_a_missing_key() {
    let text = "system/foo = {\n\tstate = running\n}\n";
    assert_eq!(find_field(text, "pid"), None);
}

// kickstart_pid =======================================================================================================

/// The measured shape (L3): `kickstart -p` writes the bare decimal pid and a
/// newline, and nothing else.
#[skuld::test]
fn kickstart_pid_reads_the_bare_decimal_form() {
    assert_eq!(kickstart_pid("4766\n"), Some(4766));
}

#[skuld::test]
fn kickstart_pid_rejects_empty_and_non_numeric_output() {
    assert_eq!(kickstart_pid(""), None);
    assert_eq!(kickstart_pid("\n"), None);
    assert_eq!(kickstart_pid("not a pid\n"), None);
    assert_eq!(kickstart_pid("4766 spawned\n"), None);
}

/// The rule the whole guard exists for. A prefix of a pid parses as a
/// *different* pid, and quite possibly a live one: `"4766\n"` cut mid-write
/// to `"47"` reads as `47`, and nothing about that number looks wrong. So the
/// whole `<digits>\n` line has to be present, or there is no confirmation —
/// otherwise the clock picks which process `start` claims it launched.
#[skuld::test]
fn kickstart_pid_rejects_a_truncated_line() {
    assert_eq!(kickstart_pid("47"), None);
    assert_eq!(kickstart_pid("4766"), None);
}
