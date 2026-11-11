//! The thread made before the work it serves exists, parked until it is handed its share.

use std::fmt;
use std::io;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// A thread made before the child it serves is spawned, parked until it is handed its share of that
/// child. Handed nothing — the child was never spawned, or needed nothing of it — it ends.
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

#[cfg(unix)]
impl<T: Send + 'static> Standby<T> {
    /// A standby on a `std::thread`, whose handle is dropped: a failure to make it is `io::Error`,
    /// and under test the hook may refuse it.
    pub(crate) fn new(name: &str, serve: impl FnOnce(T) + Send + 'static) -> io::Result<Standby<T>> {
        #[cfg(test)]
        crate::backend::bounded::test_hook::make(crate::backend::bounded::test_hook::Kind::Thread)?;
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

#[cfg(test)]
#[path = "standby_tests.rs"]
mod standby_tests;
