//! The system journal, read through `journalctl`: what the manager wrote about a task's runs, and the
//! output a run wrote.
//!
//! A seam of its own ([`Journal`]) beside [`Client`](crate::backend::client::Client), which runs a
//! program to completion or a deadline and has no way to hand over what a program writes as it is
//! written. Every query passes `-a` (a `MESSAGE` of 4088 bytes or more is `null` in JSON without it)
//! and `-o json`; a field is a string, or an array of byte numbers where it is not UTF-8, and both
//! decode, to the bytes they are.
//!
//! Three reads: newest first (`status` asks it), following (the start's identity, a run's exit and its
//! output), and after a cursor (what a run's output still holds once its follower is stopped). A run's
//! entries (what the manager wrote when it started, and how its main process ended) are the
//! `MESSAGE_ID`s below, measured on systemd 255 (knowledge base: "A oneshot run ends in a different
//! journal entry per outcome").

mod journalctl;

#[cfg(test)]
pub(super) mod fake;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::bounded::Reservation;
use crate::error::{Error, Result};
use crate::manager::Exit;

pub(super) use journalctl::Journalctl;

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

/// What `journalctl` is asked to read: one `FIELD=value` match. Only entries holding every match of
/// a query are read.
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

    /// Whether this is a match on the field `field`.
    fn is(&self, field: &str) -> bool {
        self.name == field
    }
}

/// Where in the journal an entry is: its `__CURSOR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cursor(String);

impl Cursor {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(super) fn of(cursor: &str) -> Cursor {
        Cursor(cursor.to_string())
    }
}

/// One journal entry: its fields, each as the bytes the journal holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    fields: BTreeMap<String, Vec<u8>>,
}

impl Entry {
    /// The field `name` as text, if the entry has it and it is UTF-8. Every field goetia reads to
    /// identify an entry is.
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.bytes(name).and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    /// The field `name` as the journal holds it.
    pub(super) fn bytes(&self, name: &str) -> Option<&[u8]> {
        self.fields.get(name).map(Vec::as_slice)
    }

    /// When the entry was written: its `__REALTIME_TIMESTAMP`, in microseconds.
    pub(super) fn realtime(&self) -> Result<SystemTime> {
        self.get("__REALTIME_TIMESTAMP")
            .and_then(|micros| micros.parse::<u64>().ok())
            .map(|micros| UNIX_EPOCH + Duration::from_micros(micros))
            .ok_or_else(|| lacks("a time"))
    }

    /// Where the entry is in the journal.
    pub(super) fn cursor(&self) -> Result<Cursor> {
        self.get("__CURSOR")
            .map(|cursor| Cursor(cursor.to_string()))
            .ok_or_else(|| lacks("a cursor"))
    }

    /// One line of `journalctl -o json`.
    pub(super) fn parse(line: &[u8]) -> Result<Entry> {
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|e| Error::os("read an entry of the journal's JSON", std::io::Error::other(e)))?;
        let serde_json::Value::Object(object) = value else {
            return Err(lacks("an object"));
        };
        let fields = object
            .into_iter()
            .filter_map(|(name, value)| field_bytes(&value).map(|bytes| (name, bytes)))
            .collect();
        Ok(Entry { fields })
    }

    #[cfg(test)]
    pub(super) fn of(fields: &[(&str, &str)]) -> Entry {
        Entry {
            fields: fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                .collect(),
        }
    }
}

/// A field's value as bytes: a string as it is, an array of byte numbers (a value that is not UTF-8,
/// or has a control character) as the bytes it is, and the first of an array of values (a field the
/// entry holds more than once). `null` is no field.
fn field_bytes(value: &serde_json::Value) -> Option<Vec<u8>> {
    use serde_json::Value;
    match value {
        Value::String(text) => Some(text.as_bytes().to_vec()),
        Value::Array(items) if items.iter().all(Value::is_number) => items
            .iter()
            .map(|n| n.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect(),
        Value::Array(items) => items.first().and_then(field_bytes),
        _ => None,
    }
}

/// An answer of `journalctl` without `what`.
pub(super) fn lacks(what: &str) -> Error {
    Error::IncompleteAnswer {
        command: "journalctl".to_string(),
        missing: vec![what.to_string()],
    }
}

// Journal =============================================================================================================

/// The system journal, as the systemd backend reads it.
///
/// Queries read the system journal only (`journalctl --system`). Without it an account outside
/// `systemd-journal` that has a user journal of its own opens that file and gets exit 0 and no entry:
/// an empty answer that cannot be told from a unit that never ran. With it such an account opens no
/// file, and `journalctl` fails ("No journal files were opened due to insufficient permissions"),
/// which is the error a journal that cannot be read must be. A host whose journal holds no files
/// still answers with nothing (exit 0).
pub(super) trait Journal: Send + Sync {
    /// The entries holding every one of `matches`, newest first. `Err` where the journal cannot be
    /// read at all, which is never an empty answer: a host with no journal files answers with none.
    fn newest_first(&self, matches: &[Match]) -> Result<Box<dyn Entries>>;

    /// The entries holding `matches`, oldest first, as they are written: every one there is from the
    /// start of the boot (or, with `after`, from the entry after it), then each new one as it
    /// arrives. It ends only when stopped ([`Follower::stop`]) or dropped; a query on `JOB_ID` is
    /// bound to the current boot, since job numbers restart with it. The `journalctl` writes its
    /// diagnostics to a temp file drawn from `draws`, which the caller made before it sent anything.
    fn follow(&self, matches: &[Match], after: Option<&Cursor>, draws: &mut Reservation) -> Result<Box<dyn Follower>>;

    /// Every entry holding `matches` after `cursor`, once the journal holds everything written
    /// before the call ([`Journal::sync`] is the caller's to have made). Draws a temp file as
    /// [`Journal::follow`] does.
    fn read_after(&self, matches: &[Match], cursor: &Cursor, draws: &mut Reservation) -> Result<Vec<Entry>>;

    /// Waits until journald has stored everything sent to it before the call (`journalctl --sync`).
    fn sync(&self) -> Result<()>;
}

/// The entries of one query, as they are read. Dropping it ends the `journalctl` behind it.
pub(super) trait Entries: Send {
    /// The next entry, `None` once the query has no more. `Err` where the read failed, which is not
    /// the end of the entries.
    fn next(&mut self) -> Result<Option<Entry>>;
}

/// The entries of a query that does not end on its own.
pub(super) trait Follower: Entries {
    /// What ends this follower from any thread: after it, `next` gives what was already read and
    /// then `None`. Idempotent.
    fn stop(&self) -> Stop;
}

/// See [`Follower::stop`].
pub(super) type Stop = Arc<dyn Fn() + Send + Sync>;

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

#[cfg(test)]
#[path = "journal_tests.rs"]
mod journal_tests;
