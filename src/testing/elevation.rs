//! The skuld label of every test that needs an elevated token. The tests that write real system
//! state also belong to a group (`super::groups`), which is what gates them.

/// Marker for the tests that write real system state or need an elevated token:
/// `SKULD_LABELS='!elevated'` leaves them out.
#[skuld::label]
pub const ELEVATED: skuld::Label;
