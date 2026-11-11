//! A limit on how many of something one thread may make, for the test hooks that refuse the next
//! thread, temp file or reuse on demand.

use std::cell::Cell;
use std::io;
use std::thread::LocalKey;

/// How many more of something this thread may make: `None` is no limit.
type Left = LocalKey<Cell<Option<usize>>>;

/// Spend one of `left`, or refuse with `refusal` when none remain.
pub(crate) fn spend(left: &'static Left, refusal: &str) -> io::Result<()> {
    match left.get() {
        Some(0) => Err(io::Error::other(refusal)),
        Some(n) => {
            left.set(Some(n - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

/// Holds a limit on `left` until it drops, which lifts it.
pub(crate) struct Allowance(&'static Left);

impl Allowance {
    pub(crate) fn set(left: &'static Left, n: usize) -> Allowance {
        left.set(Some(n));
        Allowance(left)
    }
}

impl Drop for Allowance {
    fn drop(&mut self) {
        self.0.set(None);
    }
}
