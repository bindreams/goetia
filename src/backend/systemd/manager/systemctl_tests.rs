//! Unit coverage for the pure halves of `systemctl.rs` — the diagnostic
//! [`failed`] builds out of a [`Capture`], across the shapes a capture can
//! arrive in, and the argv and environment each budget runs under — plus the
//! manager boundary and the version gate, driven through a stand-in
//! `systemctl` ([`stand_in`]) and a real one run offline.
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
fn a_truncated_diagnostic_says_it_may_be_only_a_prefix() {
    let e = failed("start", "x.service", &capture("", "Unit x.ser", false));
    let msg = e.to_string();
    assert!(msg.contains("Unit x.ser"), "{msg}");
    assert!(msg.contains("may be only a prefix"), "{msg}");
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
/// — with and without the prefixes `SYSTEMD_LOG_TIME`/`SYSTEMD_LOG_LOCATION` add, which [`DENIED`]
/// keeps off every child: tolerance goetia does not depend on, pinned anyway.
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
        &["/bin/sh"],
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
        &["/bin/sh"],
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

/// A watched `systemctl` whose stdout or stderr no thread could read is never run: `Error::Other`, which every verb exits `1` for, and no request — never a panic, and
/// never a request out with nothing to watch it.
#[skuld::test]
fn a_request_nothing_could_watch_is_never_sent() {
    type Allowance = fn() -> bounded::test_hook::Allowance;
    let shortfalls: [(Allowance, &str); 2] = [
        (|| bounded::test_hook::threads(0), "no thread"),
        (|| bounded::test_hook::threads(1), "no thread"),
    ];
    for (allowance, what) in shortfalls {
        let dir = tempfile::tempdir().unwrap();
        let request = dir.path().join("request");
        let _short = allowance();
        let spawns = bounded::test_hook::spawns();

        let e = run_verb_via(
            &["/bin/sh"],
            &["-c", ANNOUNCES_THEN_BLOCKS, request.to_str().unwrap()],
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Immediate.start(),
        )
        .expect_err(what);

        assert!(matches!(e, Error::Other(_)), "{what}: {e:?}");
        assert!(e.to_string().contains(what), "{e}");
        assert!(e.to_string().contains("so it was not run"), "{e}");
        assert_eq!(bounded::test_hook::spawns(), spawns, "{what}: systemctl was spawned");
        assert!(!request.exists(), "{what}: the request was sent");
    }
}

/// A watched `systemctl` whose spawn failed after it may have run is a request in doubt — exit `4`,
/// saying it may or may not have reached systemd — and one that failed before it ran is a plain
/// failure, which says it was not. The spawn failures are injected, so nothing runs for them; the
/// failed wait is injected once `/bin/true` has run, and it announced nothing.
#[skuld::test]
fn a_watched_request_that_may_have_run_is_in_doubt() {
    let spawn = || {
        run_verb_via(
            &["/bin/true"],
            &[],
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Unbounded.start(),
        )
    };
    let may_or_may_not = |e: &Error| {
        assert!(matches!(e, Error::RequestInDoubt { reached: false, .. }), "{e:?}");
        let msg = e.to_string();
        assert!(msg.contains("may have started before it failed"), "{msg}");
        assert!(msg.contains("may or may not have reached the service manager"), "{msg}");
    };
    bounded::test_hook::spawn_fails(|| cosca::error::Error::Containment {
        detail: "forced".into(),
    });
    may_or_may_not(&spawn().expect_err("injected"));

    bounded::test_hook::spawn_fails(|| std::io::Error::from_raw_os_error(libc::EAGAIN).into());
    let e = spawn().expect_err("injected");
    assert!(matches!(e, Error::Other(_)), "{e:?}");

    // Past the spawn `systemctl` runs, so a wait that fails leaves its request in doubt too.
    bounded::test_hook::wait_fails(|| std::io::Error::other("the wait failed").into());
    may_or_may_not(&spawn().expect_err("injected"));
}

/// A request lost after `systemctl` announced its job had reached systemd, and says so: its outcome
/// is what is unconfirmed, never whether it arrived.
#[skuld::test]
fn a_watched_request_lost_after_its_announcement_reached_the_manager() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    bounded::test_hook::wait_fails(|| std::io::Error::other("the wait failed").into());
    let e = run_verb_via(
        &["/bin/sh"],
        &["-c", ANNOUNCES_THEN_BLOCKS, request.to_str().unwrap()],
        Budget::Bounded(Duration::from_secs(10)),
        Budget::Unbounded.start(),
    )
    .expect_err("injected");
    assert!(matches!(e, Error::RequestInDoubt { reached: true, .. }), "{e:?}");
    let msg = e.to_string();
    assert!(
        msg.contains("reached the service manager, but goetia lost track of it before its outcome was confirmed"),
        "{msg}"
    );
    assert!(!msg.contains("may or may not"), "{msg}");
}

/// The paths std runs to completion keep the same two apart: a `spawn` that failed never ran the
/// request, and a wait that failed after it did leaves it in doubt.
#[skuld::test]
fn a_request_run_to_completion_is_in_doubt_only_once_it_ran() {
    let e = requested(
        &mut Command::new("/nonexistent/systemctl"),
        "systemctl",
        &["enable", "x"],
    )
    .expect_err("no such program");
    assert!(matches!(e, Error::Other(_)), "{e:?}");
    assert!(e.to_string().contains("so it was not sent"), "{e}");

    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let e = requested_with(
        Command::new("/bin/sh").args(["-c", "touch \"$0\"", ran.to_str().unwrap()]),
        "systemctl",
        &["enable", "x"],
        |child| {
            child.wait_with_output()?;
            Err(std::io::Error::other("the wait failed"))
        },
    )
    .expect_err("injected");
    assert!(ran.exists(), "the request ran");
    assert!(matches!(e, Error::RequestInDoubt { .. }), "{e:?}");
}

/// A `daemon-reload` in doubt after `install` wrote the unit stays in doubt — exit `4` — and says
/// the unit was written; any other failure there is exit `1`, and says so too.
#[skuld::test]
fn a_reload_in_doubt_after_the_write_stays_in_doubt() {
    let _stand_in = stand_in::set(Some(&gate_passes_then("exit 0")), booted());
    bounded::test_hook::wait_fails(|| std::io::Error::other("the wait failed").into());
    let e = daemon_reload_or_report("x").expect_err("injected");
    assert!(
        matches!(&e, Error::RequestInDoubt { request, reached: false, .. } if request == "systemctl daemon-reload"),
        "{e:?}"
    );
    let msg = e.to_string();
    assert!(msg.contains("wrote the unit for `x`"), "{msg}");
    assert!(msg.contains("the wait failed"), "{msg}");

    drop(_stand_in);
    let _stand_in = stand_in::set(Some(&gate_passes_then("echo 'Access denied' >&2; exit 1")), booted());
    let e = daemon_reload_or_report("x").expect_err("refused");
    assert!(matches!(e, Error::Other(_)), "{e:?}");
    let msg = e.to_string();
    assert!(msg.contains("wrote the unit for `x`"), "{msg}");
    assert!(msg.contains("Access denied"), "{msg}");
}

/// A watched `systemctl` needs no temp file: it runs where none may be writable.
#[skuld::test]
fn a_watched_request_needs_no_temp_file() {
    let _no_file = bounded::test_hook::temp_files(0);
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    let finished = run_verb_via(
        &["/bin/sh"],
        &["-c", ANNOUNCES_THEN_BLOCKS, request.to_str().unwrap()],
        Budget::Bounded(Duration::from_secs(10)),
        Budget::Immediate.start(),
    )
    .expect("no temp file is needed");
    assert!(matches!(finished, Finished::Expired), "{finished:?}");
    assert!(request.exists());
}

/// An inherited `SYSTEMD_LOG_LEVEL=warning` or `SYSTEMD_LOG_TARGET=null` silences the announcement,
/// so every path sets both. The stand-ins exit `9` unless they see exactly those values.
const UNLESS_AUDIBLE_EXIT_9: &str =
    "[ \"$SYSTEMD_LOG_LEVEL\" = info ] && [ \"$SYSTEMD_LOG_TARGET\" = console ] || exit 9";

#[skuld::test]
fn the_paths_that_do_not_bound_keep_the_announcement_audible() {
    for budget in [Budget::Immediate, Budget::Unbounded] {
        let finished = run_verb_via(
            &["/bin/sh"],
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
        &["/bin/sh"],
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

/// Stderr that carried nothing but protocol said nothing, so stdout is what gets reported: the
/// fallback asks what is left of stderr once the protocol is gone, not whether it had any bytes.
#[skuld::test]
fn a_failure_whose_stderr_is_only_protocol_is_reported_with_its_stdout() {
    let e = failed(
        "start",
        "x.service",
        &capture(
            "something on stdout\n",
            "Enqueued anchor job 7 x.service/start.\n",
            true,
        ),
    );
    assert_eq!(e.to_string(), "systemctl start x.service failed: something on stdout\n");
}

/// `SYSTEMD_LOG_TIME`, `SYSTEMD_LOG_LOCATION` and `SYSTEMD_LOG_TID` each prefix every protocol
/// line. No `systemctl` goetia runs can be given one — [`DENIED`] removes them with every other
/// `SYSTEMD_*` — so this pins tolerance goetia does not depend on, which is why [`enqueued`] and
/// [`is_protocol`] read a substring rather than a whole line. The lines below are what `systemctl
/// start --show-transaction` wrote under each on systemd 257.
#[skuld::test]
fn a_prefixed_transaction_is_still_protocol() {
    for prefix in [
        "Fri 2026-09-18 21:16:53 UTC ",
        "src/systemctl/systemctl-start-unit.c:114: ",
        "(1053127) ",
    ] {
        let finished = if prefix.starts_with("src/") {
            "src/shared/bus-wait-for-jobs.c:225: "
        } else {
            prefix
        };
        let stderr = format!(
            "{prefix}Enqueued anchor job 7 x.service/start.\n\
             {prefix}Enqueued auxiliary job 8 y.service/start.\n\
             {finished}Job for y.service finished.\n\
             {prefix}Job for x.service failed because the control process exited with error code.\n"
        );
        let e = failed("start", "x.service", &capture("", &stderr, true));
        assert_eq!(
            e.to_string(),
            format!(
                "systemctl start x.service failed: {prefix}Job for x.service failed because the control process \
                 exited with error code.\n"
            ),
            "{prefix:?}"
        );
    }
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

// The manager boundary ================================================================================================

/// A system booted with systemd, and not offline: its manager must be asked.
fn booted() -> Host {
    Host {
        offline: None,
        unbooted: false,
        chroot: None,
    }
}

/// A stand-in `systemctl` that passes the version gate, and answers anything else with `rest`.
fn gate_passes_then(rest: &str) -> String {
    format!(
        "case \"$*\" in \
           'show --property=Version --value') echo 257;; \
           --version) echo 'systemd 257 (257)';; \
           *) {rest};; \
         esac"
    )
}

/// One way in to the manager, named.
type Path = (String, Box<dyn Fn() -> Result<()>>);

/// Every path goetia reaches the manager by, each through the function the verbs call: the three
/// requests run to completion, the `show` a state read runs, and `start`, `stop` and `restart` under
/// every budget, bounded or not.
fn every_path() -> Vec<Path> {
    let mut paths: Vec<Path> = vec![
        ("daemon-reload".to_string(), Box::new(daemon_reload)),
        (
            "enable".to_string(),
            Box::new(|| run_systemctl(&["enable", "x.service"]).map(drop)),
        ),
        (
            "disable".to_string(),
            Box::new(|| run_systemctl(&["disable", "x.service"]).map(drop)),
        ),
        ("show".to_string(), Box::new(|| status_from_unit("x.service").map(drop))),
        ("restart".to_string(), Box::new(|| request_restart_impl("x"))),
    ];
    for budget in [
        Budget::Immediate,
        Budget::Unbounded,
        Budget::Bounded(Duration::from_secs(600)),
    ] {
        paths.push((
            format!("start {budget:?}"),
            Box::new(move || start_impl("x", budget, budget.start())),
        ));
        paths.push((
            format!("stop {budget:?}"),
            Box::new(move || stop_impl("x", budget, budget.start())),
        ));
    }
    paths
}

/// The refusal, naming `evidence`.
fn assert_no_manager(result: Result<()>, evidence: &str, path: &str) {
    let e = result.expect_err(path);
    assert!(matches!(e, Error::NoManager { .. }), "{path}: {e:?}");
    let msg = e.to_string();
    assert!(
        msg.starts_with("goetia does not manage systemd here ("),
        "{path}: {msg}"
    );
    assert!(msg.contains(evidence), "{path}: {msg}");
}

/// Evidence known without asking — `SYSTEMD_OFFLINE` set true, no `/run/systemd/system`, or a `/`
/// that is not PID 1's root — is refused on every path before any `systemctl` runs, the version
/// gate's own probes included: the stand-in records every run, and none happens.
#[skuld::test]
fn evidence_known_without_asking_refuses_every_path_before_anything_runs() {
    let hosts = [
        (
            Host {
                offline: Some("1".to_string()),
                unbooted: false,
                chroot: None,
            },
            "`SYSTEMD_OFFLINE=1` is set",
        ),
        (
            Host {
                offline: None,
                unbooted: true,
                chroot: None,
            },
            "`/run/systemd/system`, which systemd makes when it boots a system, does not exist here",
        ),
        (
            Host {
                offline: None,
                unbooted: false,
                chroot: Some(Chroot::Root),
            },
            "`/` is not PID 1's root (`/proc/1/root`), so goetia runs under another root than PID 1, as in a \
             chroot or a container sharing the host's PID namespace",
        ),
        (
            Host {
                offline: None,
                unbooted: false,
                chroot: Some(Chroot::Mount),
            },
            "the mount at `/` is none of PID 1's (`/proc/self/mountinfo`, `/proc/1/mountinfo`), so goetia runs \
             under another root than PID 1",
        ),
        (
            Host {
                offline: None,
                unbooted: false,
                chroot: Some(Chroot::NoMount),
            },
            "no mount is at `/` (`/proc/self/mountinfo`): `/` is a directory inside one, so goetia runs in a \
             chroot",
        ),
    ];
    for (host, evidence) in hosts {
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let script = format!("echo \"$*\" >> '{}'; exit 0", ran.display());
        let _stand_in = stand_in::set(Some(&script), host);
        assert_no_manager(require_supported(), evidence, "the gate");
        for (path, run) in every_path() {
            assert_no_manager(run(), evidence, &path);
        }
        assert!(
            !ran.exists(),
            "{evidence}: systemctl ran: {:?}",
            std::fs::read_to_string(&ran)
        );
    }
}

/// A chroot only `systemctl` can report, and reports only once asked. No request is sent until
/// the manager has been asked: here every request would act silently and exit `0`, as `enable` and
/// `disable` do in a chroot, and none runs. The read is refused by its own answer.
#[skuld::test]
fn no_request_is_sent_before_the_manager_is_asked() {
    let dir = tempfile::tempdir().unwrap();
    let acted = dir.path().join("acted");
    let script = format!(
        "case \"$1\" in \
           show) echo \"Running in chroot, ignoring command 'show'\" >&2;; \
           --version) echo 'systemd 257 (257)';; \
           *) echo \"$*\" >> '{}';; \
         esac",
        acted.display()
    );
    let _stand_in = stand_in::set(Some(&script), booted());
    for (path, run) in every_path() {
        assert_no_manager(run(), "Running in chroot, ignoring command 'show'", &path);
    }
    assert!(
        !acted.exists(),
        "a request was sent before the manager was asked: {:?}",
        std::fs::read_to_string(&acted)
    );
}

/// The backstop: a `systemctl` past the gate that says it ignored what it was asked is refused on
/// every path, and never read as a success — in every wording [`IGNORED`] lists, on either
/// stream. The stand-in says so only when its log level is audible, as a real one under an
/// inherited `SYSTEMD_LOG_LEVEL=warning` would not, so every path must also make it audible.
#[skuld::test]
fn a_systemctl_that_ignored_the_request_is_refused_on_every_path() {
    for (report, stream) in [
        ("Running in chroot, ignoring command '$1'", ">&2"),
        ("Running in chroot, ignoring request: $1", ">&2"),
        ("Running in chroot, ignoring request.", ">&2"),
        ("Running in chroot, ignoring request.", ""),
    ] {
        let script = gate_passes_then(&format!(
            "[ \"$SYSTEMD_LOG_LEVEL\" = info ] && [ \"$SYSTEMD_LOG_TARGET\" = console ] && echo \"{report}\" {stream}; \
             exit 0"
        ));
        let _stand_in = stand_in::set(Some(&script), booted());
        for (path, run) in every_path() {
            let verb = path.split(' ').next().unwrap();
            let said = report.replace("$1", verb);
            assert_no_manager(run(), &said, &format!("{path} ({report:?} {stream:?})"));
        }
    }
}

/// The real `systemctl`, offline, on every path `start` and `stop` take: an exit of `0` that asked
/// nobody is the refusal, never read as started or stopped. It says so in 246's words and newer
/// ones', or 242 to 245's.
#[skuld::test]
fn a_real_offline_systemctl_is_refused_on_every_path() {
    for verb in ["start", "stop", "restart"] {
        for budget in [
            Budget::Immediate,
            Budget::Unbounded,
            Budget::Bounded(Duration::from_secs(600)),
        ] {
            let result = run_verb_via(
                &["env", "SYSTEMD_OFFLINE=1", "systemctl"],
                &verb_args(verb, "goetia-offline-probe.service", budget),
                budget,
                budget.start(),
            );
            let e = result.expect_err(verb);
            assert!(matches!(e, Error::NoManager { .. }), "{verb} {budget:?}: {e:?}");
            let msg = e.to_string();
            assert!(
                [
                    format!("Running in chroot, ignoring command '{verb}'"),
                    format!("Running in chroot, ignoring request: {verb}"),
                ]
                .iter()
                .any(|said| msg.contains(&format!("`systemctl` said \"{said}\""))),
                "{verb} {budget:?}: {msg}"
            );
        }
    }
}

/// An answer `systemctl` says nothing about ignoring is not refused.
#[skuld::test]
fn an_answer_that_ignored_nothing_is_an_answer() {
    assert!(answered(b"ActiveState=active\n", b"").is_ok());
    assert!(answered(b"", b"Enqueued anchor job 7 x.service/start.\n").is_ok());
    assert!(
        answered(b"", b"x.service:3: Unknown key 'Foo' in section [Service], ignoring.\n").is_ok(),
        "a unit file warning is not a report that the request was ignored"
    );
}

// status_from_unit ====================================================================================================

/// An answer without every property asked for is no answer: never the state, pid or enablement its
/// absence would default to. Empty — as a `systemctl` that asked nobody and did not say so answers
/// — or partial.
#[skuld::test]
fn a_show_answer_without_every_property_is_not_a_state() {
    for (answer, missing) in [
        ("true", "ActiveState, MainPID, UnitFileState, LoadState"),
        ("printf 'ActiveState=active\\n'", "MainPID, UnitFileState, LoadState"),
        (
            "printf 'ActiveState=active\\nMainPID=42\\nUnitFileState=enabled\\n'",
            "LoadState",
        ),
    ] {
        let _stand_in = stand_in::set(Some(&format!("{answer}; exit 0")), booted());
        let e = status_from_unit("x.service").expect_err(answer);
        let msg = e.to_string();
        assert!(msg.contains(&format!("answered without {missing}")), "{answer}: {msg}");
    }
}

/// A unit systemd has not loaded — `LoadState=not-found`, no unit file for the name — is never a
/// state, whatever else systemd answers for it: goetia asks only about units it finds installed, so
/// that is systemd not having loaded one, and "stopped, not enabled" would be made up. Every other
/// load state is systemd's own answer about a unit it has.
#[skuld::test]
fn a_unit_systemd_has_not_loaded_is_not_a_state() {
    for (active, unit_file) in [("inactive", ""), ("active", "enabled")] {
        let answer = format!("ActiveState={active}\\nMainPID=0\\nUnitFileState={unit_file}\\nLoadState=not-found\\n");
        let _stand_in = stand_in::set(Some(&format!("printf '{answer}'")), booted());
        let msg = status_from_unit("x.service").expect_err(&answer).to_string();
        assert!(
            msg.contains("systemd has not loaded the unit file goetia finds installed for `x.service`"),
            "{answer}: {msg}"
        );
        assert!(msg.contains("`LoadState=not-found`"), "{answer}: {msg}");
    }
    for load in ["loaded", "masked", "bad-setting", "error"] {
        let answer = format!("ActiveState=inactive\\nMainPID=0\\nUnitFileState=\\nLoadState={load}\\n");
        let _stand_in = stand_in::set(Some(&format!("printf '{answer}'")), booted());
        let status = status_from_unit("x.service").expect(&answer);
        assert_eq!(status.state, State::Stopped, "{answer}");
    }
}

/// Every property answered, an empty `UnitFileState` included — what systemd answers for a unit it
/// has not loaded.
#[skuld::test]
fn a_show_answer_with_every_property_is_read() {
    for (answer, expected) in [
        (
            "ActiveState=active\\nMainPID=42\\nUnitFileState=enabled\\nLoadState=loaded\\n",
            Status {
                state: State::Running,
                pid: Some(42),
                enabled: true,
            },
        ),
        (
            "MainPID=0\\nLoadState=loaded\\nActiveState=inactive\\nUnitFileState=\\n",
            Status {
                state: State::Stopped,
                pid: None,
                enabled: false,
            },
        ),
    ] {
        let _stand_in = stand_in::set(Some(&format!("printf '{answer}'")), booted());
        let status = status_from_unit("x.service").expect(answer);
        assert_eq!(
            (status.state, status.pid, status.enabled),
            (expected.state, expected.pid, expected.enabled),
            "{answer}"
        );
    }
}

// supported ===========================================================================================================

/// The refusal for the version one `source` reported, as [`require_supported`] words it.
fn verdicted(source: Source, reported: &str) -> Result<()> {
    refusal(verdict(source, reported).into_iter().collect())
}

/// The `Version` property as distributions set it, measured with `systemctl --version`, whose
/// parenthesised part is the same `GIT_VERSION` string (identical on this host's systemd 257).
#[skuld::test]
fn a_running_systemd_242_or_newer_is_supported() {
    for version in [
        "242",
        "257.13-1~deb13u1",   // Debian 13
        "252.39-1~deb12u2",   // Debian 12
        "255.4-1ubuntu8.17",  // Ubuntu 24.04
        "249.11-0ubuntu3.22", // Ubuntu 22.04
        "256.17-1.fc41",      // Fedora 41
        "252-78.el9",         // CentOS Stream 9
        "261.3-1-arch",       // Arch
        "261.2",              // openSUSE Tumbleweed
        "258~rc1",
        "v257-rc1-15-g1234567",
    ] {
        assert!(verdicted(Source::Manager, version).is_ok(), "{version:?}");
    }
}

#[skuld::test]
fn a_client_242_or_newer_is_supported() {
    for version in [
        "systemd 242 (242)",
        "systemd 257 (257.13-1~deb13u1)",
        "systemd 258~rc1 (258~rc1-1)",
    ] {
        assert!(verdicted(Source::Client, version).is_ok(), "{version:?}");
    }
}

/// systemd 240 and 241 parse `Type=exec`; only `--show-transaction` is missing, and the refusal
/// must say that and nothing more.
#[skuld::test]
fn systemd_240_and_241_are_refused_for_show_transaction_alone() {
    for (source, version, reported) in [
        (Source::Manager, "241.7-1", "241"),
        (Source::Manager, "240", "240"),
        (Source::Client, "systemd 241 (241)", "241"),
    ] {
        let msg = verdicted(source, version).expect_err(version).to_string();
        assert!(msg.contains("requires systemd 242 or newer"), "{msg}");
        assert!(msg.contains(&format!(" {reported}")), "{msg}");
        assert!(msg.contains("--show-transaction"), "{msg}");
        assert!(!msg.contains("Type=exec"), "{version} parses Type=exec: {msg}");
    }
}

/// Before 240 both reasons apply to the running systemd. The client parses no units, so
/// `--show-transaction` is its only reason.
#[skuld::test]
fn systemd_older_than_240_is_refused_for_both_reasons() {
    let msg = verdicted(Source::Manager, "239-41.el8").unwrap_err().to_string();
    assert!(msg.contains("requires systemd 242 or newer"), "{msg}");
    assert!(msg.contains("--show-transaction"), "{msg}");
    assert!(msg.contains("Type=simple"), "{msg}");
    let msg = verdicted(Source::Client, "systemd 237").unwrap_err().to_string();
    assert!(msg.contains("--show-transaction"), "{msg}");
    assert!(!msg.contains("Type=simple"), "{msg}");
}

/// The refusal names whose version it read.
#[skuld::test]
fn a_refusal_names_whose_version_it_read() {
    let manager = verdicted(Source::Manager, "241").unwrap_err().to_string();
    assert!(manager.contains("The running systemd is 241"), "{manager}");
    let client = verdicted(Source::Client, "systemd 241 (241)").unwrap_err().to_string();
    assert!(client.contains("The `systemctl` client is 241"), "{client}");
}

/// A version goetia cannot read is not a version it can vouch for — and not one it may call old.
/// Escapes included: a colour goetia failed to switch off is refused, never misread.
#[skuld::test]
fn an_unreadable_version_is_refused_without_calling_it_old() {
    for (source, version) in [
        (Source::Manager, ""),
        (Source::Manager, "abc"),
        (Source::Manager, "\u{1b}[0;1;39m257\u{1b}[0m"),
        (Source::Manager, "257\u{1b}[0m"),
        (Source::Client, ""),
        (Source::Client, "systemd"),
        (Source::Client, "systemd abc"),
        (Source::Client, "not systemd 257"),
        (Source::Client, "\u{1b}[0;1;39msystemd 257\u{1b}[0m (257.13-1~deb13u1)"),
    ] {
        let msg = verdicted(source, version).expect_err(version).to_string();
        assert!(msg.contains("cannot read the version"), "{version:?}: {msg}");
        for claim in ["older", "before", "Type=simple", "cannot answer", "does not accept"] {
            assert!(!msg.contains(claim), "{version:?} is not known to be old: {msg}");
        }
    }
}

// gated ===============================================================================================================

/// A check that counts how often it is asked, and passes.
fn counted(asked: &std::cell::Cell<usize>) -> impl FnOnce() -> Result<()> + '_ {
    move || {
        asked.set(asked.get() + 1);
        Ok(())
    }
}

/// Inside a scope the gate is asked once; outside one, every time — nothing is remembered past the
/// invocation that opened it.
#[skuld::test]
fn the_gate_is_asked_once_inside_a_scope_and_every_time_outside_one() {
    let asked = std::cell::Cell::new(0);
    gated(counted(&asked)).unwrap();
    gated(counted(&asked)).unwrap();
    assert_eq!(asked.get(), 2, "outside a scope, every verb asks");

    let scope = gate_scope();
    gated(counted(&asked)).unwrap();
    gated(counted(&asked)).unwrap();
    assert_eq!(asked.get(), 3, "inside one, once");

    drop(scope);
    gated(counted(&asked)).unwrap();
    assert_eq!(asked.get(), 4, "the answer ends with the scope");
}

/// Scopes nest and end in any order, and share one answer until the last of them ends.
#[skuld::test]
fn nested_scopes_share_one_answer_until_the_last_ends() {
    for outer_first in [false, true] {
        let asked = std::cell::Cell::new(0);
        let outer = gate_scope();
        gated(counted(&asked)).unwrap();
        let inner = gate_scope();
        gated(counted(&asked)).unwrap();
        let (first, last) = if outer_first { (outer, inner) } else { (inner, outer) };
        drop(first);
        gated(counted(&asked)).unwrap();
        assert_eq!(asked.get(), 1, "outer first: {outer_first}");
        drop(last);
        gated(counted(&asked)).unwrap();
        assert_eq!(asked.get(), 2, "outer first: {outer_first}");
    }
}

/// A refusal is not remembered: the next step asks again.
#[skuld::test]
fn a_refused_gate_is_asked_again() {
    let _scope = gate_scope();
    assert!(gated(|| Err(Error::Other("refused".to_string()))).is_err());
    let asked = std::cell::Cell::new(0);
    gated(counted(&asked)).unwrap();
    assert_eq!(asked.get(), 1);
}

/// A remembered pass never stands in for the evidence [`door`] reads without asking: that is read
/// on every run.
#[skuld::test]
fn a_remembered_pass_does_not_skip_the_evidence() {
    let _gate = gate_scope();
    let _online = stand_in::set(Some(&gate_passes_then("exit 0")), booted());
    require_supported().expect("the stand-in passes the gate");
    drop(_online);
    let _offline = stand_in::set(
        Some(&gate_passes_then("exit 0")),
        Host {
            offline: Some("yes".to_string()),
            unbooted: false,
            chroot: None,
        },
    );
    assert_no_manager(daemon_reload(), "`SYSTEMD_OFFLINE=yes` is set", "daemon-reload");
}

// supported, asked ====================================================================================================

/// Answers `show` with `$MANAGER`'s version and `--version` with `$CLIENT`'s, each `none` to fail.
fn two_versions(manager: &str, client: &str) -> String {
    format!(
        "case \"$*\" in \
           'show --property=Version --value') [ {manager} = none ] && exit 1; echo {manager};; \
           --version) [ {client} = none ] && exit 99; echo 'systemd {client} ({client})';; \
           *) exit 98;; \
         esac"
    )
}

/// [`supported`], with `script` standing in for `systemctl` on a booted host.
fn supported_by(script: &str) -> Result<()> {
    let _stand_in = stand_in::set(Some(script), booted());
    supported()
}

fn refused(manager: &str, client: &str) -> String {
    supported_by(&two_versions(manager, client))
        .expect_err(&format!("manager {manager}, client {client}"))
        .to_string()
}

/// Both must be 242 or newer: the running systemd parses `Type=exec` and answers the job, and the
/// client parses `--show-transaction`, so neither rescues the other. The refusal names whichever
/// failed, with its version, and both when both did.
#[skuld::test]
fn both_the_running_systemd_and_the_client_must_be_242_or_newer() {
    assert!(supported_by(&two_versions("257", "257")).is_ok());

    let old_client = refused("257", "241");
    assert!(old_client.contains("The `systemctl` client is 241"), "{old_client}");
    assert!(!old_client.contains("running systemd is"), "{old_client}");

    let old_manager = refused("241", "257");
    assert!(old_manager.contains("The running systemd is 241"), "{old_manager}");
    assert!(!old_manager.contains("client is"), "{old_manager}");

    let both = refused("239", "241");
    assert!(both.contains("The running systemd is 239"), "{both}");
    assert!(both.contains("The `systemctl` client is 241"), "{both}");
}

/// A client that cannot say its version cannot be vouched for, whatever the manager says.
#[skuld::test]
fn a_client_that_cannot_say_its_version_is_refused() {
    let msg = refused("257", "none");
    assert!(msg.contains("`systemctl --version` failed"), "{msg}");
}

/// `/run/systemd/system` counts as absent only on evidence: not there, not a directory, or under
/// something that is not one. A stat that failed any other way — here a symlink loop — establishes
/// nothing.
#[skuld::test]
fn only_a_stat_that_finds_no_directory_is_evidence_of_no_systemd() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "").unwrap();
    let looped = dir.path().join("loop");
    std::os::unix::fs::symlink(&looped, &looped).unwrap();

    assert!(established_absent(&dir.path().join("missing")));
    assert!(established_absent(&file));
    assert!(established_absent(&file.join("under")));
    assert!(!established_absent(dir.path()));
    assert!(
        !established_absent(&looped),
        "a stat that failed otherwise establishes nothing"
    );
}

/// `/` and PID 1's root, both read, decide, either way — the check `running_in_chroot()` makes, so
/// two equal roots are no chroot whatever the mount tables say. A stat that failed establishes
/// nothing — `EACCES`, as `/proc/1/root` answers a caller that may not trace PID 1, or `ENOENT`,
/// with no `/proc` — and leaves it to the mount tables.
#[skuld::test]
fn two_roots_read_decide_and_the_mount_tables_only_where_they_could_not_be() {
    let failed = |errno| Err(std::io::Error::from_raw_os_error(errno));
    let differ = || (table(&[("0:131", "/", "/")]), table(&[("8:1", "/", "/")]));
    let agree = || (table(&[("8:1", "/", "/")]), table(&[("8:1", "/", "/")]));
    assert_eq!(chroot_from(Ok((1, 2)), Ok((1, 3)), agree), Some(Chroot::Root));
    assert_eq!(chroot_from(Ok((1, 2)), Ok((4, 2)), agree), Some(Chroot::Root));
    assert_eq!(
        chroot_from(Ok((1, 2)), Ok((1, 2)), differ),
        None,
        "equal roots are final"
    );
    for errno in [libc::EACCES, libc::ENOENT] {
        assert_eq!(chroot_from(Ok((1, 2)), failed(errno), agree), None, "{errno}");
        assert_eq!(chroot_from(failed(errno), Ok((1, 2)), agree), None, "{errno}");
        assert_eq!(
            chroot_from(Ok((1, 2)), failed(errno), differ),
            Some(Chroot::Mount),
            "{errno}"
        );
        assert_eq!(
            chroot_from(failed(errno), Ok((1, 2)), differ),
            Some(Chroot::Mount),
            "{errno}"
        );
    }
}

/// A mount table: one line for each `(device, root, mount point)`, the other fields as a real one
/// has them.
fn table(mounts: &[(&str, &str, &str)]) -> std::io::Result<String> {
    Ok(mounts
        .iter()
        .enumerate()
        .map(|(n, (device, root, point))| {
            format!(
                "{} 1 {device} {root} {point} rw,relatime shared:{n} - ext4 /dev/sda1 rw\n",
                n + 22
            )
        })
        .collect())
}

/// goetia's `/` is not PID 1's, by the mount tables, only when goetia's is read and has no mount at
/// `/` — whatever PID 1's — or PID 1's is read too and has a mount at `/`, and none of goetia's
/// there is PID 1's: a device and a root within it both make a mount. Shapes measured on this host:
/// a tmpfs chroot (`0:131 /` against `8:1 /`) and a directory chroot, where no mount is at `/`.
#[skuld::test]
fn only_a_mount_table_without_pid_1s_mount_at_root_is_evidence_of_a_chroot() {
    let failed = |errno| Err(std::io::Error::from_raw_os_error(errno));
    let host = [("8:1", "/", "/"), ("0:25", "/", "/proc"), ("8:1", "/usr", "/usr")];
    let cases = [
        ("the host", table(&host), None),
        (
            "a tmpfs root",
            table(&[("0:131", "/", "/"), ("8:1", "/usr", "/usr")]),
            Some(Chroot::Mount),
        ),
        (
            "a directory root",
            table(&[("8:1", "/usr", "/usr"), ("0:5", "/", "/dev")]),
            Some(Chroot::NoMount),
        ),
        (
            "a bound subdirectory",
            table(&[("8:1", "/srv/root", "/")]),
            Some(Chroot::Mount),
        ),
        (
            "an escaped mount point",
            table(&[("0:131", "/", "/\\040")]),
            Some(Chroot::NoMount),
        ),
        (
            "a mount over PID 1's",
            table(&[("8:1", "/", "/"), ("0:40", "/", "/")]),
            None,
        ),
        ("unreadable", failed(libc::EACCES), None),
    ];
    for (shape, own, chroot) in cases {
        assert_eq!(mounts_differ(own, table(&host)), chroot, "{shape}");
    }

    let btrfs = [("0:30", "/@", "/")];
    assert_eq!(mounts_differ(table(&btrfs), table(&btrfs)), None, "the same subvolume");
    assert_eq!(
        mounts_differ(table(&[("0:30", "/@snap", "/")]), table(&btrfs)),
        Some(Chroot::Mount),
        "another subvolume"
    );
    for (init, why) in [
        (failed(libc::EACCES), "PID 1's unreadable"),
        (failed(libc::ENOENT), "no /proc"),
        (table(&[("8:1", "/usr", "/usr")]), "PID 1 with no mount at /"),
    ] {
        assert_eq!(mounts_differ(table(&[("0:131", "/", "/")]), init), None, "{why}");
    }
    // No mount at `/` of goetia's own needs nothing of PID 1's: only `chroot(2)` makes such a root.
    for (init, why) in [
        (failed(libc::EACCES), "PID 1's hidden by hidepid"),
        (failed(libc::ENOENT), "PID 1's gone"),
        (table(&[("8:1", "/usr", "/usr")]), "PID 1 with no mount at /"),
        (table(&host), "PID 1's read"),
    ] {
        assert_eq!(
            mounts_differ(table(&[("8:1", "/usr", "/usr")]), init),
            Some(Chroot::NoMount),
            "{why}"
        );
    }
    assert_eq!(
        mounts_differ(failed(libc::EACCES), table(&host)),
        None,
        "our own unreadable"
    );
}

/// A chroot is named before a missing `/run/systemd/system`, which a chroot with no `/run` bound in
/// lacks too: it is the more specific cause. `SYSTEMD_OFFLINE` is named before either.
#[skuld::test]
fn a_chroot_is_named_before_a_missing_run_systemd_system() {
    let unbooted_chroot = Host {
        offline: None,
        unbooted: true,
        chroot: Some(Chroot::NoMount),
    };
    assert_eq!(unbooted_chroot.evidence(), Some(Evidence::Chroot(Chroot::NoMount)));
    let everything = Host {
        offline: Some("1".to_string()),
        ..unbooted_chroot
    };
    assert_eq!(everything.evidence(), Some(Evidence::Offline("1".to_string())));
}

/// This host is booted with systemd and not offline: its manager is asked.
#[skuld::test]
fn this_host_has_a_manager_to_ask() {
    assert_eq!(Host::this().evidence(), None);
}

/// `SYSTEMD_OFFLINE` is parsed as systemd's `parse_boolean` parses it, but only its true half is
/// evidence: anything else — a false value, or one that does not parse — leaves the manager to be
/// asked, and is not passed on to the `systemctl` that asks it ([`DENIED`]).
#[skuld::test]
fn systemd_offline_is_evidence_only_when_true() {
    for value in ["1", "yes", "Y", "TRUE", "t", "on"] {
        let host = Host {
            offline: Some(value.to_string()),
            unbooted: false,
            chroot: None,
        };
        assert_eq!(host.evidence(), Some(Evidence::Offline(value.to_string())), "{value:?}");
    }
    for value in ["0", "no", "false", "off", "", "2", "maybe"] {
        let host = Host {
            offline: Some(value.to_string()),
            unbooted: false,
            chroot: None,
        };
        assert_eq!(host.evidence(), None, "{value:?}");
    }
}

/// The chroot `systemctl` itself detects — `/proc/1/root` against `/`, or the absence that leaves
/// neither readable — as it reports it: `show` exits `0`, prints nothing, and says why on stderr,
/// in 246's words and newer ones' (measured on 257), 242 to 245's, or those with no verb to name
/// (see [`IGNORED`]). The gate refuses it, naming the report. A chroot the *environment* declares
/// is not among the causes: [`DENIED`] takes every such switch off the child.
///
/// Five settings stop a real one reporting it, MEASURED in that chroot on 255 and 257:
/// `SYSTEMD_LOG_LEVEL=emerg` or `SYSTEMD_LOG_TARGET=null`, which discard the line rather than the
/// check; `SYSTEMD_IGNORE_CHROOT` **true**; `SYSTEMD_OFFLINE` **false**, on both versions; and
/// `SYSTEMD_IN_CHROOT` **false**, on 257 only. The value is what decides: `SYSTEMD_IGNORE_CHROOT=0`
/// and `SYSTEMD_IN_CHROOT=1` leave it reporting, and `SYSTEMD_OFFLINE=maybe` does not parse, so it
/// falls through to the chroot check. The stand-in asks something stricter and simpler — that the
/// child carry [`ENVIRONMENT`] and nothing else under [`DENIED`] — because what is under test is
/// goetia's removal, not systemd's reading. That an *inherited* name is removed is the half no test
/// on this thread can show — putting a variable in this process's environment would race every
/// other test thread — and is proved end to end against a real `systemctl` by
/// `tests/systemd_integration/no_manager.rs`.
#[skuld::test]
fn a_chroot_systemctl_reports_is_refused_and_cannot_be_silenced() {
    for notice in [
        "Running in chroot, ignoring command 'show'",
        "Running in chroot, ignoring request: show",
        "Running in chroot, ignoring request.",
    ] {
        let script = format!(
            "[ \"$1\" = show ] && {{ {ONLY_GOETIAS_OWN_SWITCHES} && echo \"{notice}\" >&2; exit 0; }}; \
             echo 'systemd 257 (257)'"
        );
        assert_no_manager(supported_by(&script), notice, notice);
    }
}

/// What a `systemctl` goetia ran carries of [`DENIED`], as shell: [`ENVIRONMENT`]'s three names
/// with [`ENVIRONMENT`]'s values, and nothing else under the prefix. Asked of `env`, so it is the
/// child's environment as it arrived rather than a list of the switches a real `systemctl` happens
/// to read — which is the point of removing by prefix.
const ONLY_GOETIAS_OWN_SWITCHES: &str = "[ \"$(env | grep -c '^SYSTEMD_')\" = 3 ] \
                                         && [ \"$SYSTEMD_COLORS\" = 0 ] \
                                         && [ \"$SYSTEMD_LOG_LEVEL\" = info ] \
                                         && [ \"$SYSTEMD_LOG_TARGET\" = console ]";

/// What [`environment_from`] gives one child, recorded rather than spawned.
#[derive(Default)]
struct Recorded {
    given: Vec<(String, String)>,
    denied: Vec<String>,
}

impl Environment for Recorded {
    fn set(&mut self, key: &str, value: &str) {
        self.given.push((key.to_string(), value.to_string()));
    }

    fn remove(&mut self, key: &str) {
        self.denied.push(key.to_string());
    }
}

/// Every inherited name under [`DENIED`] is *removed* from a child, including one whose name this
/// file does not contain and cannot: it is made at run time from this process's pid, so no denylist
/// of literals — the shape [`DENIED`] replaced, twice — can hold it, and only the prefix can select
/// it. `SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET` is a name no systemd reads but this file
/// does spell, so it pins reach beyond the switches systemd has today and nothing more. Nothing
/// outside the prefix is touched, and [`ENVIRONMENT`] is set rather than removed —
/// `SYSTEMD_IN_CHROOT=0` would assert there is no chroot and `=1` one everywhere, so a value is
/// never asserted for a switch goetia does not own.
#[skuld::test]
fn every_inherited_systemd_switch_is_removed_from_a_child_and_nothing_else_is() {
    let unlistable = format!("SYSTEMD_H{}", std::process::id());
    let inherited = [
        "SYSTEMD_IGNORE_CHROOT",
        "SYSTEMD_IN_CHROOT",
        "SYSTEMD_OFFLINE",
        "SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET",
        unlistable.as_str(),
        "SYSTEMD_COLORS",
        "SYSTEMD_LOG_LEVEL",
        "SYSTEMD_LOG_TARGET",
        "PATH",
        "SYSTEMDNOUNDERSCORE",
        "NOT_SYSTEMD_EITHER",
    ];
    let mut recorded = Recorded::default();
    environment_from(&mut recorded, inherited.iter().map(|key| key.to_string()));
    assert_eq!(
        recorded.denied,
        [
            "SYSTEMD_IGNORE_CHROOT",
            "SYSTEMD_IN_CHROOT",
            "SYSTEMD_OFFLINE",
            "SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET",
            unlistable.as_str(),
        ]
    );
    assert_eq!(recorded.given, ENVIRONMENT.map(|(k, v)| (k.to_string(), v.to_string())));
    for (key, value) in &recorded.given {
        assert!(
            !recorded.denied.contains(key),
            "{key}={value} is set as well as removed"
        );
    }
}

/// On a system booted with systemd, a manager that could not be asked is not evidence that none
/// runs: a bus that timed out or refused the connection is a refusal naming what `systemctl` said,
/// and so is a `show` that answered with nothing and did not say it ignored the question.
#[skuld::test]
fn any_other_failure_to_ask_the_manager_is_a_refusal() {
    let failing = "[ \"$1\" = show ] && { echo 'Failed to get properties: Connection timed out' >&2; exit 1; }; \
                   echo 'systemd 257 (257)'";
    let msg = supported_by(failing).unwrap_err().to_string();
    assert!(msg.contains("`systemctl show --property=Version` failed"), "{msg}");
    assert!(msg.contains("Connection timed out"), "{msg}");
    assert!(msg.contains("cannot tell whether the running systemd is 242+"), "{msg}");

    let silent = "[ \"$1\" = show ] && exit 0; echo 'systemd 257 (257)'";
    let msg = supported_by(silent).unwrap_err().to_string();
    assert!(msg.contains("printed no version"), "{msg}");
}

/// Both probes switch colour off: the stand-in colours whatever it prints unless it sees
/// `SYSTEMD_COLORS=0`, as a real `systemctl` under an inherited `SYSTEMD_COLORS=1` does.
#[skuld::test]
fn every_probe_switches_colour_off() {
    let colours = "c() { [ \"$SYSTEMD_COLORS\" = 0 ] && echo \"$1\" || printf '\\033[0;1;39m%s\\033[0m\\n' \"$1\"; }; ";
    assert!(supported_by(&format!("{colours}[ \"$1\" = show ] && c 257 || c 'systemd 257'")).is_ok());
}

/// The real `systemctl` on this host, first with an inherited `SYSTEMD_COLORS=1` — set by the
/// wrapper only where goetia did not set it itself — then as it is.
#[skuld::test]
fn this_hosts_systemd_is_supported() {
    {
        let _stand_in = stand_in::set(
            Some("SYSTEMD_COLORS=${SYSTEMD_COLORS:-1} exec systemctl \"$@\""),
            Host::this(),
        );
        supported().expect("the host running the tests runs systemd 242+, whatever colour is inherited");
    }
    require_supported().expect("the host running the tests runs systemd 242+");
}
