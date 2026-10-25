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
fn daemon_code_3_and_decode_failure_are_distinct_reports() {
    let daemon = Reported::from_daemon_code(3);
    let decode = Reported::shim_failure(Some(ERROR_INVALID_DATA as i32));

    assert_eq!(daemon.scm_codes(), (ERROR_SERVICE_SPECIFIC_ERROR, 3));
    assert_eq!(decode.scm_codes(), (ERROR_INVALID_DATA, 0));
    assert_ne!(daemon, decode);
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
