use std::time::{Duration, UNIX_EPOCH};

use super::*;

fn local(year: u16, month: u16, day: u16, hour: u16) -> SYSTEMTIME {
    SYSTEMTIME {
        wYear: year,
        wMonth: month,
        wDay: day,
        wHour: hour,
        ..SYSTEMTIME::default()
    }
}

// Mid-July and mid-January fall in no zone's change of clocks, so a local time maps back to one instant.
#[skuld::test]
fn a_time_survives_the_trip_to_local_time_and_back() {
    for secs in [1_688_212_800_u64, 1_705_320_000] {
        let time = UNIX_EPOCH + Duration::from_secs(secs);

        let local = system_to_local(time).unwrap();

        assert_eq!(local_to_system(&local), Some(time), "{secs}");
    }
}

#[skuld::test]
fn a_local_date_is_within_a_zone_of_the_same_digits_read_as_utc() {
    // 2024-01-15 12:00 as UTC.
    let as_utc = UNIX_EPOCH + Duration::from_secs(1_705_320_000);

    let read = local_to_system(&local(2024, 1, 15, 12)).unwrap();

    let apart = read.duration_since(as_utc).unwrap_or_else(|e| e.duration());
    assert!(apart <= Duration::from_secs(15 * 3600), "{read:?}");
}
