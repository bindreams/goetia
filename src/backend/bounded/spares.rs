//! Everything a child's output needs, made before the child is spawned: a request whose output
//! nothing could take must not be sent, and a thread or file that could not be made once it was
//! sent would leave it unwatched — or, in a sequence of requests, stop the sequence halfway.
//!
//! Each is taken from this thread's [`Spares`] first, and made on the spot only for the rest.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs::File;
use std::io::{self, PipeReader};
use std::marker::PhantomData;
use std::process::ExitStatus;
use std::sync::mpsc;
use std::thread;

use crossbeam_channel::Receiver;

use super::{Chunk, drain};

// Standby =============================================================================================================

/// A thread made before the child it serves is spawned, parked until it is handed its share of that
/// child. Handed nothing — the child was never spawned, or needed nothing of it — it ends.
struct Standby<T>(mpsc::Sender<T>);

impl<T> fmt::Debug for Standby<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Standby")
    }
}

impl<T: Send + 'static> Standby<T> {
    fn new(name: &str, serve: impl FnOnce(T) + Send + 'static) -> io::Result<Standby<T>> {
        #[cfg(test)]
        test_hook::make(test_hook::Kind::Thread)?;
        let (tx, rx) = mpsc::channel::<T>();
        thread::Builder::new().name(name.to_string()).spawn(move || {
            if let Ok(share) = rx.recv() {
                serve(share);
            }
        })?;
        Ok(Standby(tx))
    }

    /// Hand the thread its share. It is parked in `recv` until then, so this cannot fail; were it
    /// to, the share comes back.
    fn hand(self, share: T) -> Result<(), T> {
        self.0.send(share).map_err(|mpsc::SendError(share)| share)
    }
}

// Reaper ==============================================================================================================

/// A thread made before a [`super::Role::Request`] is sent, which reaps it if an expiry leaves it
/// running: it still exits, and a caller that outlives it must not collect one zombie per expiry.
/// See [`reapers`].
///
/// The thread never kills the child, not even after the reap. It [`cosca::Child::detach`]es the
/// reaped child rather than dropping it, because `Drop` tears a contained tree down, and after the
/// reap that is the reap-then-recycle hazard [`super::wait_bounded`]'s kill path is ordered to
/// avoid: macOS's `FdMarker` `killpg`s the root's pgid, which the kernel may by then have handed to
/// another group. `detach` touches no pid. With nothing to reap — the request exited inside its
/// budget — the thread ends.
#[derive(Debug)]
pub(crate) struct Reaper(Standby<cosca::Child>);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Reaper {
    fn new() -> io::Result<Reaper> {
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
            child.detach();
            reaped(status);
        })
        .map(Reaper)
    }

    pub(super) fn reap(self, child: cosca::Child) {
        if let Err(child) = self.0.hand(child) {
            debug_assert!(false, "the reaper thread ended before it was handed its child");
            child.detach(); // never killed
        }
    }
}

// Listener ============================================================================================================

/// A thread made before its child is spawned, to read one of its pipes as it is written — for the
/// one role that must see output before the child exits.
#[derive(Debug)]
pub(super) struct Listener {
    standby: Standby<PipeReader>,
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
    pub(super) fn listen(self, pipe: Option<PipeReader>) -> Receiver<Chunk> {
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

// Spares ==============================================================================================================

/// How many of each a sequence of verbs can need — see [`spare`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Needs {
    /// One per [`super::Role::Request`].
    pub reapers: usize,
    /// Two per [`super::Role::AnnouncedRequest`], one for each stream.
    pub listeners: usize,
    /// Two per [`super::Role::Query`] or [`super::Role::Request`], one for each stream.
    pub files: usize,
}

/// What the [`Spares`] guards on this thread made ready, each tagged with its guard's number.
#[derive(Default)]
struct Pool {
    reapers: Vec<(u64, Reaper)>,
    listeners: Vec<(u64, Listener)>,
    files: Vec<(u64, File)>,
}

thread_local! {
    static SPARES: RefCell<Pool> = RefCell::default();
    /// The number the next [`Spares`] guard on this thread tags what it makes with.
    static NEXT_GUARD: Cell<u64> = const { Cell::new(0) };
}

/// Make `needs` ready now, all or none, for the verbs this thread runs while the returned guard lives
/// — a sequence, such as `restart`'s stop and start, whose later verbs must not fail for want of a
/// thread or a file once an earlier one has sent something. Any verb run on this thread meanwhile may
/// draw on them. Dropping the guard drops what is left of its own, and only that: guards nest, and
/// end in any order.
pub(crate) fn spare(needs: Needs) -> io::Result<Spares> {
    let reapers = made(needs.reapers, Reaper::new)?;
    let listeners = made(needs.listeners, Listener::new)?;
    let files = made(needs.files, make_file)?;
    let tag = NEXT_GUARD.get();
    NEXT_GUARD.set(tag + 1);
    SPARES.with_borrow_mut(|pool| {
        pool.reapers.extend(reapers.into_iter().map(|r| (tag, r)));
        pool.listeners.extend(listeners.into_iter().map(|l| (tag, l)));
        pool.files.extend(files.into_iter().map(|f| (tag, f)));
    });
    Ok(Spares {
        tag,
        this_thread: PhantomData,
    })
}

/// [`spare`], less what this thread's pool already holds: for a verb that needs `needs` itself, and
/// may be running inside a sequence that made them ready already.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn top_up(needs: Needs) -> io::Result<Spares> {
    let short = SPARES.with_borrow(|pool| Needs {
        reapers: needs.reapers.saturating_sub(pool.reapers.len()),
        listeners: needs.listeners.saturating_sub(pool.listeners.len()),
        files: needs.files.saturating_sub(pool.files.len()),
    });
    spare(short)
}

fn made<T>(n: usize, make: fn() -> io::Result<T>) -> io::Result<Vec<T>> {
    (0..n).map(|_| make()).collect()
}

/// See [`spare`]. Not `Send`: what it releases is this thread's.
#[derive(Debug)]
pub(crate) struct Spares {
    tag: u64,
    this_thread: PhantomData<*const ()>,
}

impl Drop for Spares {
    fn drop(&mut self) {
        SPARES.with_borrow_mut(|pool| {
            pool.reapers.retain(|(tag, _)| *tag != self.tag);
            pool.listeners.retain(|(tag, _)| *tag != self.tag);
            pool.files.retain(|(tag, _)| *tag != self.tag);
        });
    }
}

/// Every reaper a verb's requests can need, made before it sends any: a sequence of requests must
/// not fail between two of them for want of a thread, so a failure here means nothing was sent.
/// Taken from this thread's [`Spares`] first, and made for the rest.
///
/// An array, so each request is handed a reaper of its own by name, and a verb that sends one more
/// request than it reserved for does not compile.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn reapers<const N: usize>() -> io::Result<[Reaper; N]> {
    let mut got: Vec<Reaper> = SPARES.with_borrow_mut(|pool| {
        let keep = pool.reapers.len().saturating_sub(N);
        pool.reapers.split_off(keep).into_iter().map(|(_, r)| r).collect()
    });
    while got.len() < N {
        got.push(Reaper::new()?);
    }
    Ok(got.try_into().expect("exactly N reapers were gathered"))
}

/// A thread to read a pipe with: a spare, or made now.
pub(super) fn listener() -> io::Result<Listener> {
    match SPARES.with_borrow_mut(|pool| pool.listeners.pop()) {
        Some((_, listener)) => Ok(listener),
        None => Listener::new(),
    }
}

/// An empty anonymous temp file for a stream: a spare, or made now.
pub(super) fn file() -> io::Result<File> {
    match SPARES.with_borrow_mut(|pool| pool.files.pop()) {
        Some((_, file)) => Ok(file),
        None => make_file(),
    }
}

fn make_file() -> io::Result<File> {
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
    }

    thread_local! {
        static THREADS: Cell<Option<usize>> = const { Cell::new(None) };
        static TEMP_FILES: Cell<Option<usize>> = const { Cell::new(None) };
        static SPAWNS: Cell<usize> = const { Cell::new(0) };
    }

    /// How many children this thread has spawned through [`super::super::spawn`], or tried to.
    pub(crate) fn spawns() -> usize {
        SPAWNS.get()
    }

    pub(in crate::backend::bounded) fn spawning() {
        SPAWNS.set(SPAWNS.get() + 1);
    }

    fn allowance(kind: Kind) -> &'static std::thread::LocalKey<Cell<Option<usize>>> {
        match kind {
            Kind::Thread => &THREADS,
            Kind::TempFile => &TEMP_FILES,
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

    pub(super) fn make(kind: Kind) -> std::io::Result<()> {
        let left = allowance(kind);
        match left.get() {
            Some(0) => Err(std::io::Error::other(match kind {
                Kind::Thread => "no thread may be made (test hook)",
                Kind::TempFile => "no temp file may be made (test hook)",
            })),
            Some(n) => {
                left.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }
}
