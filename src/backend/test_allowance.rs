//! A setting one thread holds for the test hooks that refuse on demand: a count of how many of
//! something it may make ([`spend`]), or a value a hook answers from ([`Allowance`]). A count stays
//! until its guard drops; a value that a hook takes (the kill's refusal) is gone at its first use, or
//! when its guard drops first.

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

/// Holds a setting of `left` until it drops, which clears it, unless a hook has taken it first. A
/// count is a limit ([`spend`]); any other `T` is a value a hook answers from.
pub(crate) struct Allowance<T: 'static = usize>(&'static LocalKey<Cell<Option<T>>>);

impl<T: 'static> Allowance<T> {
    pub(crate) fn set(left: &'static LocalKey<Cell<Option<T>>>, value: T) -> Allowance<T> {
        left.set(Some(value));
        Allowance(left)
    }
}

impl<T: 'static> Drop for Allowance<T> {
    fn drop(&mut self) {
        self.0.set(None);
    }
}
