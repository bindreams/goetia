//! A setting one thread holds for the test hooks that refuse on demand: a count of how many of
//! something it may make ([`spend`]), or a value a hook answers from ([`Allowance`]). It is cleared
//! when its guard drops, or earlier if a hook takes it.

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

    /// Whether a hook has taken the value: for a value, not a count, which [`spend`] never takes.
    pub(crate) fn taken(&self) -> bool {
        let value = self.0.take();
        let taken = value.is_none();
        self.0.set(value);
        taken
    }
}

impl<T: 'static> Drop for Allowance<T> {
    fn drop(&mut self) {
        self.0.set(None);
    }
}
