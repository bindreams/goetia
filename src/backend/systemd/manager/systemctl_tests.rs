//! Unit coverage for `systemctl.rs`: the diagnostic [`failed`] builds out of a [`Capture`], across
//! the shapes a capture can arrive in, and the argv and environment each budget runs under; the
//! manager boundary and the version gate, driven through a [`Fake`] table of stated replies; and
//! what a real child does, driven through a [`Real`] client whose program is `sh`.

use super::*;
use crate::backend::client::fake::{Entry, Fake, RoleKind, Waited, exited, fail, reply};
use crate::backend::{bounded, standby};
use std::os::unix::process::ExitStatusExt as _;
use std::time::Duration;

/// A `systemctl` that exited with `code`.
fn exit(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

fn capture(stdout: &str, stderr: &str, complete: bool) -> Capture {
    Capture {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        complete,
    }
}

// The clients =========================================================================================================

/// A system booted with systemd, and not offline: its manager must be asked.
pub(in crate::backend::systemd::manager) fn booted() -> Host {
    Host {
        offline: None,
        unbooted: false,
        chroot: None,
    }
}

/// A host told to ask no manager, by `SYSTEMD_OFFLINE` set to `value`.
fn offline(value: &str) -> Host {
    Host {
        offline: Some(value.to_string()),
        ..booted()
    }
}

fn offline_one() -> Host {
    offline("1")
}

fn offline_yes() -> Host {
    offline("yes")
}

/// A host with no `/run/systemd/system`.
pub(in crate::backend::systemd::manager) fn unbooted() -> Host {
    Host {
        unbooted: true,
        ..booted()
    }
}

/// A host whose `/` is a root of its own, found as `chroot` says.
fn chrooted(chroot: Chroot) -> Host {
    Host {
        chroot: Some(chroot),
        ..booted()
    }
}

fn chroot_root() -> Host {
    chrooted(Chroot::Root)
}

fn chroot_mount() -> Host {
    chrooted(Chroot::Mount)
}

fn chroot_no_mount() -> Host {
    chrooted(Chroot::NoMount)
}

/// `systemctl` over `client`, on a booted host.
pub(in crate::backend::systemd::manager) fn on_booted(client: impl Client + 'static) -> Systemctl {
    Systemctl::new(Box::new(client), booted)
}

/// `systemctl` as a real child, `sh -c <script>` with `arg0` as `$0`, on a booted host: for a test
/// about the child itself, such as its environment or a wait only a deadline ends.
fn real_sh(script: &str, arg0: &str) -> Systemctl {
    on_booted(Real::new(
        ["/bin/sh", "-c", script, arg0].map(String::from).to_vec(),
        environment,
    ))
}

/// A reservation whose version gate counts as passed, so a request asks no probe: for a program
/// that answers one verb only.
fn gate_held() -> Held {
    Held {
        gate_passed: true,
        ..Held::default()
    }
}

/// [`gate_held`], holding the threads one watched request reads its two streams with.
fn watching() -> Held {
    Held {
        draws: Reservation::of_listeners(bounded::STREAMS).expect("threads for one watched request"),
        ..gate_held()
    }
}

/// The version gate's two probes, passing.
pub(in crate::backend::systemd::manager) fn gate_passes() -> Vec<Entry> {
    vec![
        reply(&["--version"], exited(0, "systemd 257 (257)\n", "")),
        reply(&["show", "--property=Version", "--value"], exited(0, "257\n", "")),
    ]
}

/// A fake that passes the version gate, and then answers `rest`.
pub(in crate::backend::systemd::manager) fn gate_then(rest: impl IntoIterator<Item = Entry>) -> Fake {
    Fake::new(gate_passes().into_iter().chain(rest))
}

/// A wait that failed after the client started.
fn lost(announced: bool) -> ClientError {
    ClientError::Lost {
        error: std::io::Error::other("the wait failed").into(),
        announced,
    }
}

// failed ==============================================================================================================

#[skuld::test]
fn a_failure_is_reported_with_what_systemctl_wrote_on_stderr() {
    let e = failed(
        "start",
        "x.service",
        exit(1),
        &capture("", "Unit x.service not found.\n", true),
    );
    assert_eq!(
        e.report().to_string(),
        "`systemctl start x.service` failed (exit status: 1): Unit x.service not found."
    );
}

/// The stdout fallback. `systemctl` writes its failures to stderr, so a
/// capture with only stdout is the case nothing else in the tree produces —
/// and the case that would otherwise report "wrote no diagnostic" when
/// systemd did write one.
#[skuld::test]
fn a_failure_that_wrote_only_to_stdout_is_still_reported_with_what_it_wrote() {
    let e = failed(
        "stop",
        "x.service",
        exit(1),
        &capture("something on stdout\n", "", true),
    );
    assert_eq!(
        e.report().to_string(),
        "`systemctl stop x.service` failed (exit status: 1): something on stdout"
    );
}

/// stderr wins when both streams carried something: that is where
/// `systemctl` puts the diagnostic, and the fallback exists for the other
/// case only.
#[skuld::test]
fn stderr_is_preferred_over_stdout_when_both_carry_something() {
    let e = failed(
        "start",
        "x.service",
        exit(1),
        &capture("noise\n", "the real diagnostic\n", true),
    );
    assert_eq!(
        e.report().to_string(),
        "`systemctl start x.service` failed (exit status: 1): the real diagnostic"
    );
}

/// A truncated capture must say so: a prefix presented as the whole story
/// reads as "systemd said this and stopped", when what happened is that
/// goetia stopped reading.
#[skuld::test]
fn a_truncated_diagnostic_says_it_may_be_only_a_prefix() {
    let e = failed("start", "x.service", exit(1), &capture("", "Unit x.ser", false));
    let msg = e.report().to_string();
    assert!(msg.contains("Unit x.ser"), "{msg}");
    assert!(msg.contains("may be only a prefix"), "{msg}");
    assert!(msg.contains("budget expired"), "{msg}");
}

/// Both empty, under either `complete`. An empty diagnostic must not render a colon with nothing
/// after it, and a truncated capture says "goetia's budget expired first" rather than nothing.
#[skuld::test]
fn an_empty_diagnostic_is_explained_rather_than_trailing_off_after_a_colon() {
    for complete in [true, false] {
        let msg = failed("start", "x.service", exit(1), &capture("", "", complete))
            .report()
            .to_string();
        assert!(
            !msg.ends_with(": "),
            "an empty diagnostic must not trail off after a colon (complete={complete}): {msg:?}"
        );
        assert!(
            msg.contains("`systemctl start x.service` failed"),
            "complete={complete}: {msg}"
        );
        assert_eq!(
            msg.contains("budget expired"),
            !complete,
            "only a truncated capture says why it is empty (complete={complete}): {msg}"
        );
    }
}

// verb_args ===========================================================================================================

/// `--no-block` is the whole of what a non-waiting budget means to systemd, and no elevated test
/// can see it: `start` returns either way, so dropping the flag leaves that suite green (measured:
/// 37/37 still passed without it). The argv is observable with no timing bet at all, which is why
/// the mechanism is pinned here rather than through a live `systemctl`.
#[skuld::test]
fn a_budget_that_does_not_wait_asks_systemd_not_to_block() {
    for budget in [Budget::Immediate, Budget::Bounded(Duration::ZERO)] {
        assert_eq!(
            verb_args("start", "x.service", budget),
            ["start", "--no-block", "--show-transaction", "--", "x.service"],
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
        ["stop", "--show-transaction", "--", "x.service"]
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

/// An id may start with `-` (`is_valid_id` allows it), and a unit name that does is read by
/// `systemctl` as an option unless `--` precedes it: `-h.service` is `-h`, which prints help and
/// exits `0` having done nothing. `systemctl` parses with `getopt_long`, which stops at `--`.
#[skuld::test]
fn start_stop_and_restart_put_the_unit_after_the_option_terminator() {
    for budget in [
        Budget::Immediate,
        Budget::Unbounded,
        Budget::Bounded(Duration::from_secs(10)),
    ] {
        for verb in ["start", "stop", "restart"] {
            let args = verb_args(verb, "-h.service", budget);
            assert_eq!(args[0], verb, "{budget:?}");
            assert_eq!(args[args.len() - 2..], ["--", "-h.service"], "{verb} {budget:?}");
        }
    }
}

/// `enable` and `disable` over `-h`, which `systemctl` would read as `-h` and print its help for,
/// exit `0`, and change nothing. Run through the functions `Systemd::enable`, `Systemd::disable` and
/// `uninstall` call.
#[skuld::test]
fn enable_and_disable_put_the_unit_after_the_option_terminator() {
    type Verb = fn(&Systemctl, &str, &mut Held) -> Result<()>;
    for (verb, run) in [
        ("enable", Systemctl::enable_impl as Verb),
        ("disable", Systemctl::disable_impl as Verb),
    ] {
        let fake = gate_then([reply(&[verb], exited(0, "", ""))]);

        run(&on_booted(fake.clone()), "-h", &mut Held::default()).unwrap();

        assert_eq!(fake.calls()[2], [verb, "--", "-h.service"]);
    }
}

#[skuld::test]
fn enable_and_disable_failures_name_the_verb_the_unit_and_systemd_s_words() {
    type Verb = fn(&Systemctl, &str, &mut Held) -> Result<()>;
    for (verb, run) in [
        ("enable", Systemctl::enable_impl as Verb),
        ("disable", Systemctl::disable_impl as Verb),
    ] {
        let fake = gate_then([reply(&[verb], exited(1, "", "Access denied\n"))]);

        let msg = run(&on_booted(fake), "x", &mut Held::default())
            .expect_err(verb)
            .report()
            .to_string();

        assert!(msg.contains(&format!("`systemctl {verb} x.service` failed")), "{msg}");
        assert!(msg.contains("Access denied"), "{msg}");
    }
}

#[skuld::test]
fn show_puts_the_unit_after_the_option_terminator() {
    let fake = Fake::new([reply(&["show"], exited(1, "", ""))]);

    let _ = on_booted(fake.clone()).status_from_unit("-h.service");

    assert_eq!(
        fake.calls(),
        [[
            "show",
            "--property",
            "ActiveState,MainPID,UnitFileState,LoadState",
            "--",
            "-h.service"
        ]]
    );
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

/// `systemctl` as [`ANNOUNCES_THEN_BLOCKS`], touching `request`.
fn announces_then_blocks(request: &std::path::Path) -> Systemctl {
    real_sh(ANNOUNCES_THEN_BLOCKS, request.to_str().unwrap())
}

/// A budget that waits under a deadline is watched, with the announcement its request is held out
/// for; any other runs to completion, with nothing to bound. The argv is the budget's own.
#[skuld::test]
fn each_budget_takes_its_path() {
    let announced = Waited {
        role: RoleKind::AnnouncedRequest,
        bounded: true,
    };
    let completed = Waited {
        role: RoleKind::ToCompletion,
        bounded: false,
    };
    for (budget, waited, argv) in [
        (
            Budget::Immediate,
            completed,
            ["start", "--no-block", "--show-transaction", "--", "x.service"].as_slice(),
        ),
        (
            Budget::Unbounded,
            completed,
            ["start", "--show-transaction", "--", "x.service"].as_slice(),
        ),
        (
            Budget::Bounded(Duration::from_secs(10)),
            announced,
            ["start", "--show-transaction", "--", "x.service"].as_slice(),
        ),
    ] {
        let fake = gate_then([reply(
            &["start"],
            exited(0, "", "Enqueued anchor job 1 x.service/start.\n"),
        )]);
        on_booted(fake.clone())
            .start_impl(
                "x",
                budget,
                budget.start(),
                &mut Held::of(&[Step::Start], budget).unwrap(),
            )
            .expect("the job was enqueued");
        assert_eq!(fake.calls()[2], argv, "{budget:?}");
        assert_eq!(fake.waits()[2], waited, "{budget:?}");
    }
}

/// The bounded path waits on the `deadline` it is handed — derived at verb entry, so it already
/// covers the spawn — never on one re-derived from `budget`.
///
/// The child cannot exit on its own and the deadline is spent, so `Expired` is the only answer and
/// no timing is bet on. `Bounded(Duration::MAX)` is what makes a re-derivation fail: it overflows
/// `Instant` into an unbounded deadline, so a `run_verb` that ignored `deadline` would wait on the
/// child forever.
#[skuld::test]
fn the_bounded_path_waits_on_the_deadline_it_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let finished = announces_then_blocks(&dir.path().join("request"))
        .run_verb(
            "start",
            "x.service",
            Budget::Bounded(Duration::MAX),
            Budget::Immediate.start(),
            &mut watching(),
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
    announces_then_blocks(&request)
        .run_verb(
            "start",
            "x.service",
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Immediate.start(),
            &mut watching(),
        )
        .expect("sh is spawnable");
    assert!(
        request.exists(),
        "the request must be issued whatever is left of the budget"
    );
}

/// A watched `systemctl` reads its stdout and stderr with the threads its reservation holds, so
/// none is made once it is spawned: the request cannot go out with nothing to watch it. A host that
/// cannot make them refuses the verb before it sends anything: see
/// `a_verb_short_of_threads_is_refused_before_it_sends_anything`.
#[skuld::test]
fn a_watched_request_draws_its_threads_from_its_reservation() {
    let mut held = watching();
    let _no_thread = standby::test_hook::threads(0);
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");

    let finished = announces_then_blocks(&request)
        .run_verb(
            "start",
            "x.service",
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Immediate.start(),
            &mut held,
        )
        .expect("its threads were reserved");

    assert!(matches!(finished, Finished::Expired), "{finished:?}");
    assert!(request.exists());
    assert_eq!(
        bounded::test_hook::listeners_left(&held.draws),
        0,
        "its threads were spent"
    );
}

/// A watched `systemctl` that may have run is a request in doubt — exit `4`, saying it may or may
/// not have reached systemd — and one that never ran is a plain failure, which says it was not.
#[skuld::test]
fn a_watched_request_that_may_have_run_is_in_doubt() {
    let start = |error: ClientError| {
        on_booted(gate_then([fail(&["start"], error)])).start_impl(
            "x",
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Unbounded.start(),
            &mut Held::of(&[Step::Start], Budget::DEFAULT).unwrap(),
        )
    };
    let may_or_may_not = |e: &Error| {
        assert!(matches!(e, Error::RequestInDoubt { reached: false, .. }), "{e:?}");
        let msg = e.to_string();
        assert!(msg.contains("may have started before it failed"), "{msg}");
        assert!(msg.contains("may or may not have reached the service manager"), "{msg}");
    };
    may_or_may_not(
        &start(ClientError::MayHaveRun(cosca::error::Error::Containment {
            detail: "forced".into(),
            source: None,
        }))
        .expect_err("injected"),
    );

    let e = start(ClientError::NotRun(
        std::io::Error::from_raw_os_error(libc::EAGAIN).into(),
    ))
    .expect_err("injected");
    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
    assert!(!e.is_indeterminate(), "{e:?}");

    // Past the spawn `systemctl` runs, so a wait that fails leaves its request in doubt too.
    may_or_may_not(&start(lost(false)).expect_err("injected"));
}

/// A request lost after `systemctl` announced its job had reached systemd, and says so: its outcome
/// is what is unconfirmed, never whether it arrived.
#[skuld::test]
fn a_watched_request_lost_after_its_announcement_reached_the_manager() {
    let e = on_booted(gate_then([fail(&["start"], lost(true))]))
        .start_impl(
            "x",
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Unbounded.start(),
            &mut Held::of(&[Step::Start], Budget::DEFAULT).unwrap(),
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

/// A request run to completion keeps the same two apart: one that never ran was never sent, and one
/// that may have run is in doubt.
#[skuld::test]
fn a_request_run_to_completion_is_in_doubt_only_once_it_ran() {
    let e = on_booted(Real::new(vec!["/nonexistent/systemctl".to_string()], environment))
        .enable_impl("x", &mut gate_held())
        .expect_err("no such program");
    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
    assert!(!e.is_indeterminate(), "{e:?}");

    let e = on_booted(Fake::new([fail(
        &["enable"],
        ClientError::MayHaveRun(std::io::Error::other("the read failed").into()),
    )]))
    .enable_impl("x", &mut gate_held())
    .expect_err("injected");
    assert!(matches!(e, Error::RequestInDoubt { reached: false, .. }), "{e:?}");
}

/// A `daemon-reload` in doubt after `install` wrote the unit stays in doubt, exit `4`, and says
/// the unit was written; any other failure there is exit `1`, and says so too.
#[skuld::test]
fn a_reload_in_doubt_after_the_write_stays_in_doubt() {
    let e = on_booted(gate_then([fail(&["daemon-reload"], lost(false))]))
        .daemon_reload_or_report("x", &mut Held::default())
        .expect_err("injected");
    assert!(
        matches!(&e, Error::RequestInDoubt { request, reached: false, .. } if request == "systemctl daemon-reload"),
        "{e:?}"
    );
    let msg = e.report().to_string();
    assert!(msg.contains("the unit file for `x` is on disk"), "{msg}");
    assert!(msg.contains("the wait failed"), "{msg}");

    let refused = gate_then([reply(&["daemon-reload"], exited(1, "", "Access denied\n"))]);
    let e = on_booted(refused)
        .daemon_reload_or_report("x", &mut Held::default())
        .expect_err("refused");
    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert!(!e.is_indeterminate(), "{e:?}");
    let msg = e.report().to_string();
    assert!(msg.contains("the unit file for `x` is on disk"), "{msg}");
    assert!(msg.contains("Access denied"), "{msg}");
}

/// A watched `systemctl` needs no temp file: it runs where none may be writable.
#[skuld::test]
fn a_watched_request_needs_no_temp_file() {
    let mut held = watching();
    let _no_file = bounded::test_hook::temp_files(0);
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    let finished = announces_then_blocks(&request)
        .run_verb(
            "start",
            "x.service",
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Immediate.start(),
            &mut held,
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
        let finished = real_sh(&format!("{UNLESS_AUDIBLE_EXIT_9}; exit 0"), "systemctl")
            .run_verb("start", "x.service", budget, budget.start(), &mut gate_held())
            .expect("sh is spawnable");
        assert!(
            matches!(&finished, Finished::Exited { status, .. } if status.success()),
            "{budget:?}: {finished:?}"
        );
    }
}

#[skuld::test]
fn the_bounded_path_keeps_the_announcement_audible() {
    let finished = real_sh(
        &format!("{UNLESS_AUDIBLE_EXIT_9}; echo 'Enqueued anchor job 1 x.service/start.' >&2; exec sleep 2147483647"),
        "systemctl",
    )
    .run_verb(
        "start",
        "x.service",
        Budget::Bounded(Duration::from_secs(10)),
        Budget::Immediate.start(),
        &mut watching(),
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
        exit(1),
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
        e.report().to_string(),
        "`systemctl start x.service` failed (exit status: 1): Job for x.service failed because the control process \
         exited with error code."
    );
}

/// Stderr that carried nothing but protocol said nothing, so stdout is what gets reported: the
/// fallback asks what is left of stderr once the protocol is gone, not whether it had any bytes.
#[skuld::test]
fn a_failure_whose_stderr_is_only_protocol_is_reported_with_its_stdout() {
    let e = failed(
        "start",
        "x.service",
        exit(1),
        &capture(
            "something on stdout\n",
            "Enqueued anchor job 7 x.service/start.\n",
            true,
        ),
    );
    assert_eq!(
        e.report().to_string(),
        "`systemctl start x.service` failed (exit status: 1): something on stdout"
    );
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
        let e = failed("start", "x.service", exit(1), &capture("", &stderr, true));
        assert_eq!(
            e.report().to_string(),
            format!(
                "`systemctl start x.service` failed (exit status: 1): {prefix}Job for x.service failed because the control process \
                 exited with error code."
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
        exit(1),
        &capture("", "Enqueued anchor job 7 x.service/start.\n", false),
    )
    .report()
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

type Run = Box<dyn Fn(&Systemctl) -> Result<()>>;

/// One way in to the manager, named.
struct Way {
    name: String,
    /// A request asks the version gate's probes first.
    gated: bool,
    run: Run,
}

impl Way {
    fn request(name: impl Into<String>, run: impl Fn(&Systemctl) -> Result<()> + 'static) -> Way {
        Way {
            name: name.into(),
            gated: true,
            run: Box::new(run),
        }
    }
}

/// Every path goetia reaches the manager by, each through the function the verbs call: the three
/// requests run to completion, the `show` a state read runs, and `start`, `stop` and `restart` under
/// every budget, bounded or not.
fn every_path() -> Vec<Way> {
    let mut paths = vec![
        Way::request("daemon-reload", |s: &Systemctl| s.daemon_reload(&mut Held::default())),
        Way::request("enable", |s: &Systemctl| s.enable_impl("x", &mut Held::default())),
        Way::request("disable", |s: &Systemctl| s.disable_impl("x", &mut Held::default())),
        Way {
            name: "show".to_string(),
            gated: false,
            run: Box::new(|s: &Systemctl| s.status_from_unit("x.service").map(drop)),
        },
        Way::request("restart", |s: &Systemctl| {
            s.request_restart_impl("x", &mut Held::default())
        }),
        Way::request("enable timer", |s: &Systemctl| {
            s.enable_timer("x", &mut Held::default())
        }),
        Way::request("disable timer", |s: &Systemctl| {
            s.disable_timer("x", &mut Held::default())
        }),
        Way::request("clean timer", |s: &Systemctl| s.clean_timer("x", &mut Held::default())),
        Way::request("reset-failed timer", |s: &Systemctl| {
            s.reset_timer("x", &mut Held::default())
        }),
        Way {
            name: "show stamp".to_string(),
            gated: false,
            run: Box::new(|s: &Systemctl| s.timer_stamp("x").map(drop)),
        },
        Way::request("start job", |s: &Systemctl| {
            s.start_job("x", &mut Held::default()).map(drop)
        }),
        Way {
            name: "show task".to_string(),
            gated: false,
            run: Box::new(|s: &Systemctl| s.task_state("x.service").map(drop)),
        },
        Way {
            name: "show timer arming".to_string(),
            gated: false,
            run: Box::new(|s: &Systemctl| s.timer_arming("x").map(drop)),
        },
    ];
    for budget in [
        Budget::Immediate,
        Budget::Unbounded,
        Budget::Bounded(Duration::from_secs(600)),
    ] {
        paths.push(Way::request(format!("start {budget:?}"), move |s: &Systemctl| {
            s.start_impl(
                "x",
                budget,
                budget.start(),
                &mut Held::of(&[Step::Start], budget).unwrap(),
            )
        }));
        paths.push(Way::request(format!("stop {budget:?}"), move |s: &Systemctl| {
            s.stop_impl(
                "x",
                budget,
                budget.start(),
                &mut Held::of(&[Step::Stop], budget).unwrap(),
            )
        }));
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
/// gate's own probes included: the fake has no entries, so a single call would fail the test.
#[skuld::test]
fn evidence_known_without_asking_refuses_every_path_before_anything_runs() {
    type Shape = fn() -> Host;
    let hosts: [(Shape, &str); 5] = [
        (offline_one, "`SYSTEMD_OFFLINE=1` is set"),
        (
            unbooted,
            "`/run/systemd/system`, which systemd makes when it boots a system, does not exist here",
        ),
        (
            chroot_root,
            "`/` is not PID 1's root (`/proc/1/root`), so goetia runs under another root than PID 1, as in a \
             chroot or a container sharing the host's PID namespace",
        ),
        (
            chroot_mount,
            "the mount at `/` is none of PID 1's (`/proc/self/mountinfo`, `/proc/1/mountinfo`), so goetia runs \
             under another root than PID 1",
        ),
        (
            chroot_no_mount,
            "no mount is at `/` (`/proc/self/mountinfo`): `/` is a directory inside one, so goetia runs in a \
             chroot",
        ),
    ];
    for (host, evidence) in hosts {
        let fake = Fake::new([]);
        let systemctl = Systemctl::new(Box::new(fake.clone()), host);
        assert_no_manager(systemctl.require_supported(&mut Held::default()), evidence, "the gate");
        for Way { name, run, .. } in every_path() {
            assert_no_manager(run(&systemctl), evidence, &name);
        }
        assert!(fake.calls().is_empty(), "{evidence}: systemctl ran: {:?}", fake.calls());
    }
}

/// A chroot only `systemctl` can report, and reports only once asked. No request is sent until
/// the manager has been asked: here every request would act silently and exit `0`, as `enable` and
/// `disable` do in a chroot, and none runs — a request would be a call past the fake's table. The
/// read is refused by its own answer.
#[skuld::test]
fn no_request_is_sent_before_the_manager_is_asked() {
    let chroot = "Running in chroot, ignoring command 'show'";
    for Way { name, gated, run } in every_path() {
        let asked = reply(&["show"], exited(0, "", &format!("{chroot}\n")));
        let fake = if gated {
            Fake::new([reply(&["--version"], exited(0, "systemd 257 (257)\n", "")), asked])
        } else {
            Fake::new([asked])
        };
        assert_no_manager(run(&on_booted(fake.clone())), chroot, &name);
        assert_eq!(fake.calls().len(), if gated { 2 } else { 1 }, "{name}");
    }
}

/// The backstop: a `systemctl` past the gate that says it ignored what it was asked is refused on
/// every path, and never read as a success: in every wording [`IGNORED`] lists, on either stream.
#[skuld::test]
fn a_systemctl_that_ignored_the_request_is_refused_on_every_path() {
    for (report, on_stderr) in [
        ("Running in chroot, ignoring command '$1'", true),
        ("Running in chroot, ignoring request: $1", true),
        ("Running in chroot, ignoring request.", true),
        ("Running in chroot, ignoring request.", false),
    ] {
        for Way { name, gated, run } in every_path() {
            let verb = name.split(' ').next().unwrap();
            let said = report.replace("$1", verb);
            let line = format!("{said}\n");
            let (stdout, stderr) = if on_stderr {
                ("", line.as_str())
            } else {
                (line.as_str(), "")
            };
            let probes = if gated { gate_passes() } else { Vec::new() };
            let fake = Fake::new(probes.into_iter().chain([reply(&[verb], exited(0, stdout, stderr))]));

            let result = run(&on_booted(fake.clone()));

            assert_no_manager(result, &said, &format!("{name} ({report:?}, stderr: {on_stderr})"));
            fake.assert_spent(&name);
        }
    }
}

/// The real `systemctl`, offline, on every path `start` and `stop` take: an exit of `0` that asked
/// nobody is the refusal, never read as started or stopped. It says so in 246's words and newer
/// ones', or 242 to 245's.
#[skuld::test]
fn a_real_offline_systemctl_is_refused_on_every_path() {
    let offline = on_booted(Real::new(
        ["env", "SYSTEMD_OFFLINE=1", "systemctl"].map(String::from).to_vec(),
        environment,
    ));
    for verb in ["start", "stop", "restart"] {
        for budget in [
            Budget::Immediate,
            Budget::Unbounded,
            Budget::Bounded(Duration::from_secs(600)),
        ] {
            let mut held = watching();
            let result = offline.run_verb(verb, "goetia-offline-probe.service", budget, budget.start(), &mut held);
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

/// `systemctl` whose `show` answers `stdout`, and exits `0`.
fn showing(stdout: &str) -> Systemctl {
    on_booted(Fake::new([reply(&["show"], exited(0, stdout, ""))]))
}

/// An answer without every property asked for is no answer: never the state, pid or enablement its
/// absence would default to. Empty — as a `systemctl` that asked nobody and did not say so answers
/// — or partial.
#[skuld::test]
fn a_show_answer_without_every_property_is_not_a_state() {
    for (answer, missing) in [
        ("", "ActiveState, MainPID, UnitFileState, LoadState"),
        ("ActiveState=active\n", "MainPID, UnitFileState, LoadState"),
        ("ActiveState=active\nMainPID=42\nUnitFileState=enabled\n", "LoadState"),
    ] {
        let e = showing(answer).status_from_unit("x.service").expect_err(answer);
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
        let answer = format!("ActiveState={active}\nMainPID=0\nUnitFileState={unit_file}\nLoadState=not-found\n");
        let msg = showing(&answer)
            .status_from_unit("x.service")
            .expect_err(&answer)
            .to_string();
        assert!(
            msg.contains("systemd has not loaded the unit file goetia finds installed for `x.service`"),
            "{answer}: {msg}"
        );
        assert!(msg.contains("`LoadState=not-found`"), "{answer}: {msg}");
    }
    for load in ["loaded", "masked", "bad-setting", "error"] {
        let answer = format!("ActiveState=inactive\nMainPID=0\nUnitFileState=\nLoadState={load}\n");
        let status = showing(&answer).status_from_unit("x.service").expect(&answer);
        assert_eq!(status.state, State::Stopped, "{answer}");
    }
}

/// Every property answered, an empty `UnitFileState` included — what systemd answers for a unit it
/// has not loaded.
#[skuld::test]
fn a_show_answer_with_every_property_is_read() {
    for (answer, expected) in [
        (
            "ActiveState=active\nMainPID=42\nUnitFileState=enabled\nLoadState=loaded\n",
            DaemonStatus {
                state: State::Running,
                pid: Some(42),
                enabled: true,
            },
        ),
        (
            "MainPID=0\nLoadState=loaded\nActiveState=inactive\nUnitFileState=\n",
            DaemonStatus {
                state: State::Stopped,
                pid: None,
                enabled: false,
            },
        ),
    ] {
        let status = showing(answer).status_from_unit("x.service").expect(answer);
        assert_eq!(
            (status.state, status.pid, status.enabled),
            (expected.state, expected.pid, expected.enabled),
            "{answer}"
        );
    }
}

// A task's job and timer ==============================================================================================

#[skuld::test]
fn the_job_number_is_read_from_the_announcement() {
    for (stderr, number) in [
        ("Enqueued anchor job 91 probe-t.service/start.\n", Some(91)),
        (
            "Enqueued anchor job 7 x.service/start.\nJob for x.service finished.\n",
            Some(7),
        ),
        (
            "Enqueued anchor job 12345678901 x.service/start.\n",
            Some(12_345_678_901),
        ),
        ("Enqueued anchor job  x.service/start.\n", None),
        ("Enqueued auxiliary job 4 x.service/start.\n", None),
        ("", None),
    ] {
        assert_eq!(job_number(stderr.as_bytes()), number, "{stderr:?}");
    }
}

/// A task's start never waits, whatever it is given: the request is `--no-block`, run to
/// completion with no listener or deadline, and answers the job's number.
#[skuld::test]
fn start_job_asks_for_the_run_and_answers_the_job_number() {
    let fake = Fake::new([reply(
        &["start"],
        exited(0, "", "Enqueued anchor job 91 x.service/start.\n"),
    )]);

    let job = on_booted(fake.clone())
        .start_job("x", &mut gate_held())
        .expect("start_job");

    assert_eq!(job, 91);
    assert_eq!(
        fake.calls(),
        [["start", "--no-block", "--show-transaction", "--", "x.service"]]
    );
    assert_eq!(
        fake.waits(),
        [Waited {
            role: RoleKind::ToCompletion,
            bounded: false
        }]
    );
}

#[skuld::test]
fn start_job_refuses_an_exit_that_enqueued_nothing_and_a_failed_start() {
    let none = Fake::new([reply(&["start"], exited(0, "", ""))]);
    let e = on_booted(none)
        .start_job("x", &mut gate_held())
        .expect_err("no job was announced");
    assert!(matches!(e, Error::NotEnqueued { .. }), "{e:?}");

    let failed = Fake::new([reply(&["start"], exited(1, "", "Unit x.service not found.\n"))]);
    let msg = on_booted(failed)
        .start_job("x", &mut gate_held())
        .expect_err("a failed start")
        .report()
        .to_string();
    assert!(msg.contains("Unit x.service not found."), "{msg}");
}

#[skuld::test]
fn the_timer_is_enabled_and_disabled_now_and_named_after_the_terminator() {
    type Verb = fn(&Systemctl, &str, &mut Held) -> Result<()>;
    for (verb, run) in [
        ("enable", Systemctl::enable_timer as Verb),
        ("disable", Systemctl::disable_timer as Verb),
    ] {
        let fake = Fake::new([reply(&[verb], exited(0, "", ""))]);

        run(&on_booted(fake.clone()), "-h", &mut gate_held()).expect(verb);

        assert_eq!(fake.calls(), [[verb, "--now", "--", "-h.timer"]]);
    }
}

#[skuld::test]
fn a_timer_verb_that_failed_names_the_verb_the_timer_and_systemd_s_words() {
    let fake = gate_then([reply(&["enable"], exited(1, "", "Access denied\n"))]);

    let msg = on_booted(fake)
        .enable_timer("x", &mut Held::default())
        .expect_err("enable")
        .report()
        .to_string();

    assert!(msg.contains("`systemctl enable --now x.timer` failed"), "{msg}");
    assert!(msg.contains("Access denied"), "{msg}");
}

#[skuld::test]
fn a_timer_s_stamp_is_cleaned_by_state_and_the_timer_named_after_the_terminator() {
    let fake = Fake::new([reply(&["clean"], exited(0, "", ""))]);

    on_booted(fake.clone())
        .clean_timer("-h", &mut gate_held())
        .expect("clean");

    assert_eq!(fake.calls(), [["clean", "--what=state", "--", "-h.timer"]]);
}

#[skuld::test]
fn a_clean_that_failed_names_the_verb_the_timer_and_systemd_s_words() {
    let fake = gate_then([reply(
        &["clean"],
        exited(
            1,
            "",
            "Failed to clean unit x.timer: Unit is not inactive or has pending job.\n",
        ),
    )]);

    let msg = on_booted(fake)
        .clean_timer("x", &mut Held::default())
        .expect_err("clean")
        .report()
        .to_string();

    assert!(msg.contains("`systemctl clean --what=state x.timer` failed"), "{msg}");
    assert!(msg.contains("not inactive"), "{msg}");
}

/// A timer's stamp is cleared only by an arm that finds the timer inactive or failed and `CanClean`
/// naming `state`. A failed timer is reset before its stamp is cleaned: `clean` refuses it.
#[skuld::test]
fn a_timer_s_stamp_is_cleared_when_it_is_settled_and_can_be_cleaned_of_state() {
    for (can_clean, active, expected) in [
        ("state", "inactive", Stamp::Clean),
        ("cache state", "inactive", Stamp::Clean),
        ("state", "failed", Stamp::ResetThenClean),
        ("state", "active", Stamp::Left),
        ("", "inactive", Stamp::Left),
        ("", "failed", Stamp::Left),
        ("cache", "inactive", Stamp::Left),
    ] {
        let answer = format!("CanClean={can_clean}\nActiveState={active}\nLoadState=loaded\n");
        assert_eq!(
            showing(&answer).timer_stamp("x").expect(&answer),
            expected,
            "{can_clean:?} {active:?}"
        );
    }
}

/// A state arming has no rule for is no "nothing to clean": the arm fails naming it.
#[skuld::test]
fn a_timer_in_a_state_arming_does_not_know_is_an_error_naming_it() {
    for active in ["activating", "deactivating", "maintenance", "reloading"] {
        let answer = format!("CanClean=state\nActiveState={active}\nLoadState=loaded\n");
        let msg = showing(&answer)
            .timer_stamp("x")
            .expect_err(&answer)
            .report()
            .to_string();
        assert_eq!(
            msg,
            format!("failed to arm `x.timer`: found it in state `{active}`, which goetia has no rule for")
        );
    }
}

#[skuld::test]
fn the_timer_is_asked_about_its_stamp_by_its_own_name_in_one_show() {
    let fake = Fake::new([timer_shows("state", "inactive")]);

    on_booted(fake.clone()).timer_stamp("x").expect("timer_stamp");

    assert_eq!(
        fake.calls(),
        [["show", "--property", "CanClean,ActiveState,LoadState", "--", "x.timer"]]
    );
}

fn timer_shows(can_clean: &str, active: &str) -> Entry {
    reply(
        &["show"],
        exited(
            0,
            &format!("CanClean={can_clean}\nActiveState={active}\nLoadState=loaded\n"),
            "",
        ),
    )
}

/// Arming asks the gate, then one `show`; an inactive timer that keeps a stamp is cleaned before it is
/// enabled now, and nothing else is.
#[skuld::test]
fn arming_a_stopped_timer_cleans_its_stamp_before_enabling_it_now() {
    let fake = gate_then([
        timer_shows("state", "inactive"),
        reply(&["clean"], exited(0, "", "")),
        reply(&["enable"], exited(0, "", "")),
    ]);

    on_booted(fake.clone())
        .arm_timer("x", &mut Held::default())
        .expect("arm");

    assert_eq!(
        fake.calls().into_iter().skip(2).collect::<Vec<_>>(),
        [
            vec!["show", "--property", "CanClean,ActiveState,LoadState", "--", "x.timer"],
            vec!["clean", "--what=state", "--", "x.timer"],
            vec!["enable", "--now", "--", "x.timer"],
        ]
    );
}

/// A failed timer is reset before its stamp is cleaned: `clean` refuses it.
#[skuld::test]
fn arming_a_failed_timer_resets_it_then_cleans_its_stamp_before_enabling_it_now() {
    let fake = gate_then([
        timer_shows("state", "failed"),
        reply(&["reset-failed"], exited(0, "", "")),
        reply(&["clean"], exited(0, "", "")),
        reply(&["enable"], exited(0, "", "")),
    ]);

    on_booted(fake.clone())
        .arm_timer("x", &mut Held::default())
        .expect("arm");

    assert_eq!(
        fake.calls().into_iter().skip(2).collect::<Vec<_>>(),
        [
            vec!["show", "--property", "CanClean,ActiveState,LoadState", "--", "x.timer"],
            vec!["reset-failed", "--", "x.timer"],
            vec!["clean", "--what=state", "--", "x.timer"],
            vec!["enable", "--now", "--", "x.timer"],
        ]
    );
}

/// A reset that fails fails the arm in systemd's words, with nothing cleaned or enabled.
#[skuld::test]
fn a_failing_reset_fails_the_arm() {
    let fake = gate_then([
        timer_shows("state", "failed"),
        reply(
            &["reset-failed"],
            exited(1, "", "Failed to reset failed state of unit x.timer: busy\n"),
        ),
    ]);

    let msg = on_booted(fake.clone())
        .arm_timer("x", &mut Held::default())
        .expect_err("the reset failed")
        .report()
        .to_string();

    assert!(msg.contains("`systemctl reset-failed x.timer` failed"), "{msg}");
    assert_eq!(fake.calls().len(), 4, "the gate's two probes, the show and the reset");
}

fn timer_not_found() -> Entry {
    reply(
        &["show"],
        exited(0, "CanClean=\nActiveState=inactive\nLoadState=not-found\n", ""),
    )
}

/// A timer file written and not reloaded reads `not-found`: the arm reloads and asks again, and still
/// cleans the stamp.
#[skuld::test]
fn arming_a_timer_systemd_has_not_loaded_reloads_asks_again_and_cleans() {
    let fake = gate_then([
        timer_not_found(),
        reply(&["daemon-reload"], exited(0, "", "")),
        timer_shows("state", "inactive"),
        reply(&["clean"], exited(0, "", "")),
        reply(&["enable"], exited(0, "", "")),
    ]);

    on_booted(fake.clone())
        .arm_timer("x", &mut Held::default())
        .expect("arm");

    let show = vec!["show", "--property", "CanClean,ActiveState,LoadState", "--", "x.timer"];
    assert_eq!(
        fake.calls().into_iter().skip(2).collect::<Vec<_>>(),
        [
            show.clone(),
            vec!["daemon-reload"],
            show,
            vec!["clean", "--what=state", "--", "x.timer"],
            vec!["enable", "--now", "--", "x.timer"],
        ]
    );
}

/// A timer systemd cannot see after a reload is still not a state: the arm fails with nothing enabled.
#[skuld::test]
fn a_timer_still_not_loaded_after_the_reload_fails_the_arm() {
    let fake = gate_then([
        timer_not_found(),
        reply(&["daemon-reload"], exited(0, "", "")),
        timer_not_found(),
    ]);

    let e = on_booted(fake.clone())
        .arm_timer("x", &mut Held::default())
        .expect_err("not loaded");

    assert!(matches!(e, Error::UnitNotLoaded { .. }), "{e:?}");
    assert_eq!(fake.calls().len(), 5, "the gate's two probes, show, reload, show");
}

/// A timer whose `Persistent=` is off (a drop-in) keeps no stamp the manager reads, and `clean` of it
/// fails: it is not asked to.
#[skuld::test]
fn arming_a_timer_that_keeps_no_stamp_does_not_clean() {
    let fake = gate_then([timer_shows("", "inactive"), reply(&["enable"], exited(0, "", ""))]);

    on_booted(fake.clone())
        .arm_timer("x", &mut Held::default())
        .expect("arm");

    let calls: Vec<_> = fake.calls().into_iter().skip(2).collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1], ["enable", "--now", "--", "x.timer"]);
}

/// Decision 15: a run in flight is `Running` in any of its three states, and a job between runs is
/// `Idle` whether its last run succeeded (`inactive`) or failed (`failed`): a failed run is a
/// result, not a state. Anything else systemd says is `Unknown`, and never a state it made up.
#[skuld::test]
fn task_state_table() {
    for (active, state) in [
        ("active", State::Running),
        ("activating", State::Running),
        ("deactivating", State::Running),
        ("inactive", State::Idle),
        ("failed", State::Idle),
        ("reloading", State::Unknown),
        ("maintenance", State::Unknown),
        ("", State::Unknown),
    ] {
        let answer = format!("ActiveState={active}\nMainPID=0\nInvocationID=\nLoadState=loaded\n");
        assert_eq!(
            showing(&answer).task_state("x.service").expect(active),
            TaskJob {
                state,
                pid: None,
                invocation: None
            },
            "{active:?}"
        );
    }
    let answer = "ActiveState=active\nMainPID=42\nInvocationID=0123abcd\nLoadState=loaded\n";
    assert_eq!(
        showing(answer).task_state("x.service").expect("a pid"),
        TaskJob {
            state: State::Running,
            pid: Some(42),
            invocation: Some("0123abcd".to_string())
        }
    );
}

#[skuld::test]
fn a_task_job_systemd_has_not_loaded_is_not_a_state() {
    let answer = "ActiveState=inactive\nMainPID=0\nInvocationID=\nLoadState=not-found\n";
    let e = showing(answer).task_state("x.service").expect_err(answer);
    assert!(matches!(e, Error::UnitNotLoaded { .. }), "{e:?}");
}

/// A read is no request: of a unit systemd has not loaded it sends the one `show` and reloads nothing,
/// so it needs no privilege and no gate.
#[skuld::test]
fn a_read_of_a_unit_systemd_has_not_loaded_sends_nothing_else() {
    let answer = "ActiveState=inactive\nMainPID=0\nUnitFileState=\nInvocationID=\nLoadState=not-found\n";
    type Read = fn(&Systemctl) -> Result<()>;
    let reads: [(&str, Read); 4] = [
        ("status", |s| s.status_from_unit("x.service").map(drop)),
        ("task", |s| s.task_state("x.service").map(drop)),
        ("pid", |s| s.main_pid_of("x.service").map(drop)),
        ("arming", |s| s.timer_arming("x").map(drop)),
    ];
    for (name, read) in reads {
        let fake = Fake::new([reply(&["show"], exited(0, answer, ""))]);
        let e = read(&on_booted(fake.clone())).expect_err(name);
        assert!(matches!(e, Error::UnitNotLoaded { .. }), "{name}: {e:?}");
        assert_eq!(fake.calls().len(), 1, "{name}");
    }
}

/// An install that finds the units up to date reloads once when systemd reads one of them not found, and
/// only then: a unit it has loaded sends no request.
#[skuld::test]
fn an_up_to_date_install_reloads_when_a_unit_reads_not_found() {
    let loaded = || reply(&["show"], exited(0, "LoadState=loaded\n", ""));
    let not_found = || reply(&["show"], exited(0, "LoadState=not-found\n", ""));
    let kinds = [UnitKind::Service, UnitKind::Timer];

    let fake = Fake::new([loaded(), loaded()]);
    on_booted(fake.clone())
        .reload_if_unloaded("x", &kinds, &mut Held::default())
        .expect("both loaded");
    assert_eq!(fake.calls().len(), 2, "two shows, no request");

    let mut entries = vec![loaded(), not_found()];
    entries.extend(gate_passes());
    entries.push(reply(&["daemon-reload"], exited(0, "", "")));
    let fake = Fake::new(entries);
    on_booted(fake.clone())
        .reload_if_unloaded("x", &kinds, &mut Held::default())
        .expect("reloaded");
    let verbs: Vec<_> = fake.calls().iter().map(|call| call[0].clone()).collect();
    assert_eq!(verbs, ["show", "show", "--version", "show", "daemon-reload"]);
}

/// A reload that fails keeps its context: the unit file is on disk and not picked up, and a reload in
/// doubt stays in doubt. The arm and the install that finds the files up to date say the same.
#[skuld::test]
fn a_reload_that_fails_after_a_not_found_keeps_its_context() {
    let not_found = || {
        reply(
            &["show"],
            exited(0, "CanClean=\nActiveState=inactive\nLoadState=not-found\n", ""),
        )
    };

    let e = on_booted(gate_then([not_found(), fail(&["daemon-reload"], lost(false))]))
        .arm_timer("x", &mut Held::default())
        .expect_err("injected");
    assert!(matches!(&e, Error::RequestInDoubt { .. }), "{e:?}");
    let msg = e.report().to_string();
    assert!(msg.contains("the unit file for `x` is on disk"), "{msg}");
    assert!(!msg.contains("wrote"), "{msg}");

    let e = on_booted(gate_then([
        not_found(),
        reply(&["daemon-reload"], exited(1, "", "Access denied\n")),
    ]))
    .arm_timer("x", &mut Held::default())
    .expect_err("refused");
    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert!(e.report().to_string().contains("Access denied"));
}

/// A timer is enabled when its unit file state says so, and active when its `ActiveState` does; the
/// two are read apart: goetia's own `disable --now` can leave it active and not enabled.
#[skuld::test]
fn a_timer_s_activity_and_its_link_are_read_apart() {
    for (active, state, expected) in [
        ("active", "enabled", (true, true)),
        ("active", "disabled", (true, false)),
        ("inactive", "enabled", (false, true)),
        ("inactive", "disabled", (false, false)),
        ("failed", "disabled", (false, false)),
        ("inactive", "static", (false, false)),
        ("inactive", "masked", (false, false)),
        ("inactive", "", (false, false)),
    ] {
        let answer = format!("ActiveState={active}\nUnitFileState={state}\nLoadState=loaded\n");
        let arming = showing(&answer).timer_arming("x").expect(&answer);
        assert_eq!((arming.active, arming.enabled), expected, "{active:?} {state:?}");
    }
}

#[skuld::test]
fn the_timer_is_asked_about_by_its_own_name() {
    let fake = Fake::new([reply(
        &["show"],
        exited(0, "ActiveState=active\nUnitFileState=enabled\nLoadState=loaded\n", ""),
    )]);

    on_booted(fake.clone()).timer_arming("x").expect("timer_arming");

    assert_eq!(
        fake.calls(),
        [[
            "show",
            "--property",
            "ActiveState,UnitFileState,LoadState",
            "--",
            "x.timer"
        ]]
    );
}

// supported ===========================================================================================================

/// The refusal for the version one `source` reported, as [`Systemctl::require_supported`] words it.
fn verdicted(source: Source, reported: &str) -> Result<()> {
    let at_floor = VersionReading::Major(SYSTEMD_FLOOR);
    match source {
        Source::Manager => refusal(at_floor, reading(source, reported)),
        Source::Client => refusal(reading(source, reported), at_floor),
    }
}

/// The `Version` property as distributions set it, measured with `systemctl --version`, whose
/// parenthesised part is the same `GIT_VERSION` string (identical on this host's systemd 257).
#[skuld::test]
fn a_running_systemd_243_or_newer_is_supported() {
    for version in [
        "243",
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
fn a_client_243_or_newer_is_supported() {
    for version in [
        "systemd 243 (243)",
        "systemd 257 (257.13-1~deb13u1)",
        "systemd 258~rc1 (258~rc1-1)",
    ] {
        assert!(verdicted(Source::Client, version).is_ok(), "{version:?}");
    }
}

/// systemd 240 and 241 parse `Type=exec`; only `--show-transaction` is missing, and the refusal
/// must say that and that they lack `clean` of a timer, and not `Type=exec`.
#[skuld::test]
fn systemd_240_and_241_are_refused_for_show_transaction_and_clean() {
    for (source, version, reported) in [
        (Source::Manager, "241.7-1", "241"),
        (Source::Manager, "240", "240"),
        (Source::Client, "systemd 241 (241)", "241"),
    ] {
        let msg = verdicted(source, version).expect_err(version).to_string();
        assert!(msg.contains("requires systemd 243 or newer"), "{msg}");
        assert!(msg.contains(&format!(" {reported}")), "{msg}");
        assert!(msg.contains("--show-transaction"), "{msg}");
        assert!(msg.contains("clean"), "{msg}");
        assert!(!msg.contains("Type=exec"), "{version} parses Type=exec: {msg}");
    }
}

/// systemd 242 has `--show-transaction` and parses `Type=exec`; it has no `clean` of a timer's
/// stamp, and the refusal says that and nothing more.
#[skuld::test]
fn systemd_242_is_refused_for_clean_alone() {
    for (source, version, reported) in [
        (Source::Manager, "242.1-1", "242"),
        (Source::Client, "systemd 242 (242)", "242"),
    ] {
        let msg = verdicted(source, version).expect_err(version).to_string();
        assert!(msg.contains("requires systemd 243 or newer"), "{msg}");
        assert!(msg.contains(&format!(" {reported}")), "{msg}");
        assert!(msg.contains("clean"), "{msg}");
        assert!(!msg.contains("--show-transaction"), "{version} has it: {msg}");
        assert!(!msg.contains("Type=exec"), "{version} parses Type=exec: {msg}");
    }
}

/// Before 240 both reasons apply to the running systemd. The client parses no units, so
/// `--show-transaction` is its only reason.
#[skuld::test]
fn systemd_older_than_240_is_refused_for_both_reasons() {
    let msg = verdicted(Source::Manager, "239-41.el8").unwrap_err().to_string();
    assert!(msg.contains("requires systemd 243 or newer"), "{msg}");
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

// the gate ============================================================================================================

/// What a `Fake` was asked, one name per call: the probes of the gate are `--version` and `show`.
fn asked_of(fake: &Fake) -> Vec<String> {
    fake.calls().into_iter().map(|argv| argv[0].clone()).collect()
}

/// A reservation asks the gate once, whichever verbs of its sequence it serves; another reservation
/// asks again, so nothing is remembered past the invocation that held one.
#[skuld::test]
fn the_gate_is_asked_once_per_reservation() {
    let fake = Fake::new(gate_passes().into_iter().chain(gate_passes()));
    let systemctl = on_booted(fake.clone());

    let mut held = Held::default();
    systemctl.require_supported(&mut held).unwrap();
    systemctl.require_supported(&mut held).unwrap();
    assert_eq!(asked_of(&fake), ["--version", "show"], "two steps, one pair of probes");

    systemctl.require_supported(&mut Held::default()).unwrap();
    assert_eq!(
        asked_of(&fake),
        ["--version", "show", "--version", "show"],
        "another reservation asks again"
    );
}

/// A refusal is not remembered: the next step asks again.
#[skuld::test]
fn a_refused_gate_is_asked_again() {
    let fake = Fake::new(two_versions("241", "257").into_iter().chain(gate_passes()));
    let systemctl = on_booted(fake.clone());
    let mut held = Held::default();

    assert!(systemctl.require_supported(&mut held).is_err());
    systemctl.require_supported(&mut held).unwrap();

    assert_eq!(asked_of(&fake), ["--version", "show", "--version", "show"]);
}

/// A remembered pass never stands in for the evidence [`Systemctl::door`] reads without asking: that
/// is read on every run.
#[skuld::test]
fn a_remembered_pass_does_not_skip_the_evidence() {
    let mut held = gate_held();
    let offline = Systemctl::new(Box::new(Fake::new([])), offline_yes);
    assert_no_manager(
        offline.daemon_reload(&mut held),
        "`SYSTEMD_OFFLINE=yes` is set",
        "daemon-reload",
    );
}

// supported, asked ====================================================================================================

/// The probes' replies: `show` answers with `manager`'s version and `--version` with `client`'s, each
/// `none` to fail.
fn two_versions(manager: &str, client: &str) -> Vec<Entry> {
    let client = match client {
        "none" => exited(99, "", ""),
        client => exited(0, &format!("systemd {client} ({client})\n"), ""),
    };
    let manager = match manager {
        "none" => exited(1, "", ""),
        manager => exited(0, &format!("{manager}\n"), ""),
    };
    vec![
        reply(&["--version"], client),
        reply(&["show", "--property=Version", "--value"], manager),
    ]
}

/// [`Systemctl::supported`], with `probes` the replies on a booted host.
fn supported_by(probes: impl IntoIterator<Item = Entry>) -> Result<()> {
    on_booted(Fake::new(probes)).supported(&mut Held::default())
}

fn refused(manager: &str, client: &str) -> String {
    supported_by(two_versions(manager, client))
        .expect_err(&format!("manager {manager}, client {client}"))
        .report()
        .to_string()
}

/// Both must be 243 or newer: the running systemd parses `Type=exec`, answers the job and cleans a
/// timer, and the client parses `--show-transaction` and has `clean`, so neither rescues the other.
/// The refusal names whichever failed, with its version, and both when both did.
#[skuld::test]
fn both_the_running_systemd_and_the_client_must_be_243_or_newer() {
    assert!(supported_by(two_versions("257", "257")).is_ok());

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
    for value in ["0", "no", "n", "false", "f", "off", "", "2", "maybe"] {
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
/// falls through to the chroot check. The child asks something stricter and simpler — that the
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
        assert_no_manager(
            real_sh(&script, "systemctl").supported(&mut Held::default()),
            notice,
            notice,
        );
    }
}

/// What a `systemctl` goetia ran carries of [`DENIED`], as shell: [`ENVIRONMENT`]'s three names
/// with [`ENVIRONMENT`]'s values, and nothing else under either prefix. Asked of `env`, so it is
/// the child's environment as it arrived rather than a list of the switches a real `systemctl`
/// happens to read — which is the point of removing by prefix.
const ONLY_GOETIAS_OWN_SWITCHES: &str = "[ \"$(env | grep -c '^SYSTEMD_')\" = 3 ] \
                                         && [ \"$(env | grep -c '^SYSTEMCTL_')\" = 0 ] \
                                         && [ \"$SYSTEMD_COLORS\" = 0 ] \
                                         && [ \"$SYSTEMD_LOG_LEVEL\" = info ] \
                                         && [ \"$SYSTEMD_LOG_TARGET\" = console ]";

/// Every inherited name under either of [`DENIED`]'s prefixes is *removed* from a child, including
/// one per prefix whose name this file does not contain and cannot: it is made at run time from
/// this process's pid, so no denylist of literals — the shape [`DENIED`] replaced, twice — can hold
/// it, and only the prefix can select it. `SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET` is a name
/// no systemd reads but this file does spell, so it pins reach beyond the switches systemd has
/// today and nothing more. Nothing outside the prefixes is touched, and [`ENVIRONMENT`] is set
/// rather than removed — `SYSTEMD_IN_CHROOT=0` would assert there is no chroot and `=1` one
/// everywhere, so a value is never asserted for a switch goetia does not own.
#[skuld::test]
fn every_inherited_systemd_switch_is_removed_from_a_child_and_nothing_else_is() {
    let pid = std::process::id();
    let (unlistable, unlistable_client) = (format!("SYSTEMD_H{pid}"), format!("SYSTEMCTL_H{pid}"));
    let inherited = [
        "SYSTEMD_IGNORE_CHROOT",
        "SYSTEMD_IN_CHROOT",
        "SYSTEMD_OFFLINE",
        "SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET",
        unlistable.as_str(),
        "SYSTEMD_COLORS",
        "SYSTEMD_LOG_LEVEL",
        "SYSTEMD_LOG_TARGET",
        "SYSTEMCTL_FORCE_BUS",
        unlistable_client.as_str(),
        "PATH",
        "SYSTEMDNOUNDERSCORE",
        "SYSTEMCTLNOUNDERSCORE",
        "NOT_SYSTEMD_EITHER",
    ];
    assert_eq!(
        removals(inherited.iter().map(|key| key.to_string())),
        [
            "SYSTEMD_IGNORE_CHROOT",
            "SYSTEMD_IN_CHROOT",
            "SYSTEMD_OFFLINE",
            "SYSTEMD_A_SWITCH_NO_SUPPORTED_VERSION_HAS_YET",
            unlistable.as_str(),
            "SYSTEMCTL_FORCE_BUS",
            unlistable_client.as_str(),
        ]
    );
}

/// On a system booted with systemd, a manager that could not be asked is not evidence that none
/// runs: a bus that timed out or refused the connection is a refusal naming what `systemctl` said,
/// and so is a `show` that answered with nothing and did not say it ignored the question.
#[skuld::test]
fn any_other_failure_to_ask_the_manager_is_a_refusal() {
    let version = || reply(&["--version"], exited(0, "systemd 257 (257)\n", ""));
    let failing = reply(
        &["show"],
        exited(1, "", "Failed to get properties: Connection timed out\n"),
    );
    let msg = supported_by([version(), failing]).unwrap_err().report().to_string();
    assert!(
        msg.contains("`systemctl show --property=Version --value` failed"),
        "{msg}"
    );
    assert!(msg.contains("Connection timed out"), "{msg}");
    assert!(msg.contains("cannot tell whether the running systemd is 243+"), "{msg}");

    let silent = reply(&["show"], exited(0, "", ""));
    let msg = supported_by([version(), silent]).unwrap_err().to_string();
    assert!(
        msg.contains("cannot read the version the running systemd reports"),
        "{msg}"
    );
}

/// Both probes switch colour off: the stand-in colours whatever it prints unless it sees
/// `SYSTEMD_COLORS=0`, as a real `systemctl` under an inherited `SYSTEMD_COLORS=1` does.
#[skuld::test]
fn every_probe_switches_colour_off() {
    let colours = "c() { [ \"$SYSTEMD_COLORS\" = 0 ] && echo \"$1\" || printf '\\033[0;1;39m%s\\033[0m\\n' \"$1\"; }; ";
    let script = format!("{colours}[ \"$1\" = show ] && c 257 || c 'systemd 257'");
    assert!(real_sh(&script, "systemctl").supported(&mut Held::default()).is_ok());
}
