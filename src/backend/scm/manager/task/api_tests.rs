use std::time::{Duration, UNIX_EPOCH};

use super::*;

/// The OLE date of 1970-01-01 00:00.
const OLE_UNIX_EPOCH: f64 = 25569.0;

/// How far a local date may sit from the same digits read as UTC: the widest zone offset.
const WIDEST_ZONE: Duration = Duration::from_secs(15 * 3600);

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

// A zone-wide window: on a runner whose zone is UTC this cannot tell a local reading from a UTC one.
#[skuld::test]
fn ole_date_lands_within_a_zone_of_the_digits_read_as_utc() {
    let read = ole_date(OLE_UNIX_EPOCH + 18_000.5).expect("a date after the sentinel is a time");

    let as_utc = UNIX_EPOCH + Duration::from_secs(18_000 * 86_400 + 43_200);
    assert!(
        apart(read, as_utc) <= WIDEST_ZONE,
        "{read:?} is too far from {as_utc:?}"
    );
}

#[skuld::test]
fn ole_dates_keep_their_order() {
    let earlier = ole_date(OLE_UNIX_EPOCH + 18_000.0).unwrap();
    let later = ole_date(OLE_UNIX_EPOCH + 18_001.0).unwrap();

    assert!(earlier < later);
    assert!(apart(later, earlier) >= Duration::from_secs(23 * 3600));
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
