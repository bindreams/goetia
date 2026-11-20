use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::tz::{Offset, TimeZone};

use super::*;
use crate::manager::{DaemonStatus, Exit, State, TaskStatus};
use crate::spec::DaemonKind;

fn report(kinds: &[Kind]) -> Report {
    Report {
        daemons: Vec::new(),
        errors: kinds
            .iter()
            .map(|kind| ErrorReport::said(None, *kind, "why".to_string()))
            .collect(),
        undetermined: Vec::new(),
    }
}

/// One entry of the third key. Named, since the name is the only thing an
/// aggregate changes and nothing here depends on it.
fn undetermined_entry() -> UndeterminedReport {
    UndeterminedReport {
        name: Some("opaque".to_string()),
        reason: "why".to_string(),
    }
}

/// Also pins that an empty `undetermined` is still emitted: a key that
/// disappears when empty (a `skip_serializing_if` away) is exactly what a
/// consumer reading `doc["undetermined"]` cannot tolerate.
#[skuld::test]
fn write_emits_one_compact_line_terminated_by_a_newline() {
    let report = Report {
        daemons: vec![DaemonReport {
            id: "frpc".to_string(),
            kind: DaemonKind::Service,
            state: "running",
            enabled: true,
            pid: Some(1234),
            last_result: None,
            last_run: None,
            next_run: None,
        }],
        errors: vec![ErrorReport::said(
            Some("corrupt".to_string()),
            Kind::Unusable,
            "why".to_string(),
        )],
        undetermined: Vec::new(),
    };

    let mut out = Vec::new();
    write(&report, &mut out).expect("a Vec never fails to accept a write");

    let text = String::from_utf8(out).expect("the document is UTF-8");
    assert_eq!(
        text,
        concat!(
            r#"{"daemons":[{"id":"frpc","type":"service","state":"running","enabled":true,"pid":1234,"#,
            r#""last-result":null,"last-run":null,"next-run":null}],"#,
            r#""errors":[{"id":"corrupt","kind":"unusable","message":"why"}],"#,
            r#""undetermined":[]}"#,
            "\n"
        ),
        "compact (no spaces or indentation), one line, newline-terminated"
    );
}

/// The exit code is the precedence-max over `errors[].kind` — not "1 if
/// `errors` is non-empty", which would collapse the partial-answer case
/// (`4`) into the same code as an outright failure.
#[skuld::test]
fn exit_code_is_the_precedence_max_over_error_kinds() {
    // Not a hand-list standing in for completeness: `Kind::code` is an
    // exhaustive match, so a kind added without a code does not compile.
    for (kinds, expected) in [
        (&[][..], ExitCode::Success),
        (&[Kind::Unusable][..], ExitCode::Indeterminate),
        (&[Kind::Undetermined][..], ExitCode::Indeterminate),
        (&[Kind::NotInstalled][..], ExitCode::Error),
        (&[Kind::Foreign][..], ExitCode::Error),
        (&[Kind::Other][..], ExitCode::Error),
        (&[Kind::Unavailable][..], ExitCode::Error),
        (&[Kind::InvalidId][..], ExitCode::Error),
        (&[Kind::Unsupported][..], ExitCode::Usage),
        // 1 outranks 4, in either order.
        (&[Kind::Unusable, Kind::NotInstalled][..], ExitCode::Error),
        (&[Kind::NotInstalled, Kind::Unusable][..], ExitCode::Error),
        // Repeats of the same kind stay that kind's code.
        (&[Kind::Unusable, Kind::Unusable][..], ExitCode::Indeterminate),
    ] {
        assert_eq!(exit_code(&report(kinds)), expected, "for {kinds:?}");
    }
}

/// A `list()`-derived undetermined entry is not an `errors[]` entry, so
/// without folding the third key in there is nothing for the code to come
/// from — and `list` exits `0` on the partial answer it just reported.
#[skuld::test]
fn an_undetermined_entry_alone_yields_four() {
    let mut report = report(&[]);
    report.undetermined.push(undetermined_entry());

    assert_eq!(exit_code(&report), ExitCode::Indeterminate);
}

/// The precedence rule spans both keys: a determinate failure outranks an
/// unanswered question wherever each was carried.
#[skuld::test]
fn an_error_beside_an_undetermined_entry_yields_one() {
    let mut report = report(&[Kind::NotInstalled]);
    report.undetermined.push(undetermined_entry());

    assert_eq!(exit_code(&report), ExitCode::Error);
}

/// The wire spelling is the stable contract, and `Serialize` goes through
/// `as_str`, so pinning `as_str` pins the JSON.
#[skuld::test]
fn every_kind_serializes_to_its_documented_wire_spelling() {
    for (kind, spelling) in [
        (Kind::NotInstalled, "not-installed"),
        (Kind::Foreign, "foreign"),
        (Kind::Unusable, "unusable"),
        (Kind::Undetermined, "undetermined"),
        (Kind::InvalidId, "invalid-id"),
        (Kind::Unavailable, "unavailable"),
        (Kind::Unsupported, "unsupported"),
        (Kind::Other, "other"),
    ] {
        assert_eq!(kind.as_str(), spelling);
        assert_eq!(
            serde_json::to_string(&kind).expect("a Kind serializes infallibly"),
            format!("\"{spelling}\"")
        );
    }
}

/// The defect this kind exists for: `status_error`'s catch-all sent every non-`NotInstalled`,
/// non-`Foreign` error to `unusable`, whose published meaning is "goetia owns the id but cannot
/// report on it" and whose remedy is `uninstall`. A read that never completed establishes no
/// ownership, so it must not borrow that claim — nor the advice attached to it.
#[skuld::test]
fn an_undetermined_error_is_not_reported_as_unusable() {
    let e = Error::Undetermined {
        id: "frpc".to_string(),
        reason: Box::new(Error::os_at(
            "read",
            "/run/systemd/system/frpc.service.d",
            std::io::Error::other("Permission denied"),
        )),
        recovery: "re-run as root".to_string(),
    };

    let report = status_error("frpc", e);

    assert_eq!(report.kind, Kind::Undetermined);
    assert_eq!(
        report.kind.code(),
        ExitCode::Indeterminate,
        "still indeterminate, just not an ownership claim"
    );
    assert!(!report.message.contains("uninstall"), "{}", report.message);
}

/// `status` classifies by what a [`Error::Step`] failed on, so a step over `NotInstalled` is
/// `not-installed`, not `unusable`.
#[skuld::test]
fn a_step_over_not_installed_is_reported_as_not_installed() {
    let e = Error::Step {
        what: "query the live state".to_string(),
        source: Box::new(Error::NotInstalled { id: "frpc".to_string() }),
    };

    let report = status_error("frpc", e);

    assert_eq!(report.kind, Kind::NotInstalled);
    assert_eq!(
        report.message, "query the live state: daemon `frpc` is not installed",
        "the message is the whole chain"
    );
}

// emit ================================================================================================================

/// Refuses every write, the way a broken pipe (`goetia --json daemon list | head -1`) or a full
/// disk does.
struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected write failure",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Accepts every write and fails only on `flush`, the way a buffered writer does when the pipe
/// closes after the last `write` call returned `Ok`. This is the case the flush in [`emit`] exists
/// for: without it a document that never left the buffer would be certified as delivered.
struct FailingFlush;

impl std::io::Write for FailingFlush {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected flush failure",
        ))
    }
}

/// A buffered write that succeeds and then fails to reach the far end is still an undelivered
/// document. Without this the flush in [`emit`] would be justified only by its doc comment.
#[skuld::test]
fn emit_exits_one_when_only_the_flush_fails() {
    let report = report(&[]);
    assert_eq!(
        exit_code(&report),
        ExitCode::Success,
        "the report's own code, before the flush fails"
    );

    let mut err = Vec::new();
    let code = emit(&report, &mut FailingFlush, &mut err);

    assert_eq!(code, ExitCode::Error);
    let text = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(text.contains("injected flush failure"), "{text}");
}

/// The exit code certifies that the document was delivered, so an undelivered one cannot exit `0`.
/// Deliberately built from a report whose own [`exit_code`] *is* `0`: that is the case a consumer
/// following "parse stdout first, then read `errors`" would meet as `json.loads("")`.
#[skuld::test]
fn emit_exits_one_when_the_document_cannot_be_written() {
    let report = report(&[]);
    assert_eq!(
        exit_code(&report),
        ExitCode::Success,
        "the report's own code, before the write fails"
    );

    let mut err = Vec::new();
    let code = emit(&report, &mut FailingWriter, &mut err);

    assert_eq!(code, ExitCode::Error);
    let text = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(text.contains("injected write failure"), "{text}");
}

/// A write failure outranks whatever the report would have said on its own — including `Usage`, which
/// otherwise ranks above everything.
#[skuld::test]
fn a_write_failure_replaces_the_reports_own_code_whatever_it_was() {
    for kinds in [
        &[Kind::Unsupported][..],
        &[Kind::Unusable][..],
        &[Kind::NotInstalled][..],
    ] {
        let report = report(kinds);
        let code = emit(&report, &mut FailingWriter, &mut Vec::new());
        assert_eq!(code, ExitCode::Error, "{:?}", kinds[0]);
    }
}

#[skuld::test]
fn emit_returns_the_reports_own_code_when_the_write_succeeds() {
    let report = report(&[Kind::Unusable]);
    let mut out = Vec::new();
    let mut err = Vec::new();

    let code = emit(&report, &mut out, &mut err);

    assert_eq!(code, exit_code(&report));
    assert_eq!(code, ExitCode::Indeterminate);
    assert!(!out.is_empty(), "the document is on stdout");
    assert!(err.is_empty(), "nothing is said about a write that worked");
}

// The document carries the exact text ---------------------------------------------------------------------------------

/// The exact text for `--json`; the human sinks escape.
#[skuld::test]
fn a_refused_id_is_reported_as_the_id_was_given() {
    let id = "a\nb\u{202e}";
    let e = crate::spec::Id::try_from(id.to_string()).unwrap_err();
    assert_eq!(
        invalid_id(id, e).message,
        "id `a\nb\u{202e}` does not match the required pattern ^[A-Za-z0-9._-]{1,80}$"
    );
}

#[skuld::test]
fn an_errors_message_in_the_document_is_its_unescaped_chain() {
    let step = || Error::Step {
        what: "step\u{202e}".to_string(),
        source: Box::new(Error::os_at("read", "/a\nb", std::io::Error::other("x"))),
    };
    assert_eq!(
        status_error("frpc", step()).message,
        "step\u{202e}: failed to read /a\nb: x"
    );
    assert_eq!(
        unavailable(step()).errors[0].message,
        "step\u{202e}: failed to read /a\nb: x"
    );
}

/// The three places that say `installed but unusable: ` (the document's message, the `error:` line
/// and the `warning:` line) take it from the one reason, so they agree, whatever the reason is.
#[skuld::test]
fn every_rendering_of_an_unusable_entry_has_the_same_prefix_and_reason() {
    use crate::cli::support::{partition_installed, print_unusable_warnings};
    use crate::error::error_tests::os;
    use crate::manager::Installed;

    let reasons = [
        Reason::said("damaged"),
        Reason::failed(os("read", "/p", "denied")),
        Reason::around("scan (", os("read", "/p", "denied"), ")."),
    ];
    for reason in reasons {
        let index = partition_installed(vec![Installed::OursUnusable {
            name: "x".to_string(),
            reason: reason.clone(),
        }]);
        let report = from_index(&index, &TimeZone::UTC);
        let message = format!("installed but unusable: {}", reason.unescaped());
        assert_eq!(report.errors[0].message, message);

        let (mut err, mut warn) = (Vec::new(), Vec::new());
        print_errors(&report, &mut err);
        print_unusable_warnings(&index.unusable, &mut warn);
        assert_eq!(String::from_utf8(err).unwrap(), format!("error: x: {message}\n"));
        assert_eq!(String::from_utf8(warn).unwrap(), format!("warning: x: {message}\n"));
    }
}

// Daemons =============================================================================================================

/// 2026-01-15T12:00:00Z.
const NOON: u64 = 1_768_478_400;

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn zone(offset_seconds: i32) -> TimeZone {
    Offset::from_seconds(offset_seconds)
        .expect("an offset in range")
        .to_time_zone()
}

fn common(state: State, pid: Option<u32>, enabled: bool) -> DaemonStatus {
    DaemonStatus { state, pid, enabled }
}

fn task_status(
    common: DaemonStatus,
    last_result: Option<Exit>,
    last_run: Option<SystemTime>,
    next_run: Option<SystemTime>,
) -> Status {
    Status::Task(TaskStatus {
        common,
        last_result,
        last_run,
        next_run,
    })
}

/// The document of one daemon, as `status` and `list` write it.
fn document(status: &Status) -> serde_json::Value {
    serde_json::to_value(daemon("job", status, &TimeZone::UTC)).expect("a DaemonReport serializes infallibly")
}

#[skuld::test]
fn a_service_entry_carries_type_service_and_null_task_fields() {
    let status = Status::Service(common(State::Running, Some(1234), true));

    let json = serde_json::to_string(&daemon("frpc", &status, &TimeZone::UTC)).unwrap();

    assert_eq!(
        json,
        concat!(
            r#"{"id":"frpc","type":"service","state":"running","enabled":true,"pid":1234,"#,
            r#""last-result":null,"last-run":null,"next-run":null}"#
        )
    );
}

#[skuld::test]
fn a_task_that_never_ran_is_idle_with_null_results() {
    let status = task_status(common(State::Idle, None, false), None, None, None);

    let json = serde_json::to_string(&daemon("job", &status, &TimeZone::UTC)).unwrap();

    assert_eq!(
        json,
        concat!(
            r#"{"id":"job","type":"task","state":"idle","enabled":false,"pid":null,"#,
            r#""last-result":null,"last-run":null,"next-run":null}"#
        )
    );
}

#[skuld::test]
fn a_finished_task_run_shows_code_signal_and_times() {
    for (exit, expected) in [
        (Exit::Code(3), serde_json::json!({"code": 3})),
        (Exit::Signal(15), serde_json::json!({"signal": 15})),
        // A Windows `stop`: `TERMINATED`, `0xC000013A`, as the signed 32-bit value.
        (Exit::Code(-1_073_741_510), serde_json::json!({"code": -1_073_741_510})),
    ] {
        let status = task_status(common(State::Idle, None, true), Some(exit), None, None);
        assert_eq!(document(&status)["last-result"], expected, "for {exit}");
    }

    // A time is RFC 3339 with the zone's offset, to the second: the fraction is dropped.
    let started = at(NOON) + Duration::from_millis(999);
    let status = task_status(
        common(State::Idle, None, true),
        Some(Exit::Code(0)),
        Some(started),
        Some(at(NOON + 3600)),
    );
    for (offset, last_run, next_run) in [
        (-5 * 3600, "2026-01-15T07:00:00-05:00", "2026-01-15T08:00:00-05:00"),
        (19_800, "2026-01-15T17:30:00+05:30", "2026-01-15T18:30:00+05:30"),
        (0, "2026-01-15T12:00:00+00:00", "2026-01-15T13:00:00+00:00"),
    ] {
        let json = serde_json::to_value(daemon("job", &status, &zone(offset))).unwrap();
        assert_eq!(json["last-run"], last_run);
        assert_eq!(json["next-run"], next_run);
    }
}

/// One zone for a whole document: every entry `from_index` writes carries the zone it was given.
#[skuld::test]
fn every_entry_of_a_listing_is_written_in_the_one_zone_given() {
    use crate::cli::support::partition_installed;
    use crate::manager::Installed;
    use crate::spec::test_support::program;
    use crate::testing::task;

    let entry = |id: &str| Installed::Ours {
        spec: task(id, vec![program("job")], None),
        status: task_status(
            common(State::Idle, None, true),
            None,
            Some(at(NOON)),
            Some(at(NOON + 3600)),
        ),
    };
    let index = partition_installed(vec![entry("a"), entry("b")]);

    let report = from_index(&index, &zone(19_800));

    let runs: Vec<_> = report.daemons.iter().map(|d| d.last_run.as_deref()).collect();
    assert_eq!(runs, [Some("2026-01-15T17:30:00+05:30"); 2]);
    let runs: Vec<_> = report.daemons.iter().map(|d| d.next_run.as_deref()).collect();
    assert_eq!(runs, [Some("2026-01-15T18:30:00+05:30"); 2]);
}

/// `time` as `SystemTime`, or `None` where the platform's `SystemTime` cannot hold it (Windows before 1601).
fn system_time(time: Timestamp) -> Option<SystemTime> {
    let since_epoch = time.as_duration();
    if since_epoch.is_negative() {
        UNIX_EPOCH.checked_sub(since_epoch.unsigned_abs())
    } else {
        UNIX_EPOCH.checked_add(since_epoch.unsigned_abs())
    }
}

/// The `last-run` document of an instant seen in `zone`.
fn last_run(time: SystemTime, zone: &TimeZone) -> serde_json::Value {
    let status = task_status(common(State::Idle, None, true), None, Some(time), None);
    serde_json::to_value(daemon("job", &status, zone)).unwrap()["last-run"].clone()
}

/// RFC 3339 has years 0000 to 9999: an instant whose civil year in the display offset is outside them is `null`,
/// as is one outside `Timestamp`'s own range, where `jiff` would print a negative year in an expanded form.
#[skuld::test]
fn a_time_is_null_outside_rfc_3339s_years_and_jiffs_range() {
    let utc = &TimeZone::UTC;
    let null = serde_json::Value::Null;

    // The last day: 9999-12-30, and `Timestamp::MAX`, both in range.
    let last_day: Timestamp = "9999-12-30T00:00:00Z".parse().unwrap();
    assert_eq!(
        last_run(system_time(last_day).expect("year 9999 fits"), utc),
        "9999-12-30T00:00:00+00:00"
    );
    let max = system_time(Timestamp::MAX).expect("year 9999 fits");
    let shown = last_run(max, utc);
    assert!(shown.as_str().unwrap().starts_with("9999-12-30T"), "{shown}");
    // One second past `Timestamp::MAX` is outside its range, for `next-run` as for `last-run`.
    assert_eq!(last_run(max + Duration::from_secs(1), utc), null);
    let status = task_status(
        common(State::Idle, None, true),
        None,
        None,
        Some(max + Duration::from_secs(1)),
    );
    assert_eq!(document(&status)["next-run"], null);

    // Negative years and `Timestamp::MIN` cannot be held by Windows' `SystemTime`.
    let year = |year: i16, month, day, hour| {
        jiff::civil::date(year, month, day)
            .at(hour, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    };
    let cases = [
        // The first instant of year 0 is written; the second before it is year -1.
        (year(0, 1, 1, 0), utc, Some("0000-01-01T00:00:00+00:00")),
        (year(0, 1, 1, 0) - jiff::SignedDuration::from_secs(1), utc, None),
        // The year is the display offset's: year 0 in UTC is year -1 at -01:00, and year -1 in UTC is year 0 at +01:00.
        (year(0, 1, 1, 0), &zone(-3600), None),
        (year(-1, 12, 31, 23), &zone(3600), Some("0000-01-01T00:00:00+01:00")),
        (Timestamp::MIN, utc, None),
    ];
    for (instant, zone, expected) in cases {
        match system_time(instant) {
            Some(time) => assert_eq!(
                last_run(time, zone),
                expected.map_or(null.clone(), serde_json::Value::from),
                "{instant}"
            ),
            None if cfg!(windows) => {} // Windows' `SystemTime` starts in 1601.
            None => panic!("only Windows' SystemTime lacks {instant}"),
        }
    }
    if let Some(min) = system_time(Timestamp::MIN) {
        assert_eq!(last_run(min - Duration::from_secs(1), utc), null);
    }
}

/// jiff prints the offset to the minute but would compute the civil time from the exact offset: the printed
/// time must name the instant it was given.
#[skuld::test]
fn an_offset_with_seconds_is_written_to_the_minute_and_names_the_same_instant() {
    // Africa/Monrovia until 1972: -00:44:30.
    let time = at(0);
    for (offset, expected) in [
        (-2670, "1969-12-31T23:16:00-00:44"),
        (2670, "1970-01-01T00:44:00+00:44"),
        (-2669, "1969-12-31T23:16:00-00:44"),
    ] {
        let shown = last_run(time, &zone(offset));
        assert_eq!(shown, expected, "offset {offset} s");
        let parsed: Timestamp = shown.as_str().unwrap().parse().unwrap();
        assert_eq!(parsed.as_second(), 0, "{shown} names the instant given");
    }
}

/// The unknown zone, which `jiff` gives where it cannot determine the system zone, writes UTC with `Z`.
#[skuld::test]
fn the_unknown_zone_writes_utc_with_z() {
    assert_eq!(last_run(at(NOON), &TimeZone::unknown()), "2026-01-15T12:00:00Z");
}

/// A zone with daylight saving writes each time with the offset in force at that time.
#[skuld::test]
fn each_time_carries_the_offset_in_force_at_that_instant() {
    let new_york = TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").expect("a valid POSIX zone");
    let summer: Timestamp = "2026-07-15T12:00:00Z".parse().unwrap();
    let status = task_status(
        common(State::Idle, None, true),
        None,
        Some(at(NOON)),
        Some(system_time(summer).expect("2026 fits")),
    );

    let json = serde_json::to_value(daemon("job", &status, &new_york)).unwrap();

    assert_eq!(json["last-run"], "2026-01-15T07:00:00-05:00");
    assert_eq!(json["next-run"], "2026-07-15T08:00:00-04:00");
}

/// The fraction of a second is dropped toward the earlier second, before the epoch as after it.
#[skuld::test]
fn a_time_before_the_epoch_drops_its_fraction_toward_the_earlier_second() {
    for (before_epoch, expected) in [
        (Duration::from_millis(1500), "1969-12-31T23:59:58+00:00"),
        (Duration::from_millis(1), "1969-12-31T23:59:59+00:00"),
        (Duration::from_secs(2), "1969-12-31T23:59:58+00:00"),
    ] {
        let time = UNIX_EPOCH.checked_sub(before_epoch).expect("a time before the epoch");
        let status = task_status(common(State::Idle, None, true), None, Some(time), None);
        assert_eq!(
            document(&status)["last-run"],
            expected,
            "{before_epoch:?} before the epoch"
        );
    }
}

/// systemd: the journal's newest run has no terminal entry while the run is in flight, so `last-result` is
/// null, and `last-run` is that run's start.
#[skuld::test]
fn a_systemd_shaped_run_in_flight_has_last_run_and_null_last_result() {
    let status = task_status(
        common(State::Running, Some(77), true),
        None,
        Some(at(NOON)),
        Some(at(NOON + 60)),
    );

    let json = document(&status);

    assert_eq!(json["state"], "running");
    assert_eq!(json["pid"], 77);
    assert_eq!(json["last-result"], serde_json::Value::Null);
    assert_eq!(json["last-run"], "2026-01-15T12:00:00+00:00");
}

/// systemd: a run the manager failed before the program exited (a resource or timeout failure) leaves no
/// result and a start; the task is idle again.
#[skuld::test]
fn a_systemd_run_the_manager_failed_has_last_run_and_null_last_result() {
    let status = task_status(
        common(State::Idle, None, true),
        None,
        Some(at(NOON)),
        Some(at(NOON + 60)),
    );

    let json = document(&status);

    assert_eq!(json["state"], "idle");
    assert_eq!(json["last-result"], serde_json::Value::Null);
    assert_eq!(json["last-run"], "2026-01-15T12:00:00+00:00");
}

/// launchd: while a run is in flight `last-result` is still the previous run's, and `last-run` is never known.
#[skuld::test]
fn a_launchd_shaped_run_in_flight_shows_the_previous_runs_result_and_null_last_run() {
    let status = task_status(
        common(State::Running, Some(77), true),
        Some(Exit::Code(7)),
        None,
        Some(at(NOON + 60)),
    );

    let json = document(&status);

    assert_eq!(json["state"], "running");
    assert_eq!(json["last-result"], serde_json::json!({"code": 7}));
    assert_eq!(json["last-run"], serde_json::Value::Null);
}

/// A null `last-run` beside a `next-run` and a result is launchd's shape, not a fault.
#[skuld::test]
fn a_launchd_shaped_task_has_null_last_run_and_a_next_run() {
    let status = task_status(
        common(State::Idle, None, true),
        Some(Exit::Code(0)),
        None,
        Some(at(NOON + 60)),
    );

    let json = document(&status);

    assert_eq!(json["last-run"], serde_json::Value::Null);
    assert_eq!(json["next-run"], "2026-01-15T12:01:00+00:00");
    assert_eq!(json["last-result"], serde_json::json!({"code": 0}));
}

/// Windows: `LastTaskResult` reads running while a run is in flight, which is no result; `last-run` is the
/// run's start.
#[skuld::test]
fn a_windows_shaped_run_in_flight_has_null_last_result_and_a_last_run() {
    let status = task_status(
        common(State::Running, Some(4040), true),
        None,
        Some(at(NOON)),
        Some(at(NOON + 60)),
    );

    let json = document(&status);

    assert_eq!(json["state"], "running");
    assert_eq!(json["last-result"], serde_json::Value::Null);
    assert_eq!(json["last-run"], "2026-01-15T12:00:00+00:00");
}
