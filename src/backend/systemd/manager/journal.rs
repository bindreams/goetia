//! The system journal, read through `journalctl`: what the manager wrote about a task's runs.
//!
//! A seam of its own ([`Journal`]) beside [`Client`](crate::backend::client::Client), which runs a
//! program to completion or a deadline and has no way to hand over what a program writes as it is
//! written. Every query passes `-a` (a `MESSAGE` of 4088 bytes or more is `null` in JSON without it)
//! and `-o json`; a field is a string, or an array of byte numbers where it is not UTF-8, and both
//! decode.
//!
//! This is the read newest first, which `status` asks. A run's entries (what the manager wrote when it
//! started, and how its main process ended) are the `MESSAGE_ID`s below, measured on systemd 255
//! (knowledge base: "A oneshot run ends in a different journal entry per outcome").

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::bounded;
use crate::error::{Error, Result};
use crate::manager::Exit;

/// "Starting `<unit>`...": the manager began a run. Carries the run's `INVOCATION_ID` and the job's
/// `JOB_ID`.
pub(super) const STARTING: &str = "7d4958e842da4a758f6c1cdc7b36dcc5";

/// "Main process exited": a run's main process ended with `EXIT_CODE` (`exited`, `killed` or
/// `dumped`) and `EXIT_STATUS` (the exit status, or the signal's number).
pub(super) const MAIN_PROCESS_EXITED: &str = "98e322203f7a4ed290d09fe03c09fe15";

/// "Deactivated successfully": a run ended with exit status 0, which has no "Main process exited"
/// entry of its own.
pub(super) const DEACTIVATED: &str = "7ad2d189f7e94e70a38c781354912448";

// Match / Entry =======================================================================================================

/// One `FIELD=value` match: only entries holding it are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Match {
    name: &'static str,
    value: String,
}

impl Match {
    pub(super) fn new(name: &'static str, value: impl Into<String>) -> Match {
        Match {
            name,
            value: value.into(),
        }
    }

    /// The argument `journalctl` takes it as.
    pub(super) fn argument(&self) -> String {
        format!("{}={}", self.name, self.value)
    }
}

/// One journal entry: its fields, each as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    fields: BTreeMap<String, String>,
}

impl Entry {
    /// The field `name`, if the entry has it.
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    /// When the entry was written: its `__REALTIME_TIMESTAMP`, in microseconds.
    pub(super) fn realtime(&self) -> Result<SystemTime> {
        self.get("__REALTIME_TIMESTAMP")
            .and_then(|micros| micros.parse::<u64>().ok())
            .map(|micros| UNIX_EPOCH + Duration::from_micros(micros))
            .ok_or_else(|| lacks("a time"))
    }

    /// One line of `journalctl -o json`.
    fn parse(line: &[u8]) -> Result<Entry> {
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|e| Error::os("read an entry of the journal's JSON", std::io::Error::other(e)))?;
        let serde_json::Value::Object(object) = value else {
            return Err(lacks("an object"));
        };
        let fields = object
            .into_iter()
            .filter_map(|(name, value)| field_text(&value).map(|text| (name, text)))
            .collect();
        Ok(Entry { fields })
    }

    #[cfg(test)]
    pub(super) fn of(fields: &[(&str, &str)]) -> Entry {
        Entry {
            fields: fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }
}

/// A field's value as text: a string as it is, an array of byte numbers (a value that is not
/// UTF-8, or has a control character) as the bytes it is, and the first of an array of values (a
/// field the entry holds more than once). `null` is no field.
fn field_text(value: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) if items.iter().all(Value::is_number) => {
            let bytes: Option<Vec<u8>> = items
                .iter()
                .map(|n| n.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            bytes.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        }
        Value::Array(items) => items.first().and_then(field_text),
        _ => None,
    }
}

/// An answer of `journalctl` without `what`.
fn lacks(what: &str) -> Error {
    Error::IncompleteAnswer {
        command: "journalctl".to_string(),
        missing: vec![what.to_string()],
    }
}

// Journal =============================================================================================================

/// The system journal, as the systemd backend reads it.
pub(super) trait Journal: Send + Sync {
    /// The entries holding every one of `matches`, newest first. `Err` where the journal cannot be
    /// read at all, which is never an empty answer: a host with no journal files answers with none.
    fn newest_first(&self, matches: &[Match]) -> Result<Box<dyn Entries>>;
}

/// The entries of one query, as they are read. Dropping it ends the `journalctl` behind it.
pub(super) trait Entries: Send {
    /// The next entry, `None` once the query has no more. `Err` where the read failed, which is not
    /// the end of the entries.
    fn next(&mut self) -> Result<Option<Entry>>;
}

// Terminal entries ====================================================================================================

/// How the run an entry belongs to ended, when `entry` is the entry that says so: `None` for any
/// other entry. `Some(Err)` is an entry that says a run ended and not how, since a status that is
/// not a number, or an `EXIT_CODE` that is none of `exited`, `killed` and `dumped`, is not read as a
/// code or a signal.
///
/// "Main process exited" carries the exit status (`exited`) or the signal (`killed`, and `dumped` for
/// one that dumped a core); "Deactivated successfully" is exit status 0, which has no entry of its own.
pub(super) fn exit_of(entry: &Entry) -> Option<Result<Exit>> {
    match entry.get("MESSAGE_ID")? {
        DEACTIVATED => Some(Ok(Exit::Code(0))),
        MAIN_PROCESS_EXITED => Some(main_process_exit(entry)),
        _ => None,
    }
}

fn main_process_exit(entry: &Entry) -> Result<Exit> {
    let status = entry
        .get("EXIT_STATUS")
        .and_then(|status| status.parse::<i32>().ok())
        .ok_or_else(|| lacks("an exit status"))?;
    match entry.get("EXIT_CODE") {
        Some("exited") => Ok(Exit::Code(status)),
        Some("killed" | "dumped") => Ok(Exit::Signal(status)),
        _ => Err(lacks("an exit code of `exited`, `killed` or `dumped`")),
    }
}

// last_run ============================================================================================================

/// What the journal says of a task's most recent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LastRun {
    /// When it started: `None` when the journal holds no run.
    pub started: Option<SystemTime>,
    /// How it ended: `None` while it runs, and when it ended with no entry to say so.
    pub exit: Option<Exit>,
}

/// The most recent run of `unit`, from one walk newest first over every entry the manager wrote about
/// it, across the unit's whole life in the journal.
///
/// The newest "Starting" entry is the newest run; its terminal entry is the one of its own
/// `INVOCATION_ID`, which the walk met on the way, so an older run's terminal entry is never taken
/// for it. The walk stops at that entry: the entries it left are never read, and dropping them ends
/// the `journalctl`.
pub(super) fn last_run(journal: &dyn Journal, unit: &str) -> Result<LastRun> {
    let mut entries = journal.newest_first(&[Match::new("UNIT", unit)])?;
    let mut ended: Vec<(Option<String>, Result<Exit>)> = Vec::new();
    while let Some(entry) = entries.next()? {
        if entry.get("MESSAGE_ID") == Some(STARTING) {
            let invocation = entry.get("INVOCATION_ID");
            let exit = ended
                .into_iter()
                .find(|(of, _)| invocation.is_some() && of.as_deref() == invocation)
                .map(|(_, exit)| exit)
                .transpose()?;
            return Ok(LastRun {
                started: Some(entry.realtime()?),
                exit,
            });
        }
        if let Some(exit) = exit_of(&entry) {
            ended.push((entry.get("INVOCATION_ID").map(str::to_string), exit));
        }
    }
    Ok(LastRun {
        started: None,
        exit: None,
    })
}

// Journalctl ==========================================================================================================

/// The real journal: `journalctl`, run once per query.
pub(super) struct Journalctl {
    program: Vec<String>,
}

impl Journalctl {
    pub(super) fn native() -> Journalctl {
        Journalctl::of(vec!["journalctl".to_string()])
    }

    /// The journal as `program` answers it, with the arguments of a query after `program`'s own.
    pub(super) fn of(program: Vec<String>) -> Journalctl {
        debug_assert!(!program.is_empty(), "a program to run");
        Journalctl { program }
    }
}

impl Journal for Journalctl {
    fn newest_first(&self, matches: &[Match]) -> Result<Box<dyn Entries>> {
        let arguments: Vec<String> = ["-a", "-o", "json", "--no-pager", "-r"]
            .into_iter()
            .map(str::to_string)
            .chain(matches.iter().map(Match::argument))
            .collect();
        let argv: Vec<&str> = self.program.iter().chain(&arguments).map(String::as_str).collect();
        let command = argv.join(" ");
        let stderr = tempfile::tempfile().map_err(|e| Error::os("make a file for `journalctl`'s diagnostics", e))?;
        let spawn_failed = |e: cosca::error::Error| Error::spawn(command.clone(), e);
        let diagnostics = stderr
            .try_clone()
            .map_err(|e| Error::os("make a file for `journalctl`'s diagnostics", e))?;
        let mut cmd = bounded::command(argv[0], &argv[1..]).map_err(spawn_failed)?;
        cmd.stdout(cosca::Stdio::pipe_out()).map_err(spawn_failed)?;
        cmd.stderr(cosca::Stdio::from_file(diagnostics)).map_err(spawn_failed)?;
        let mut child = cmd.spawn().map_err(spawn_failed)?;
        let Some(stdout) = child.stdout() else {
            return Err(Error::spawn(
                command,
                std::io::Error::other("the child has no stdout pipe"),
            ));
        };
        Ok(Box::new(Lines {
            lines: BufReader::new(stdout),
            child,
            stderr,
            command,
            ended: false,
        }))
    }
}

/// The lines `journalctl -o json` writes, one entry each.
struct Lines {
    lines: BufReader<cosca::PipeReader>,
    /// Dropped with this, which ends the `journalctl` if it still runs.
    child: cosca::Child,
    /// What the child wrote on stderr, which is only read for the error of a failed exit.
    stderr: std::fs::File,
    command: String,
    ended: bool,
}

impl Entries for Lines {
    fn next(&mut self) -> Result<Option<Entry>> {
        if self.ended {
            return Ok(None);
        }
        let mut line = Vec::new();
        self.lines
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::os("read from `journalctl`", e))?;
        // A line the child did not finish (it was killed mid-write) is no entry: the exit status
        // below says whether that was a failure.
        if line.ends_with(b"\n") {
            return Entry::parse(&line).map(Some);
        }
        self.ended = true;
        self.finish()
    }
}

impl Lines {
    /// The output is at its end: `journalctl` ended `Ok`, or failed with what it said.
    fn finish(&mut self) -> Result<Option<Entry>> {
        let status = self.child.wait().map_err(|e| Error::spawn(self.command.clone(), e))?;
        if status.success() {
            return Ok(None);
        }
        let mut said = Vec::new();
        // The child has exited, so what it wrote is whole.
        let read = self.stderr.rewind().and_then(|()| self.stderr.read_to_end(&mut said));
        if let Err(e) = read {
            return Err(Error::Both {
                first: Box::new(Error::command(self.command.clone(), status, b"")),
                then: Box::new(Error::os("read `journalctl`'s diagnostics", e)),
            });
        }
        Err(Error::command(self.command.clone(), status, &said))
    }
}

// A scripted journal ==================================================================================================

/// A test journal: entries stated newest first, with a count of what was read.
#[cfg(test)]
pub(super) mod fake {
    use std::sync::{Arc, Mutex};

    use super::{Entries, Entry, Journal, Match};
    use crate::error::{Error, Result};

    #[derive(Default)]
    struct State {
        asked: Vec<Vec<String>>,
        read: usize,
    }

    /// Clones share one record, so a test keeps a handle after giving one away.
    #[derive(Clone)]
    pub(in crate::backend::systemd::manager) struct Fake {
        entries: Vec<Entry>,
        /// The query itself fails, before any entry.
        unreadable: bool,
        /// The read fails after the entries.
        then_fails: bool,
        state: Arc<Mutex<State>>,
    }

    impl Fake {
        pub(in crate::backend::systemd::manager) fn new(entries: Vec<Entry>) -> Fake {
            Fake {
                entries,
                unreadable: false,
                then_fails: false,
                state: Arc::default(),
            }
        }

        /// A journal no query can read.
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

        /// The matches of every query so far, as `journalctl` takes them.
        pub(in crate::backend::systemd::manager) fn asked(&self) -> Vec<Vec<String>> {
            self.state.lock().unwrap().asked.clone()
        }

        /// How many entries the queries have been given.
        pub(in crate::backend::systemd::manager) fn read(&self) -> usize {
            self.state.lock().unwrap().read
        }
    }

    impl Journal for Fake {
        fn newest_first(&self, matches: &[Match]) -> Result<Box<dyn Entries>> {
            self.state
                .lock()
                .unwrap()
                .asked
                .push(matches.iter().map(Match::argument).collect());
            if self.unreadable {
                return Err(Error::os("read the journal", std::io::Error::other("no journal")));
            }
            Ok(Box::new(Scripted {
                entries: self.entries.clone().into_iter(),
                then_fails: self.then_fails,
                state: Arc::clone(&self.state),
            }))
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
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod journal_tests;
