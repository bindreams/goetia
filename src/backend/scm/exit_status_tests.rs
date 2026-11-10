use super::*;

#[skuld::test]
fn clean_exit_is_win32_zero_and_process_zero() {
    let reported = Reported::from_daemon_code(0);

    assert_eq!(reported, Reported::Clean);
    assert_eq!(reported.scm_codes(), (0, 0));
    assert_eq!(reported.process_exit_code(), 0);
}

#[skuld::test]
fn a_daemon_failure_is_service_specific_and_exits_with_the_same_code() {
    let reported = Reported::from_daemon_code(7);

    assert_eq!(reported, Reported::DaemonExit(7));
    assert_eq!(reported.scm_codes(), (ERROR_SERVICE_SPECIFIC_ERROR, 7));
    assert_eq!(reported.process_exit_code(), 7);
}

#[skuld::test]
fn daemon_code_3_and_a_shim_failure_are_distinct_reports() {
    let daemon = Reported::from_daemon_code(3);
    let failure = Reported::shim_failure(Some(3));

    assert_eq!(daemon.scm_codes(), (ERROR_SERVICE_SPECIFIC_ERROR, 3));
    assert_eq!(failure.scm_codes(), (3, 0));
    assert_ne!(daemon, failure);
}

#[skuld::test]
fn an_nt_status_daemon_code_round_trips_as_u32() {
    let reported = Reported::from_daemon_code(-1073741819);

    assert_eq!(reported.scm_codes(), (ERROR_SERVICE_SPECIFIC_ERROR, 0xC000_0005));
    assert_eq!(reported.process_exit_code(), -1073741819);
}

#[skuld::test]
fn shim_failure_with_no_os_error_is_31() {
    let reported = Reported::shim_failure(None);

    assert_eq!(reported.scm_codes(), (ERROR_GEN_FAILURE, 0));
    assert_eq!(reported.process_exit_code(), ERROR_GEN_FAILURE as i32);
}

#[skuld::test]
fn shim_failure_keeps_the_os_error_it_is_given() {
    let reported = Reported::shim_failure(Some(2));

    assert_eq!(reported.scm_codes(), (2, 0));
    assert_eq!(reported.process_exit_code(), 2);
}

#[skuld::test]
fn shim_failure_never_holds_0_or_1066() {
    for os_error in [0, ERROR_SERVICE_SPECIFIC_ERROR as i32] {
        let reported = Reported::shim_failure(Some(os_error));

        assert_eq!(
            reported,
            Reported::shim_failure(None),
            "os error {os_error} would read as a clean exit or as a daemon's code"
        );
        assert_eq!(reported.scm_codes(), (ERROR_GEN_FAILURE, 0));
    }
}

#[skuld::test]
fn a_zero_daemon_code_is_clean() {
    assert_eq!(Reported::from_daemon_code(0), Reported::Clean);
    assert_ne!(Reported::from_daemon_code(1), Reported::Clean);
}

#[skuld::test]
fn an_incomplete_stop_reports_win32_zero_and_the_specific_code_1003() {
    // SCM restarts after a commanded stop that reports a non-zero Win32 code, and an incomplete stop
    // leaves a daemon that may still be alive. A non-zero service-specific code beside Win32 0 is
    // stored as reported and never queues recovery, so it is the visible record that costs no restart.
    let reported = Reported::incomplete_stop();

    assert_eq!(reported.scm_codes(), (0, ERROR_CAN_NOT_COMPLETE));
    assert_eq!(reported.process_exit_code(), ERROR_CAN_NOT_COMPLETE as i32);
}

#[skuld::test]
fn a_refusal_reports_win32_zero_and_its_own_specific_code() {
    let reported = Reported::Refused;

    assert_eq!(reported.scm_codes(), (0, SHIM_REFUSED));
    assert_eq!(reported.process_exit_code(), SHIM_REFUSED as i32);
    assert_ne!(reported.scm_codes(), Reported::incomplete_stop().scm_codes());
}

// What SCM recorded ---------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_recorded_daemon_exit_means_the_daemon_was_launched() {
    for reported in [
        Reported::Clean,
        Reported::from_daemon_code(7),
        Reported::from_daemon_code(-1073741819),
        Reported::incomplete_stop(),
    ] {
        let (win32, specific) = reported.scm_codes();

        assert!(RecordedExit { win32, specific }.daemon_ran(), "{reported:?}");
    }
}

#[skuld::test]
fn only_the_incomplete_stop_report_reads_back_as_an_incomplete_stop() {
    let record = |reported: Reported| {
        let (win32, specific) = reported.scm_codes();
        RecordedExit { win32, specific }
    };

    assert!(record(Reported::incomplete_stop()).is_incomplete_stop());
    for reported in [
        Reported::Clean,
        Reported::from_daemon_code(ERROR_CAN_NOT_COMPLETE as i32),
        Reported::shim_failure(Some(ERROR_CAN_NOT_COMPLETE as i32)),
        Reported::Refused,
    ] {
        assert!(!record(reported).is_incomplete_stop(), "{reported:?}");
    }
}

#[skuld::test]
fn only_the_refusal_report_reads_back_as_a_refusal() {
    let record = |reported: Reported| {
        let (win32, specific) = reported.scm_codes();
        RecordedExit { win32, specific }
    };

    assert!(record(Reported::Refused).is_refusal());
    for reported in [
        Reported::Clean,
        Reported::incomplete_stop(),
        Reported::from_daemon_code(SHIM_REFUSED as i32),
        Reported::shim_failure(Some(SHIM_REFUSED as i32)),
    ] {
        assert!(!record(reported).is_refusal(), "{reported:?}");
    }
}

/// The shim refused before it launched anything, though the record is Win32 0.
#[skuld::test]
fn a_recorded_refusal_means_the_daemon_never_ran() {
    let (win32, specific) = Reported::Refused.scm_codes();

    assert!(!RecordedExit { win32, specific }.daemon_ran());
}

#[skuld::test]
fn a_recorded_shim_failure_means_the_daemon_never_ran() {
    for reported in [
        Reported::shim_failure(None),
        Reported::shim_failure(Some(2)),
        Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)),
    ] {
        let (win32, specific) = reported.scm_codes();

        assert!(!RecordedExit { win32, specific }.daemon_ran(), "{reported:?}");
    }
}

#[skuld::test]
fn a_service_that_died_without_reporting_did_not_run_a_daemon() {
    // SCM records ERROR_PROCESS_ABORTED (1067) when the host process exits without reporting STOPPED.
    assert!(
        !RecordedExit {
            win32: 1067,
            specific: 0
        }
        .daemon_ran()
    );
}

#[skuld::test]
fn a_recorded_exit_is_displayed_as_a_service_specific_code_or_a_win32_one() {
    let specific = RecordedExit {
        win32: ERROR_SERVICE_SPECIFIC_ERROR,
        specific: 7,
    };
    let win32 = RecordedExit { win32: 2, specific: 0 };

    assert_eq!(specific.to_string(), "service-specific exit code 7");
    assert_eq!(win32.to_string(), "exit code 2");
}

/// Win32 0 beside a service-specific code is the shim's own record, not an exit code 0.
#[skuld::test]
fn a_win32_zero_record_with_a_specific_code_names_both() {
    let refused = RecordedExit {
        win32: 0,
        specific: SHIM_REFUSED,
    };

    assert_eq!(
        refused.to_string(),
        format!("exit code 0 with service-specific code {SHIM_REFUSED}")
    );
}

// The process exit code of a recorded exit ----------------------------------------------------------------------------

/// One of each [`Reported`] shape. A new variant stops the match below from compiling, which is the
/// prompt to add a sample to the list; nothing forces the sample itself.
fn every_reported() -> Vec<Reported> {
    fn exhaustive(reported: Reported) {
        match reported {
            Reported::Clean
            | Reported::DaemonExit(_)
            | Reported::ShimFailure(_)
            | Reported::IncompleteStop
            | Reported::Refused => {}
        }
    }
    let every = vec![
        Reported::Clean,
        Reported::from_daemon_code(7),
        Reported::from_daemon_code(-1073741819),
        Reported::shim_failure(None),
        Reported::shim_failure(Some(2)),
        Reported::incomplete_stop(),
        Reported::Refused,
    ];
    every.iter().copied().for_each(exhaustive);
    every
}

/// What SCM recorded for a report names that report again, so a `stop` that ends the shim itself ends
/// it with the code the shim exits with.
#[skuld::test]
fn a_recorded_exit_maps_back_to_the_report_that_produced_it() {
    for reported in every_reported() {
        let (win32, specific) = reported.scm_codes();
        let record = RecordedExit { win32, specific };

        assert_eq!(Reported::from_record(record), Some(reported), "{reported:?}");
        assert_eq!(record.process_exit_code(), reported.process_exit_code(), "{reported:?}");
    }
}

/// A pair no `Reported` produces (a `Managed` service's own codes) names no report, and ends the
/// process as SCM reads the pair: the service-specific code only under `ERROR_SERVICE_SPECIFIC_ERROR`
/// and non-zero, 0 for Win32 0 whatever else is recorded, else the Win32 code.
#[skuld::test]
fn a_recorded_exit_no_report_produces_ends_the_process_as_scm_reads_it() {
    let record = |win32, specific| RecordedExit { win32, specific };

    for (win32, specific) in [(5, 9), (0, 42), (ERROR_SERVICE_SPECIFIC_ERROR, 0)] {
        assert_eq!(
            Reported::from_record(record(win32, specific)),
            None,
            "({win32}, {specific})"
        );
    }
    for (win32, specific, code) in [
        (5, 9, 5),
        (0, 42, 0),
        (ERROR_SERVICE_SPECIFIC_ERROR, 7, 7),
        (ERROR_SERVICE_SPECIFIC_ERROR, 0, ERROR_SERVICE_SPECIFIC_ERROR as i32),
    ] {
        assert_eq!(
            record(win32, specific).process_exit_code(),
            code,
            "({win32}, {specific})"
        );
    }
}
