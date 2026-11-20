use std::time::{Duration, UNIX_EPOCH};

use super::*;

/// The OLE date of 1970-01-01 00:00.
const OLE_UNIX_EPOCH: f64 = 25569.0;

use super::super::support::fixed_zone;

fn apart(a: std::time::SystemTime, b: std::time::SystemTime) -> Duration {
    a.duration_since(b).unwrap_or_else(|e| e.duration())
}

// M1: Task Scheduler gives a date it has none for (a task that never ran) as 1999-11-30 00:00, OLE date
// 36494; zero is the absence COM itself uses.
#[skuld::test]
fn ole_date_maps_the_sentinels_m1_measured_to_none() {
    assert_eq!(ole_date(36494.0), None);
    assert!(
        ole_date(36494.5).is_some(),
        "only the sentinel's midnight is the sentinel"
    );
    assert_eq!(ole_date(0.0), None);
    assert_eq!(ole_date(-1.0), None);
    assert_eq!(ole_date(f64::NAN), None);
    assert_eq!(ole_date(f64::INFINITY), None);
    assert_eq!(ole_date(f64::NEG_INFINITY), None);
}

/// A COM date is a local civil time: the same digits are other instants in other zones, and the machine's
/// zone is whatever the runner has, so the offset is pinned in zones the test names.
#[skuld::test]
fn an_ole_date_is_read_as_a_local_time_in_the_zone_it_is_given() {
    let date = OLE_UNIX_EPOCH + 18_000.5;
    let as_utc = UNIX_EPOCH + Duration::from_secs(18_000 * 86_400 + 43_200);

    assert_eq!(ole_date_in(Some(&fixed_zone(0)), date), Some(as_utc));
    assert_eq!(
        ole_date_in(Some(&fixed_zone(-5)), date),
        Some(as_utc + Duration::from_secs(5 * 3600))
    );
    assert_eq!(
        ole_date_in(Some(&fixed_zone(9)), date),
        Some(as_utc - Duration::from_secs(9 * 3600))
    );
}

#[skuld::test]
fn ole_dates_keep_their_order() {
    let earlier = ole_date(OLE_UNIX_EPOCH + 18_000.0).unwrap();
    let later = ole_date(OLE_UNIX_EPOCH + 18_001.0).unwrap();

    assert!(earlier < later);
    assert!(apart(later, earlier) >= Duration::from_secs(23 * 3600));
}

// A task is left with its own flag false by an install or an uninstall that did not finish, so a `Run` of one
// names that cause and the two remedies that are always right: the install again with `--force`, and `uninstall`.
// It names no `enable`, which is refused for a task with no schedule, arms one that was disarmed, and leaves the
// removal's marker.
#[skuld::test]
fn a_run_refused_for_a_disabled_task_names_the_cause_and_both_remedies() {
    use windows::Win32::Foundation::{E_ACCESSDENIED, SCHED_E_TASK_DISABLED};

    let disabled = Error::os("start `\\Goetia\\x`", windows::core::Error::from(SCHED_E_TASK_DISABLED));
    let said = disabled_run(disabled, "x");

    let text = said.to_string();
    assert!(
        text.contains("`x` is disabled")
            && text.contains("`x` is being, or did not finish being, installed or uninstalled"),
        "the cause is named: {text}"
    );
    assert!(
        text.contains("run the install again with `--force`") && text.contains("`goetia daemon uninstall x`"),
        "both remedies are named: {text}"
    );
    assert!(!text.contains("enable"), "no `enable`: {text}");
    assert!(com_code(&said) == Some(SCHED_E_TASK_DISABLED), "the cause is kept");

    let other = Error::os("start `\\Goetia\\x`", windows::core::Error::from(E_ACCESSDENIED));
    assert!(
        matches!(disabled_run(other, "x"), Error::Os { .. }),
        "any other failure is as it was"
    );
}

// The code is found wherever the error carries the COM failure: in an `io::Error`'s payload and in either half of
// a `Both`.
#[skuld::test]
fn a_com_code_is_found_in_a_payload_and_in_each_half_of_a_both() {
    use windows::Win32::Foundation::{E_ACCESSDENIED, SCHED_E_TASK_NOT_RUNNING};

    let com = || windows::core::Error::from(SCHED_E_TASK_NOT_RUNNING);
    let other = || Error::os("read", std::io::Error::other("no code"));
    let both = |first, then| Error::Both {
        first: Box::new(first),
        then: Box::new(then),
    };
    let in_payload = Error::os("stop an instance", std::io::Error::other(com()));

    assert_eq!(com_code(&in_payload), Some(SCHED_E_TASK_NOT_RUNNING));
    assert!(is_not_running(&in_payload));
    assert_eq!(
        com_code(&both(Error::os("stop", com()), other())),
        Some(SCHED_E_TASK_NOT_RUNNING)
    );
    assert_eq!(
        com_code(&both(other(), Error::os("stop", com()))),
        Some(SCHED_E_TASK_NOT_RUNNING)
    );
    assert_eq!(
        com_code(&both(
            Error::os("stop", windows::core::Error::from(E_ACCESSDENIED)),
            Error::os("stop", com())
        )),
        Some(E_ACCESSDENIED),
        "the first half's code comes first"
    );
}

// The one failure of a `Stop` a caller takes as a stop: the run ended before it was asked to.
#[skuld::test]
fn only_task_schedulers_not_running_code_says_the_run_had_ended() {
    use windows::Win32::Foundation::{E_ACCESSDENIED, SCHED_E_TASK_NOT_RUNNING};

    let ended = Error::os("stop an instance", windows::core::Error::from(SCHED_E_TASK_NOT_RUNNING));
    let wrapped = Error::Step {
        what: "stop `x`".to_owned(),
        source: Box::new(Error::os(
            "stop an instance",
            windows::core::Error::from(SCHED_E_TASK_NOT_RUNNING),
        )),
    };
    let denied = Error::os("stop an instance", windows::core::Error::from(E_ACCESSDENIED));

    assert!(is_not_running(&ended));
    assert!(is_not_running(&wrapped));
    assert!(!is_not_running(&denied));
    assert!(!is_not_running(&Error::os(
        "stop an instance",
        std::io::Error::other("x")
    )));
}
