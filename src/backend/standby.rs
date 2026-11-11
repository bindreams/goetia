//! A thread that serves one share of work: made before the work exists and parked until handed it,
//! or made at the point of use.

use std::fmt;
use std::io;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// A thread that serves one share of work, made in one of two ways.
///
/// Made before the work it serves ([`Standby::new`], [`Standby::with`]), it is parked until handed
/// its share with [`Standby::hand`], so a refusal to make it is known before the work starts. Handed
/// nothing — the child was never spawned, or needed nothing of it — it ends.
///
/// Made at the point of use ([`Standby::run_now`]), it is handed its item at once. That can fail, for
/// want of a thread, and the item then comes back unserved.
pub(crate) struct Standby<T>(mpsc::Sender<T>);

impl<T> fmt::Debug for Standby<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Standby")
    }
}

impl<T: Send + 'static> Standby<T> {
    /// Make the thread through `make` (the caller's way of making a thread, which may refuse) and park
    /// it until it is handed a share, which `serve` then runs on. The handle is the thread's, to join
    /// or drop.
    pub(crate) fn with(
        name: &str,
        make: impl FnOnce(&str, Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
        serve: impl FnOnce(T) + Send + 'static,
    ) -> io::Result<(Standby<T>, JoinHandle<()>)> {
        let (tx, rx) = mpsc::channel::<T>();
        let thread = make(
            name,
            Box::new(move || {
                if let Ok(share) = rx.recv() {
                    serve(share);
                }
            }),
        )?;
        Ok((Standby(tx), thread))
    }

    /// Hand the thread its share. It is parked in `recv` until then, so this cannot fail; were it
    /// to, the share comes back.
    pub(crate) fn hand(self, share: T) -> Result<(), T> {
        self.0.send(share).map_err(|mpsc::SendError(share)| share)
    }
}

// Unix makes its threads through these; Windows' only caller of `Standby` is `with`.
#[cfg_attr(not(unix), allow(dead_code))]
impl<T: Send + 'static> Standby<T> {
    /// A standby on a `std::thread`, whose handle is dropped: a failure to make it is `io::Error`,
    /// and under test the hook may refuse it.
    pub(crate) fn new(name: &str, serve: impl FnOnce(T) + Send + 'static) -> io::Result<Standby<T>> {
        #[cfg(test)]
        test_hook::make()?;
        let (standby, _thread) = Standby::with(
            name,
            |name, run| std::thread::Builder::new().name(name.to_string()).spawn(run),
            serve,
        )?;
        Ok(standby)
    }

    /// Make a thread now and run `serve` on `item` there, for work with no earlier moment to make
    /// the thread in. With no thread to be had `item` comes back, still unserved.
    pub(crate) fn run_now(name: &str, item: T, serve: impl FnOnce(T) + Send + 'static) -> Result<(), T> {
        match Standby::new(name, serve) {
            Ok(standby) => standby.hand(item),
            Err(_) => Err(item),
        }
    }
}

/// Makes thread creation fail on demand, on this thread only, so the paths that must refuse to send
/// anything when it does are testable.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;
    use std::io;
    use std::thread::LocalKey;

    /// How many more of something this thread may make: `None` is no limit.
    pub(crate) type Left = LocalKey<Cell<Option<usize>>>;

    thread_local! {
        static THREADS: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Let this thread make only `n` more threads, until the returned guard drops.
    pub(crate) fn threads(n: usize) -> Allowance {
        Allowance::set(&THREADS, n)
    }

    pub(super) fn make() -> io::Result<()> {
        spend(&THREADS, "no thread may be made (test hook)")
    }

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
}

#[cfg(test)]
#[path = "standby_tests.rs"]
mod standby_tests;
