//! Everything a child's output needs, made before the child is spawned: a request whose output
//! nothing could take must not be sent, and a thread or file that could not be made once it was
//! sent would leave it unwatched — or, in a sequence of requests, stop the sequence halfway.
//!
//! A launchd verb is handed a [`Reservation`] and draws its reapers and temp files from it. systemd's
//! listener threads come from this thread's [`Spares`].

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{self, Seek as _};
use std::marker::PhantomData;
use std::process::ExitStatus;

use crossbeam_channel::Receiver;

use super::{Chunk, drain};
use crate::backend::standby::Standby;

// Reaper ==============================================================================================================

/// A thread made before a [`super::Role::Request`] is sent, which reaps it if an expiry leaves it
/// running: it still exits, and a caller that outlives it must not collect one zombie per expiry.
/// See [`Reservation`].
///
/// The thread never kills the child, not even after the reap: an expired request is left running.
/// It [`cosca::Child::detach`]es the reaped child rather than dropping it, because `Drop` tears the
/// contained tree down, and the request's descendants may still be in it. With nothing to reap, the
/// request having exited inside its budget, the thread ends.
#[derive(Debug)]
pub(crate) struct Reaper(Standby<cosca::Child>);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Reaper {
    fn new() -> io::Result<Reaper> {
        // A failed reap is dropped on purpose: the caller has already reported the expiry, the
        // crate has no log, and the child is detached whatever the wait said.
        Reaper::with(cosca::Child::wait, |_| {})
    }

    /// A reaper that waits with `wait` and hands the status to `reaped`: the seam a test drives, to
    /// release its child only once the child is the reaper's, and to learn how it ended.
    pub(super) fn with(
        wait: impl FnOnce(&cosca::Child) -> Result<ExitStatus, cosca::error::Error> + Send + 'static,
        reaped: impl FnOnce(Result<ExitStatus, cosca::error::Error>) + Send + 'static,
    ) -> io::Result<Reaper> {
        Standby::new("goetia-reaper", move |child: cosca::Child| {
            let status = wait(&child);
            let_go(child);
            reaped(status);
        })
        .map(Reaper)
    }

    pub(super) fn reap(self, child: cosca::Child) {
        if let Err(child) = self.0.hand(child) {
            debug_assert!(false, "the reaper thread ended before it was handed its child");
            let_go(child); // never killed
        }
    }
}

/// [`cosca::Child::detach`] it. Only Windows' disarm can be refused, and nothing here runs there:
/// on a refusal the child would come back armed, and dropping it would kill the tree.
fn let_go(child: cosca::Child) {
    if let Err((_armed, e)) = child.detach() {
        debug_assert!(false, "detach refused off Windows: {e}");
    }
}

// Listener ============================================================================================================

/// A thread made before its child is spawned, to read one of its pipes as it is written — for the
/// one role that must see output before the child exits.
#[derive(Debug)]
pub(super) struct Listener {
    standby: Standby<cosca::PipeReader>,
    heard: Receiver<Chunk>,
}

impl Listener {
    fn new() -> io::Result<Listener> {
        let (tx, heard) = crossbeam_channel::unbounded();
        let standby = Standby::new("goetia-listener", move |pipe| drain(pipe, tx))?;
        Ok(Listener { standby, heard })
    }

    /// Start reading `pipe`: what it holds arrives on the returned receiver, which disconnects at its
    /// EOF. `None` — a stream that was not piped — reads as an immediate EOF, never a panic.
    pub(super) fn listen(self, pipe: Option<cosca::PipeReader>) -> Receiver<Chunk> {
        if let Some(pipe) = pipe {
            let handed = self.standby.hand(pipe);
            debug_assert!(
                handed.is_ok(),
                "the listener thread ended before it was handed its pipe"
            );
        }
        self.heard
    }
}

// Reservation =========================================================================================================

/// What one launchd verb, or a sequence of them, draws: a [`Reaper`] for each request and a temp
/// file for each stream of each `launchctl` call, all made before anything is sent. Handed to every
/// call that draws, never ambient: a draw past what was made is a miscount, which fails a debug
/// assertion and is made on the spot in a release build.
#[derive(Debug, Default)]
pub(crate) struct Reservation {
    reapers: Vec<Reaper>,
    files: Vec<File>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Reservation {
    /// `reapers` and `files`, all or none: a failure to make one makes nothing, so the verb that
    /// asked sends nothing.
    pub(crate) fn make(reapers: usize, files: usize) -> io::Result<Reservation> {
        Ok(Reservation {
            reapers: made(reapers, Reaper::new)?,
            files: made(files, make_file)?,
        })
    }

    /// The reaper for one request.
    pub(crate) fn reaper(&mut self) -> io::Result<Reaper> {
        match self.reapers.pop() {
            Some(reaper) => Ok(reaper),
            None => {
                miscounted("reaper");
                Reaper::new()
            }
        }
    }

    /// An empty anonymous temp file for one stream of one call.
    pub(crate) fn file(&mut self) -> io::Result<File> {
        match self.files.pop() {
            Some(file) => Ok(file),
            None => {
                miscounted("temp file");
                make_file()
            }
        }
    }
}

/// Temp files taken out of a [`Reservation`] once, to serve a read repeated as often as it takes
/// (a wait for a job to be gone), where a count cannot be made ahead of time.
///
/// A spawn consumes the files it draws, so each repetition gets a [`Reservation`] of its own from
/// [`Kept::again`], which reuses the kept files: each is emptied and rewound, and a duplicate of it is
/// handed over. The child writes through the duplicate, which shares the open file's offset and
/// contents, so the kept file is the same file and the next read starts on an empty one.
#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Kept {
    files: Vec<File>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Reservation {
    /// `files` of this reservation's temp files, taken once: see [`Kept`].
    pub(crate) fn keep(&mut self, files: usize) -> io::Result<Kept> {
        Ok(Kept {
            files: (0..files).map(|_| self.file()).collect::<io::Result<_>>()?,
        })
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Kept {
    /// A reservation of one more read's files. The previous read must be over: nothing may still
    /// be writing to them.
    pub(crate) fn again(&self) -> io::Result<Reservation> {
        #[cfg(test)]
        test_hook::make(test_hook::Kind::Reuse)?;
        let files = self
            .files
            .iter()
            .map(|file| {
                file.set_len(0)?;
                (&*file).rewind()?;
                file.try_clone()
            })
            .collect::<io::Result<_>>()?;
        Ok(Reservation {
            reapers: Vec::new(),
            files,
        })
    }
}

fn miscounted(what: &str) {
    debug_assert!(
        false,
        "a {what} was drawn past the reservation: a verb needed more than was counted for it"
    );
}

fn made<T>(n: usize, make: fn() -> io::Result<T>) -> io::Result<Vec<T>> {
    (0..n).map(|_| make()).collect()
}

// Spares ==============================================================================================================

/// The listener threads the [`Spares`] guards on this thread made ready, each tagged with its
/// guard's number.
type Pool = Vec<(u64, Listener)>;

thread_local! {
    static SPARES: RefCell<Pool> = const { RefCell::new(Vec::new()) };
    /// The number the next [`Spares`] guard on this thread tags what it makes with.
    static NEXT_GUARD: Cell<u64> = const { Cell::new(0) };
    /// How many [`Spares`] guards are live on this thread.
    static RESERVATIONS: Cell<usize> = const { Cell::new(0) };
}

/// Called wherever a listener is made on the spot rather than taken from this thread's [`Spares`]:
/// while a reservation is live, everything its verbs need was to have been made in it, so making
/// one here means a verb needed more than was counted for it — a request could then fail halfway
/// through a sequence for want of it. Loud in every debug build, the test suites' included, so an
/// undercount fails them.
fn made_on_the_spot(what: &str) {
    debug_assert!(
        RESERVATIONS.get() == 0,
        "a {what} was made outside the reservation live on this thread: a verb needed more than was counted for it"
    );
}

/// Make `listeners` threads ready, all or none, for the verbs this thread runs while the guard lives.
/// Dropping it drops only its own unspent threads; guards nest and may end in any order.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn spare(listeners: usize) -> io::Result<Spares> {
    let listeners = made(listeners, Listener::new)?;
    let tag = NEXT_GUARD.get();
    NEXT_GUARD.set(tag + 1);
    RESERVATIONS.set(RESERVATIONS.get() + 1);
    SPARES.with_borrow_mut(|pool| pool.extend(listeners.into_iter().map(|l| (tag, l))));
    Ok(Spares {
        tag,
        this_thread: PhantomData,
    })
}

/// See [`spare`]. Not `Send`: what it releases is this thread's.
#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Spares {
    tag: u64,
    this_thread: PhantomData<*const ()>,
}

impl Drop for Spares {
    fn drop(&mut self) {
        RESERVATIONS.set(RESERVATIONS.get() - 1);
        SPARES.with_borrow_mut(|pool| pool.retain(|(tag, _)| *tag != self.tag));
    }
}

/// A thread to read a pipe with: a spare, or made now.
pub(super) fn listener() -> io::Result<Listener> {
    match SPARES.with_borrow_mut(|pool| pool.pop()) {
        Some((_, listener)) => Ok(listener),
        None => {
            made_on_the_spot("listener");
            Listener::new()
        }
    }
}

/// An empty anonymous temp file. A read no verb counted ([`super::Role::UncountedQuery`]) makes its
/// own, whatever reservation it is handed.
pub(super) fn make_file() -> io::Result<File> {
    #[cfg(test)]
    test_hook::make(test_hook::Kind::TempFile)?;
    tempfile::tempfile()
}

// test_hook ===========================================================================================================

/// Makes thread or temp-file creation fail on demand, on this thread only, so the paths that must
/// refuse to send anything when it does are testable.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::Cell;

    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Kind {
        Thread,
        TempFile,
        Reuse,
    }

    thread_local! {
        static THREADS: Cell<Option<usize>> = const { Cell::new(None) };
        static TEMP_FILES: Cell<Option<usize>> = const { Cell::new(None) };
        static REUSES: Cell<Option<usize>> = const { Cell::new(None) };
        static SPAWNS: Cell<usize> = const { Cell::new(0) };
        static SPAWN_FAILS: std::cell::RefCell<Option<fn() -> cosca::error::Error>> =
            const { std::cell::RefCell::new(None) };
        static WAIT_FAILS: std::cell::RefCell<Option<fn() -> cosca::error::Error>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Make this thread's next wait on a spawned child fail with `error()`, the child already
    /// running.
    pub(crate) fn wait_fails(error: fn() -> cosca::error::Error) {
        WAIT_FAILS.set(Some(error));
    }

    /// The failure [`wait_fails`] set up for this wait, if any.
    pub(crate) fn waiting() -> Option<cosca::error::Error> {
        WAIT_FAILS.take().map(|error| error())
    }

    /// Make this thread's next spawn fail with `error()` instead of running anything, as cosca's
    /// would.
    pub(crate) fn spawn_fails(error: fn() -> cosca::error::Error) {
        SPAWN_FAILS.set(Some(error));
    }

    /// What a cosca spawn fails with once its program may have started, `source` being what failed.
    pub(crate) fn may_have_started(source: cosca::error::Error) -> cosca::error::Error {
        cosca::error::Error::MayHaveStarted {
            fate: cosca::error::ChildFate::Reaped,
            wrapper_elevated: false,
            source: Box::new(source),
            unreaped: None,
        }
    }

    /// How many listeners this thread's pool holds now.
    pub(crate) fn pooled() -> usize {
        super::SPARES.with_borrow(|pool| pool.len())
    }

    /// What `reservation` holds now: its reapers and its temp files.
    pub(crate) fn left(reservation: &super::Reservation) -> (usize, usize) {
        (reservation.reapers.len(), reservation.files.len())
    }

    /// How many children this thread has spawned through [`super::super::spawn`], or tried to.
    pub(crate) fn spawns() -> usize {
        SPAWNS.get()
    }

    /// Count a spawn, and hand back the failure [`spawn_fails`] set up for it, if any.
    pub(in crate::backend::bounded) fn spawning() -> Option<cosca::error::Error> {
        SPAWNS.set(SPAWNS.get() + 1);
        SPAWN_FAILS.take().map(|error| error())
    }

    fn allowance(kind: Kind) -> &'static std::thread::LocalKey<Cell<Option<usize>>> {
        match kind {
            Kind::Thread => &THREADS,
            Kind::TempFile => &TEMP_FILES,
            Kind::Reuse => &REUSES,
        }
    }

    /// Let this thread make only `n` more threads, until the returned guard drops.
    pub(crate) fn threads(n: usize) -> Allowance {
        allow(Kind::Thread, n)
    }

    /// Let this thread make only `n` more temp files, until the returned guard drops.
    pub(crate) fn temp_files(n: usize) -> Allowance {
        allow(Kind::TempFile, n)
    }

    /// Let this thread run only `n` more [`super::Kept::again`]s, until the returned guard drops.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn reuses(n: usize) -> Allowance {
        allow(Kind::Reuse, n)
    }

    fn allow(kind: Kind, n: usize) -> Allowance {
        allowance(kind).set(Some(n));
        Allowance(kind)
    }

    /// See [`threads`] and [`temp_files`].
    pub(crate) struct Allowance(Kind);

    impl Drop for Allowance {
        fn drop(&mut self) {
            allowance(self.0).set(None);
        }
    }

    pub(crate) fn make(kind: Kind) -> std::io::Result<()> {
        let left = allowance(kind);
        match left.get() {
            Some(0) => Err(std::io::Error::other(match kind {
                Kind::Thread => "no thread may be made (test hook)",
                Kind::TempFile => "no temp file may be made (test hook)",
                Kind::Reuse => "no kept file may be reused (test hook)",
            })),
            Some(n) => {
                left.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }
}
