//! Waits that end on the state being waited for, with no cap: a state that never arrives hangs the test.

/// Re-check `condition` until it holds, with no cap: it is the state being waited for that ends the
/// wait. A state that never arrives hangs the test.
pub fn until(mut condition: impl FnMut() -> bool) {
    while !condition() {
        std::thread::yield_now();
    }
}

/// [`until`] for a condition that costs a process or a query to check: it sleeps `pause` between
/// checks, and the state still ends the wait.
pub fn until_paced(pause: std::time::Duration, mut condition: impl FnMut() -> bool) {
    while !condition() {
        std::thread::sleep(pause);
    }
}
