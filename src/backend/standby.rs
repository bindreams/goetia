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

#[cfg(test)]
#[path = "standby_tests.rs"]
mod standby_tests;
