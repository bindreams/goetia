//! The skuld label of every test that writes real system state. The tests also belong to a group
//! (`super::groups`), which is what gates them.

/// Marker for the tests that write real system state: `SKULD_LABELS='!elevated'` leaves them out.
#[skuld::label]
pub const ELEVATED: skuld::Label;
