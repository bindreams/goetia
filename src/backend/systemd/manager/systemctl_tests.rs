//! Unit coverage for the pure halves of `systemctl.rs` — the diagnostic
//! [`failed`] builds out of a [`Capture`], across the shapes a capture can
//! arrive in, and the argv and environment each budget runs under — plus
//! [`took`] against a real `systemctl` run offline.
//!
//! These are pure-function tests on purpose. `failed`'s whole job is the
//! wording of a message, and `tests/systemd_integration/linux.rs` can only
//! reach it through a `systemctl` that actually fails — which is how the
//! stdout fallback and both truncation messages shipped with no test at all.

use super::*;
use std::time::Duration;

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
/// and the case that would otherwise report "wrote no diagnostic" when
/// systemd did write one.
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

// verb_args ===========================================================================================================

/// `--no-block` is the whole of what a non-waiting budget means to systemd, and no elevated test
/// can see it: `start` returns either way, so dropping the flag leaves that suite green (measured —
/// the review's mutation 2b kept 37/37). The argv is observable with no timing bet at all, which is
/// why the mechanism is pinned here rather than through a live `systemctl`.
#[skuld::test]
fn a_budget_that_does_not_wait_asks_systemd_not_to_block() {
    for budget in [Budget::Immediate, Budget::Bounded(Duration::ZERO)] {
        assert_eq!(
            verb_args("start", "x.service", budget),
            ["start", "--no-block", "--show-transaction", "x.service"],
            "{budget:?}"
        );
    }
}

/// A budget that waits must not pass `--no-block`: `systemctl` blocking until its job completes is
/// the confirmation the budget exists to wait for.
#[skuld::test]
fn a_budget_that_waits_lets_systemctl_block() {
    assert_eq!(
        verb_args("stop", "x.service", Budget::Unbounded),
        ["stop", "--show-transaction", "x.service"]
    );
}

/// Every budget asks `systemctl` to say when systemd has the job: the line [`enqueued`] waits for
/// before the deadline may cut anything short, and the only evidence that systemd took the request.
#[skuld::test]
fn every_budget_asks_systemctl_to_announce_the_job() {
    for budget in [
        Budget::Immediate,
        Budget::Unbounded,
        Budget::Bounded(Duration::from_secs(10)),
    ] {
        assert!(
            verb_args("stop", "x.service", budget).contains(&"--show-transaction"),
            "{budget:?}"
        );
    }
}

// enqueued ============================================================================================================

/// The line `systemctl --show-transaction` writes once systemd has answered the request with a job
/// — with and without the prefixes `SYSTEMD_LOG_TIME`/`SYSTEMD_LOG_LOCATION` add, which the
/// environment goetia sets does not switch off.
#[skuld::test]
fn enqueued_recognises_the_anchor_job_line() {
    assert!(enqueued(b"Enqueued anchor job 16157 x.service/start.\n"));
    assert!(enqueued(
        b"Fri 2026-09-18 11:16:37 UTC (165435) src/systemctl/systemctl-start-unit.c:114: \
          Enqueued anchor job 16565 x.service/stop.\n"
    ));
}

#[skuld::test]
fn enqueued_is_false_for_anything_else() {
    assert!(!enqueued(b""));
    assert!(!enqueued(
        b"Failed to start x.service: Unit x.service has a bad unit file setting.\n"
    ));
    assert!(!enqueued(b"Enqueued auxiliary job 12 y.service/start.\n"));
}

// run_verb ============================================================================================================

/// A stand-in for `systemctl --show-transaction`: `touch`es `$0` — the request reaching systemd —
/// then announces it exactly as `systemctl` does, and never exits on its own.
const ANNOUNCES_THEN_BLOCKS: &str =
    "touch \"$0\"; echo 'Enqueued anchor job 1 x.service/start.' >&2; exec sleep 2147483647";

/// The bounded path waits on the `deadline` it is handed — derived at verb entry, so it already
/// covers `require_installed`'s scan and the spawn — never on one re-derived from `budget`.
///
/// The child cannot exit on its own and the deadline is spent, so `Expired` is the only answer and
/// no timing is bet on. `Bounded(Duration::MAX)` is what makes a re-derivation fail: it overflows
/// `Instant` into an unbounded deadline, so a `run_verb` that ignored `deadline` would wait on the
/// child forever, which the suite's watchdog surfaces.
#[skuld::test]
fn the_bounded_path_waits_on_the_deadline_it_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    let finished = run_verb_via(
        "/bin/sh",
        &["-c", ANNOUNCES_THEN_BLOCKS, request.to_str().unwrap()],
        Budget::Bounded(Duration::MAX),
        Budget::Immediate.start(),
    )
    .expect("sh is spawnable");
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
}

/// The budget never decides whether the request is sent: with the deadline already spent when
/// `systemctl` is spawned, the request still reaches systemd before the deadline cuts anything
/// short. Deterministic — the stand-in announces only after its request is out.
#[skuld::test]
fn the_bounded_path_issues_the_request_on_a_spent_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    run_verb_via(
        "/bin/sh",
        &["-c", ANNOUNCES_THEN_BLOCKS, request.to_str().unwrap()],
        Budget::Bounded(Duration::from_secs(10)),
        Budget::Immediate.start(),
    )
    .expect("sh is spawnable");
    assert!(
        request.exists(),
        "the request must be issued whatever is left of the budget"
    );
}

/// An inherited `SYSTEMD_LOG_LEVEL=warning` or `SYSTEMD_LOG_TARGET=null` silences the announcement,
/// so every path sets both. The stand-ins exit `9` unless they see exactly those values.
const UNLESS_AUDIBLE_EXIT_9: &str =
    "[ \"$SYSTEMD_LOG_LEVEL\" = info ] && [ \"$SYSTEMD_LOG_TARGET\" = console ] || exit 9";

#[skuld::test]
fn the_paths_that_do_not_bound_keep_the_announcement_audible() {
    for budget in [Budget::Immediate, Budget::Unbounded] {
        let finished = run_verb_via(
            "/bin/sh",
            &["-c", &format!("{UNLESS_AUDIBLE_EXIT_9}; exit 0")],
            budget,
            budget.start(),
        )
        .expect("sh is spawnable");
        assert!(
            matches!(&finished, Finished::Exited { status, .. } if status.success()),
            "{budget:?}: {finished:?}"
        );
    }
}

#[skuld::test]
fn the_bounded_path_keeps_the_announcement_audible() {
    let finished = run_verb_via(
        "/bin/sh",
        &[
            "-c",
            &format!(
                "{UNLESS_AUDIBLE_EXIT_9}; echo 'Enqueued anchor job 1 x.service/start.' >&2; exec sleep 2147483647"
            ),
        ],
        Budget::Bounded(Duration::from_secs(10)),
        Budget::Immediate.start(),
    )
    .expect("sh is spawnable");
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
}

// diagnostic ==========================================================================================================

/// The transaction `--show-transaction` announces, and a job's `finished` notice, are written on
/// success as much as on failure: protocol, not diagnosis, so they never open a failure message.
#[skuld::test]
fn a_failure_is_reported_without_the_transaction_lines() {
    let e = failed(
        "start",
        "x.service",
        &capture(
            "",
            "Enqueued anchor job 7 x.service/start.\n\
             Enqueued auxiliary job 8 y.service/start.\n\
             Job for y.service finished.\n\
             Job for x.service failed because the control process exited with error code.\n",
            true,
        ),
    );
    assert_eq!(
        e.to_string(),
        "systemctl start x.service failed: Job for x.service failed because the control process \
         exited with error code.\n"
    );
}

/// A diagnostic the deadline cut short after the announcement is no diagnostic at all, and says so
/// rather than presenting the transaction as what went wrong.
#[skuld::test]
fn a_truncated_capture_of_only_the_transaction_says_nothing_was_captured() {
    let msg = failed(
        "start",
        "x.service",
        &capture("", "Enqueued anchor job 7 x.service/start.\n", false),
    )
    .to_string();
    assert!(!msg.contains("Enqueued"), "{msg}");
    assert!(msg.contains("no diagnostic was captured"), "{msg}");
}

// took ================================================================================================================

#[skuld::test]
fn an_exit_that_enqueued_a_job_took_the_request() {
    let taken = took(
        "start",
        "x.service",
        &capture("", "Enqueued anchor job 7 x.service/start.\n", true),
    );
    assert!(taken.is_ok(), "{taken:?}");
}

/// `systemctl` in a chroot or offline exits `0` and enqueues nothing. The real one, run offline, on
/// every path and for both verbs: an exit of `0` alone must never read as started or stopped.
#[skuld::test]
fn an_offline_systemctl_that_exits_0_took_nothing() {
    for verb in ["start", "stop"] {
        for budget in [
            Budget::Immediate,
            Budget::Unbounded,
            Budget::Bounded(Duration::from_secs(600)),
        ] {
            let mut args = vec!["SYSTEMD_OFFLINE=1", "systemctl"];
            args.extend(verb_args(verb, "goetia-offline-probe.service", budget));
            let finished = run_verb_via("env", &args, budget, budget.start()).expect("env is spawnable");
            let Finished::Exited { status, capture } = finished else {
                panic!("{verb} {budget:?}: an offline systemctl exits on its own: {finished:?}");
            };
            assert!(status.success(), "{verb} {budget:?}: {status:?} {capture:?}");
            let e = took(verb, "goetia-offline-probe.service", &capture)
                .expect_err("an exit of 0 with no job enqueued took nothing");
            let msg = e.to_string();
            assert!(msg.contains("without enqueuing a job"), "{verb} {budget:?}: {msg}");
            assert!(msg.contains("ignoring command"), "systemd's own reason: {msg}");
        }
    }
}
