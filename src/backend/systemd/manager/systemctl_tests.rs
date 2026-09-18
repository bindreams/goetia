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
            "/bin/sh",
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
/// failure, which says it was not. Injected, so nothing runs.
#[skuld::test]
fn a_watched_request_that_may_have_run_is_in_doubt() {
    let spawn = || {
        run_verb_via(
            "/bin/true",
            &[],
            Budget::Bounded(Duration::from_secs(10)),
            Budget::Unbounded.start(),
        )
    };
    bounded::test_hook::spawn_fails(|| cosca::error::Error::Containment {
        detail: "forced".into(),
    });
    let e = spawn().expect_err("injected");
    assert!(matches!(e, Error::RequestInDoubt { .. }), "{e:?}");
    assert!(
        e.to_string()
            .contains("may or may not have reached the service manager"),
        "{e}"
    );

    bounded::test_hook::spawn_fails(|| std::io::Error::from_raw_os_error(libc::EAGAIN).into());
    let e = spawn().expect_err("injected");
    assert!(matches!(e, Error::Other(_)), "{e:?}");

    // Past the spawn `systemctl` runs, so a wait that fails leaves its request in doubt too.
    bounded::test_hook::wait_fails(|| std::io::Error::other("the wait failed").into());
    let e = spawn().expect_err("injected");
    assert!(matches!(e, Error::RequestInDoubt { .. }), "{e:?}");
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

/// A watched `systemctl` needs no temp file: it runs where none may be writable — a chroot, or an
/// image build.
#[skuld::test]
fn a_watched_request_needs_no_temp_file() {
    let _no_file = bounded::test_hook::temp_files(0);
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request");
    let finished = run_verb_via(
        "/bin/sh",
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

/// `SYSTEMD_LOG_TIME`, `SYSTEMD_LOG_LOCATION` and `SYSTEMD_LOG_TID` each prefix every protocol line,
/// and goetia's environment switches none of them off. The lines below are what `systemctl start
/// --show-transaction` wrote under each on systemd 257.
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

// supported ===========================================================================================================

/// The refusal for the version one `source` reported, as [`require_supported`] words it.
fn supported(source: Source, reported: &str) -> Result<()> {
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
        assert!(supported(Source::Manager, version).is_ok(), "{version:?}");
    }
}

#[skuld::test]
fn a_client_242_or_newer_is_supported() {
    for version in [
        "systemd 242 (242)",
        "systemd 257 (257.13-1~deb13u1)",
        "systemd 258~rc1 (258~rc1-1)",
    ] {
        for source in [Source::Client, Source::OfflineClient(NoManager::Offline)] {
            assert!(supported(source, version).is_ok(), "{source:?} {version:?}");
        }
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
        (Source::OfflineClient(NoManager::Chroot), "systemd 241 (241)", "241"),
    ] {
        let msg = supported(source, version).expect_err(version).to_string();
        assert!(msg.contains("requires systemd 242 or newer"), "{msg}");
        assert!(msg.contains(&format!(" {reported}")), "{msg}");
        assert!(msg.contains("--show-transaction"), "{msg}");
        assert!(!msg.contains("Type=exec"), "{version} parses Type=exec: {msg}");
    }
}

/// Before 240 both reasons apply — to the running systemd, and to a client standing in for it
/// offline. A client next to a running systemd parses no units, so `--show-transaction` is its only
/// reason.
#[skuld::test]
fn systemd_older_than_240_is_refused_for_both_reasons() {
    for (source, version) in [
        (Source::Manager, "239-41.el8"),
        (Source::OfflineClient(NoManager::NotBooted), "systemd 237"),
    ] {
        let msg = supported(source, version).expect_err(version).to_string();
        assert!(msg.contains("requires systemd 242 or newer"), "{msg}");
        assert!(msg.contains("--show-transaction"), "{msg}");
        assert!(msg.contains("Type=simple"), "{msg}");
    }
    let msg = supported(Source::Client, "systemd 237").unwrap_err().to_string();
    assert!(msg.contains("--show-transaction"), "{msg}");
    assert!(!msg.contains("Type=simple"), "{msg}");
}

/// The refusal names whose version it read.
#[skuld::test]
fn a_refusal_names_whose_version_it_read() {
    let manager = supported(Source::Manager, "241").unwrap_err().to_string();
    assert!(manager.contains("The running systemd is 241"), "{manager}");
    let client = supported(Source::Client, "systemd 241 (241)").unwrap_err().to_string();
    assert!(client.contains("The `systemctl` client is 241"), "{client}");
    let offline = supported(Source::OfflineClient(NoManager::Offline), "systemd 241 (241)")
        .unwrap_err()
        .to_string();
    assert!(
        offline.contains("No systemd runs here (`SYSTEMD_OFFLINE` is set)"),
        "{offline}"
    );
    assert!(offline.contains("`systemctl --version` reports 241"), "{offline}");
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
        (
            Source::OfflineClient(NoManager::Offline),
            "systemd 257\u{1b}[0m (257.13-1~deb13u1)",
        ),
    ] {
        let msg = supported(source, version).expect_err(version).to_string();
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

// require_supported_via ===============================================================================================

/// A stand-in for `systemctl`: `sh -c <script> systemctl <args>`, so the probe's own arguments are
/// the script's `$@`. Never a file on disk — writing one and `exec`ing it races every `fork` the
/// test process makes on another thread (`ETXTBSY`).
fn stand_in(script: &str) -> [&str; 4] {
    ["/bin/sh", "-c", script, "systemctl"]
}

/// A system booted with systemd, and not offline: its manager must be asked.
fn booted() -> Host {
    Host {
        offline: None,
        unbooted: false,
    }
}

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

fn refused(manager: &str, client: &str) -> String {
    require_supported_via(&stand_in(&two_versions(manager, client)), &booted())
        .expect_err(&format!("manager {manager}, client {client}"))
        .to_string()
}

/// Both must be 242 or newer: the running systemd parses `Type=exec` and answers the job, and the
/// client parses `--show-transaction`, so neither rescues the other. The refusal names whichever
/// failed, with its version, and both when both did.
#[skuld::test]
fn both_the_running_systemd_and_the_client_must_be_242_or_newer() {
    assert!(require_supported_via(&stand_in(&two_versions("257", "257")), &booted()).is_ok());

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

/// Evidence found without asking `systemctl`: `SYSTEMD_OFFLINE` set true, or no
/// `/run/systemd/system`. The client alone decides, and the manager is never asked — the stand-in
/// fails its `show`.
#[skuld::test]
fn where_no_manager_runs_the_client_alone_decides() {
    let hosts = [
        (
            Host {
                offline: Some("1".to_string()),
                unbooted: false,
            },
            "`SYSTEMD_OFFLINE` is set",
        ),
        (
            Host {
                offline: None,
                unbooted: true,
            },
            "`/run/systemd/system` does not exist",
        ),
    ];
    for (host, why) in hosts {
        assert!(
            require_supported_via(&stand_in(&two_versions("none", "257")), &host).is_ok(),
            "{why}"
        );
        let msg = require_supported_via(&stand_in(&two_versions("none", "241")), &host)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains(&format!(
                "No systemd runs here ({why}), and `systemctl --version` reports 241"
            )),
            "{msg}"
        );
    }
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

/// This host is booted with systemd and not offline: its manager is asked.
#[skuld::test]
fn this_host_has_a_manager_to_ask() {
    assert_eq!(Host::this().no_manager(), None);
}

/// `SYSTEMD_OFFLINE` is read as `systemctl` reads it, with systemd's `parse_boolean`: only a true
/// value is evidence, and anything else leaves the manager to be asked.
#[skuld::test]
fn systemd_offline_is_evidence_only_when_true() {
    for value in ["1", "yes", "Y", "TRUE", "t", "on"] {
        let host = Host {
            offline: Some(value.to_string()),
            unbooted: false,
        };
        assert_eq!(host.no_manager(), Some(NoManager::Offline), "{value:?}");
    }
    for value in ["0", "no", "false", "off", "", "2", "maybe"] {
        let host = Host {
            offline: Some(value.to_string()),
            unbooted: false,
        };
        assert_eq!(host.no_manager(), None, "{value:?}");
    }
}

/// The chroot `systemctl` itself detects — `/proc/1/root` against `/`, or `SYSTEMD_IN_CHROOT` — as
/// it reports it: `show` exits `0`, prints nothing, and says why on stderr (measured on systemd
/// 257; older ones say "ignoring request."). The stand-in says so only when its log level is audible,
/// as a real one under an inherited `SYSTEMD_LOG_LEVEL=warning` would not.
#[skuld::test]
fn a_chroot_systemctl_reports_is_evidence_that_cannot_be_silenced() {
    for notice in [
        "Running in chroot, ignoring command 'show'",
        "Running in chroot, ignoring request.",
    ] {
        let in_chroot = |client: &str| {
            format!(
                "[ \"$1\" = show ] && {{ [ \"$SYSTEMD_LOG_LEVEL\" = info ] && echo \"{notice}\" >&2; exit 0; }}; \
                 echo 'systemd {client} ({client})'"
            )
        };
        assert!(
            require_supported_via(&stand_in(&in_chroot("257")), &booted()).is_ok(),
            "{notice}"
        );
        let msg = require_supported_via(&stand_in(&in_chroot("241")), &booted())
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("No systemd runs here (`systemctl` says it is running in a chroot)"),
            "{msg}"
        );
    }
}

/// On a system booted with systemd, a manager that could not be asked is not evidence that none
/// runs: a bus that timed out or refused the connection is a refusal naming what `systemctl` said,
/// and so is a `show` that answered with nothing and no chroot to explain it.
#[skuld::test]
fn any_other_failure_to_ask_the_manager_is_a_refusal() {
    let failing = "[ \"$1\" = show ] && { echo 'Failed to get properties: Connection timed out' >&2; exit 1; }; \
                   echo 'systemd 257 (257)'";
    let msg = require_supported_via(&stand_in(failing), &booted())
        .unwrap_err()
        .to_string();
    assert!(msg.contains("`systemctl show --property=Version` failed"), "{msg}");
    assert!(msg.contains("Connection timed out"), "{msg}");
    assert!(msg.contains("cannot tell whether the running systemd is 242+"), "{msg}");

    let silent = "[ \"$1\" = show ] && exit 0; echo 'systemd 257 (257)'";
    let msg = require_supported_via(&stand_in(silent), &booted())
        .unwrap_err()
        .to_string();
    assert!(msg.contains("printed no version"), "{msg}");
}

/// Both probes switch colour off: the stand-in colours whatever it prints unless it sees
/// `SYSTEMD_COLORS=0`, as a real `systemctl` under an inherited `SYSTEMD_COLORS=1` does.
#[skuld::test]
fn every_probe_switches_colour_off() {
    let colours = "c() { [ \"$SYSTEMD_COLORS\" = 0 ] && echo \"$1\" || printf '\\033[0;1;39m%s\\033[0m\\n' \"$1\"; }; ";
    let online = format!("{colours}[ \"$1\" = show ] && c 257 || c 'systemd 257'");
    assert!(require_supported_via(&stand_in(&online), &booted()).is_ok());
    let offline = format!("{colours}[ \"$1\" = show ] && exit 1; c 'systemd 257'");
    let unbooted = Host {
        offline: None,
        unbooted: true,
    };
    assert!(require_supported_via(&stand_in(&offline), &unbooted).is_ok());
}

/// The real `systemctl` on this host, first with an inherited `SYSTEMD_COLORS=1` — set by the
/// wrapper only where goetia did not set it itself — then as it is.
#[skuld::test]
fn this_hosts_systemd_is_supported() {
    require_supported_via(
        &stand_in("SYSTEMD_COLORS=${SYSTEMD_COLORS:-1} exec systemctl \"$@\""),
        &Host::this(),
    )
    .expect("the host running the tests runs systemd 242+, whatever colour is inherited");
    require_supported().expect("the host running the tests runs systemd 242+");
}
