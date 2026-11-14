//! A test journal: entries stated newest first, followers that deliver what they are given and then
//! wait to be stopped, and a record of what was asked.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use super::{Cursor, Entries, Entry, Follower, Journal, Match, Stop};
use crate::backend::bounded::Reservation;
use crate::error::{Error, Result};

/// The calls of a journal and of whatever else a test records into it, in the order they happened.
pub(in crate::backend::systemd::manager) type Log = Arc<Mutex<Vec<String>>>;

#[derive(Default)]
struct State {
    asked: Vec<Vec<String>>,
    read: usize,
    /// One per `follow`, consumed in order.
    follows: VecDeque<Script>,
    /// One per `read_after`, consumed in order.
    after: VecDeque<Vec<Entry>>,
    followed: Vec<(Vec<String>, Option<String>)>,
    read_after: Vec<(Vec<String>, String)>,
    syncs: usize,
    stops: usize,
}

/// What one follower delivers.
struct Script {
    entries: Vec<Entry>,
    /// The follower ends by itself once it has delivered them, instead of waiting to be stopped.
    ends: bool,
}

/// Clones share one record, so a test keeps a handle after giving one away.
#[derive(Clone)]
pub(in crate::backend::systemd::manager) struct Fake {
    entries: Vec<Entry>,
    /// Every query fails, before any entry.
    unreadable: bool,
    /// The read fails after the entries.
    then_fails: bool,
    sync_fails: bool,
    state: Arc<Mutex<State>>,
    log: Log,
}

impl Fake {
    pub(in crate::backend::systemd::manager) fn new(entries: Vec<Entry>) -> Fake {
        Fake {
            entries,
            unreadable: false,
            then_fails: false,
            sync_fails: false,
            state: Arc::default(),
            log: Log::default(),
        }
    }

    /// A journal no query can read (`sync` still works).
    pub(in crate::backend::systemd::manager) fn unreadable() -> Fake {
        Fake {
            unreadable: true,
            ..Fake::new(Vec::new())
        }
    }

    /// A journal whose read fails once it has given every entry.
    pub(in crate::backend::systemd::manager) fn then_fails(mut self) -> Fake {
        self.then_fails = true;
        self
    }

    /// A journal whose `sync` fails.
    pub(in crate::backend::systemd::manager) fn sync_fails(mut self) -> Fake {
        self.sync_fails = true;
        self
    }

    /// Records `sync`, `follow` and `read_after` into `log`, which a test shares with what else it
    /// orders them against.
    pub(in crate::backend::systemd::manager) fn logging(mut self, log: &Log) -> Fake {
        self.log = Arc::clone(log);
        self
    }

    /// The next `follow` delivers `entries` and then waits to be stopped.
    pub(in crate::backend::systemd::manager) fn follows(self, entries: Vec<Entry>) -> Fake {
        self.script(Script { entries, ends: false })
    }

    /// The next `follow` delivers `entries` and then ends by itself.
    pub(in crate::backend::systemd::manager) fn follows_then_ends(self, entries: Vec<Entry>) -> Fake {
        self.script(Script { entries, ends: true })
    }

    fn script(self, script: Script) -> Fake {
        self.state.lock().unwrap().follows.push_back(script);
        self
    }

    /// The next `read_after` answers `entries`.
    pub(in crate::backend::systemd::manager) fn then_reads(self, entries: Vec<Entry>) -> Fake {
        self.state.lock().unwrap().after.push_back(entries);
        self
    }

    /// The matches of every `newest_first` so far, as `journalctl` takes them.
    pub(in crate::backend::systemd::manager) fn asked(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().asked.clone()
    }

    /// How many entries the `newest_first` queries have been given.
    pub(in crate::backend::systemd::manager) fn read(&self) -> usize {
        self.state.lock().unwrap().read
    }

    /// The matches and the cursor of every `follow` so far.
    pub(in crate::backend::systemd::manager) fn followed(&self) -> Vec<(Vec<String>, Option<String>)> {
        self.state.lock().unwrap().followed.clone()
    }

    /// The matches and the cursor of every `read_after` so far.
    pub(in crate::backend::systemd::manager) fn read_after_calls(&self) -> Vec<(Vec<String>, String)> {
        self.state.lock().unwrap().read_after.clone()
    }

    pub(in crate::backend::systemd::manager) fn syncs(&self) -> usize {
        self.state.lock().unwrap().syncs
    }

    /// How many followers have been stopped.
    pub(in crate::backend::systemd::manager) fn stops(&self) -> usize {
        self.state.lock().unwrap().stops
    }

    fn arguments(matches: &[Match]) -> Vec<String> {
        matches.iter().map(Match::argument).collect()
    }

    fn unread() -> Error {
        Error::os("read the journal", std::io::Error::other("no journal"))
    }

    /// What the real journal draws for each follower and read: a temp file, so that a caller that
    /// counted too few trips the reservation's assertion in every test that drives it.
    fn draw(draws: &mut Reservation) -> Result<()> {
        draws
            .file()
            .map(drop)
            .map_err(|e| Error::os("make a file for `journalctl`'s diagnostics", e))
    }

    fn record(&self, what: String) {
        self.log.lock().unwrap().push(what);
    }
}

impl Journal for Fake {
    fn newest_first(&self, matches: &[Match]) -> Result<Box<dyn Entries>> {
        self.state.lock().unwrap().asked.push(Fake::arguments(matches));
        if self.unreadable {
            return Err(Fake::unread());
        }
        Ok(Box::new(Scripted {
            entries: self.entries.clone().into_iter(),
            then_fails: self.then_fails,
            state: Arc::clone(&self.state),
        }))
    }

    fn follow(&self, matches: &[Match], after: Option<&Cursor>, draws: &mut Reservation) -> Result<Box<dyn Follower>> {
        Fake::draw(draws)?;
        self.record(format!("follow {}", Fake::arguments(matches).join(" ")));
        let script = {
            let mut state = self.state.lock().unwrap();
            state.followed.push((
                Fake::arguments(matches),
                after.map(|cursor| cursor.as_str().to_string()),
            ));
            state.follows.pop_front()
        };
        if self.unreadable {
            return Err(Fake::unread());
        }
        let script = script.expect("an unscripted `follow`");
        Ok(Box::new(Live {
            queue: Arc::new(Queue {
                state: Mutex::new((script.entries.into(), false)),
                wake: Condvar::new(),
                ends: script.ends,
            }),
            journal: Arc::clone(&self.state),
        }))
    }

    fn read_after(&self, matches: &[Match], cursor: &Cursor, draws: &mut Reservation) -> Result<Vec<Entry>> {
        Fake::draw(draws)?;
        self.record("read_after".to_string());
        self.state
            .lock()
            .unwrap()
            .read_after
            .push((Fake::arguments(matches), cursor.as_str().to_string()));
        if self.unreadable {
            return Err(Fake::unread());
        }
        Ok(self.state.lock().unwrap().after.pop_front().unwrap_or_default())
    }

    fn sync(&self) -> Result<()> {
        self.record("sync".to_string());
        self.state.lock().unwrap().syncs += 1;
        if self.sync_fails {
            return Err(Error::os("sync the journal", std::io::Error::other("it failed")));
        }
        Ok(())
    }
}

struct Scripted {
    entries: std::vec::IntoIter<Entry>,
    then_fails: bool,
    state: Arc<Mutex<State>>,
}

impl Entries for Scripted {
    fn next(&mut self) -> Result<Option<Entry>> {
        match self.entries.next() {
            Some(entry) => {
                self.state.lock().unwrap().read += 1;
                Ok(Some(entry))
            }
            None if self.then_fails => Err(Error::os("read the journal", std::io::Error::other("it failed"))),
            None => Ok(None),
        }
    }
}

/// The entries a follower has yet to deliver, and whether it was stopped.
struct Queue {
    state: Mutex<(VecDeque<Entry>, bool)>,
    wake: Condvar,
    ends: bool,
}

struct Live {
    queue: Arc<Queue>,
    journal: Arc<Mutex<State>>,
}

impl Entries for Live {
    fn next(&mut self) -> Result<Option<Entry>> {
        let mut state = self.queue.state.lock().unwrap();
        loop {
            if let Some(entry) = state.0.pop_front() {
                return Ok(Some(entry));
            }
            if self.queue.ends || state.1 {
                return Ok(None);
            }
            state = self.queue.wake.wait(state).unwrap();
        }
    }
}

impl Follower for Live {
    fn stop(&self) -> Stop {
        let queue = Arc::clone(&self.queue);
        let journal = Arc::clone(&self.journal);
        Arc::new(move || {
            queue.state.lock().unwrap().1 = true;
            queue.wake.notify_all();
            journal.lock().unwrap().stops += 1;
        })
    }
}
