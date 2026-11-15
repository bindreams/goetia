//! The skuld label of every test that writes real system state. The tests also belong to a group
//! (`super::groups`), which is what gates them.

/// Marker for the tests that write real system state: `SKULD_LABELS='!elevated'` leaves them out.
#[skuld::label]
pub const ELEVATED: skuld::Label;

/// Fails the calling test, naming the label that leaves it out, unless this process is elevated. For a
/// test that is not in a group (it changes nothing that is not its own) but needs root: to make a
/// directory root owns, to give a file to another uid.
pub fn require_elevated() {
    assert!(
        cosca::elevation::is_elevated(),
        "this test needs an elevated process (root); run the elevated tests as root, or leave them out with \
         SKULD_LABELS='!elevated'"
    );
}
