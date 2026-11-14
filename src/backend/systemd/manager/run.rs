//! A task's run, followed through the system journal: the run its start request produced, what it
//! wrote and how it ended.
//!
//! The measured facts it rests on are in the knowledge base ("A start request's job number leads to
//! its run's invocation, output and exit in the journal", "A run's terminal journal entry precedes
//! most of its output when it prints a lot", "Output of a fast-exiting run can lack
//! _SYSTEMD_INVOCATION_ID", "A start refused by the start limit has no Starting entry"). In short:
//!
//! - The start request's job number finds the run: the "Starting" entry with that `JOB_ID` has its
//!   `INVOCATION_ID` and cursor. A job that ends with no Starting entry never began a run, and its
//!   entries carry the *previous* run's invocation, which is never used.
//! - The exit is the first terminal entry ([`exit_of`]) of that invocation after the cursor.
//! - Output is selected by unit and transport after the cursor, never by invocation: a fast run's
//!   lines can lack `_SYSTEMD_INVOCATION_ID`. Runs of one oneshot unit do not overlap, so the next
//!   run's Starting entry ends the read.
//! - The terminal entry can precede most of a chatty run's output, so once the run has ended the
//!   output's follower is stopped, the journal is synced, and the rest is read after the last line
//!   shown.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::Receiver;

use super::journal::{Cursor, Entry, Follower, Journal, Match, STARTING, exit_of, lacks};
use super::systemctl::Held;
use super::task::Fragment;
use super::{Systemd, UnitKind, unit_name};
use crate::backend::{InstalledService as _, OutputSource, RunHandle};
use crate::error::{Error, Result};
use crate::manager::{Exit, output};
use crate::spec::Id;

// begin_run ===========================================================================================================

impl Systemd {
    /// Requests a run of the task `snapshot` is and finds it in the journal.
    ///
    /// In this order: the length of the task's `logs` file is measured, if it has one; the journal is
    /// synced, so that a previous run's late output is stored before this run's Starting entry; the
    /// request is sent; and the run is found by the job number the request announced. A failure before
    /// the request is sent is its own error and sends nothing. After it, every failure is
    /// [`Error::RequestInDoubt`] with `reached`, except the determinate [`Error::RunNotStarted`] of a
    /// job that ended before any run began.
    ///
    /// On systemd, `goetia daemon run` finds its run, the run's output and its exit in the system
    /// journal. On a host whose journal does not keep the service manager's messages (journald off,
    /// `Storage=none`, or `MaxLevelStore=` below `info`), it waits until interrupted; the run itself
    /// still happens. There, `status` shows state and next run but not the last run or its result; use
    /// `start`. This is not guarded.
    pub(super) fn begin_run(&self, snapshot: Fragment, reservation: &mut Held) -> Result<Run> {
        let id = snapshot.id().clone();
        let unit = unit_name(id.as_str(), UnitKind::Service);
        let blob = snapshot.blob().map_err(|e| Error::Kept(Arc::clone(e)))?;
        let logs = match &blob.spec.common().logs {
            Some(path) => Some((path.clone(), length(path)?)),
            None => None,
        };
        self.journal.sync()?;
        let job = self.systemctl.start_job(id.as_str(), reservation)?;

        let (invocation, start) =
            find_start(self.journal.as_ref(), &id, &unit, job).map_err(|e| after_the_request(&unit, e))?;
        Ok(Run {
            id,
            unit,
            invocation,
            start,
            logs,
            journal: Arc::clone(&self.journal),
        })
    }
}

/// The length of `path`, which is where a run's output to it begins; a file that is not there is
/// empty.
fn length(path: &Path) -> Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(Error::os_at("read the length of", path, e)),
    }
}

/// The invocation and cursor of the run job `job` of `unit` began, from the first entry of the job
/// that says anything about it: its Starting entry, or the end of a job that never began a run.
fn find_start(journal: &dyn Journal, id: &Id, unit: &str, job: u64) -> Result<(String, Cursor)> {
    let mut follower = journal.follow(&[Match::new("UNIT", unit), Match::new("JOB_ID", job.to_string())], None)?;
    loop {
        let Some(entry) = follower.next()? else {
            return Err(lacks("the Starting entry of the run: the journal ended"));
        };
        if entry.get("MESSAGE_ID") == Some(STARTING) {
            let invocation = entry.get("INVOCATION_ID").ok_or_else(|| lacks("an invocation id"))?;
            return Ok((invocation.to_string(), entry.cursor()?));
        }
        if let Some(result) = entry.get("JOB_RESULT") {
            let message = entry
                .bytes("MESSAGE")
                .map(|message| format!(": {}", String::from_utf8_lossy(message)))
                .unwrap_or_default();
            return Err(Error::RunNotStarted {
                id: id.to_string(),
                reason: format!("systemd ended the start job as `{result}`{message}"),
            });
        }
    }
}

/// `e`, a failure to follow the run after the request was sent: the request reached the manager, and
/// the run may have happened.
fn after_the_request(unit: &str, e: Error) -> Error {
    match e {
        e @ Error::RunNotStarted { .. } => e,
        e => Error::RequestInDoubt {
            request: format!("systemctl start {unit}"),
            reached: true,
            note: Some("its run could not be followed in the journal".to_string()),
            source: Box::new(e),
        },
    }
}

// Run =================================================================================================================

/// A run of a task that [`Systemd::begin_run`] found.
pub(crate) struct Run {
    id: Id,
    unit: String,
    /// The run's `INVOCATION_ID`, from its Starting entry.
    invocation: String,
    /// The cursor of that entry: everything of the run is after it.
    start: Cursor,
    /// The task's `logs` file and its length before the request, where it has one.
    logs: Option<(PathBuf, u64)>,
    journal: Arc<dyn Journal>,
}

impl RunHandle for Run {
    fn output(&self) -> OutputSource {
        match &self.logs {
            Some((path, offset)) => OutputSource::LogFile {
                path: path.clone(),
                offset: *offset,
            },
            None => OutputSource::Journal(JournalOutput {
                unit: self.unit.clone(),
                cursor: self.start.clone(),
                journal: Arc::clone(&self.journal),
            }),
        }
    }

    /// Follows the manager's entries about the unit after the run's start to the run's terminal entry,
    /// and decodes the exit from it. The follower is dropped when it returns.
    fn wait(self) -> Result<Exit> {
        self.exit().map_err(|e| Error::RunUnrecorded {
            id: self.id.to_string(),
            reason: e.report().to_string(),
        })
    }
}

impl Run {
    fn exit(&self) -> Result<Exit> {
        let mut follower = self
            .journal
            .follow(&[Match::new("UNIT", self.unit.as_str())], Some(&self.start))?;
        loop {
            let Some(entry) = follower.next()? else {
                return Err(lacks("the run's terminal entry: the journal ended"));
            };
            if entry.get("INVOCATION_ID") != Some(self.invocation.as_str()) {
                continue;
            }
            if let Some(exit) = exit_of(&entry) {
                return exit;
            }
        }
    }
}

// Output ==============================================================================================================

/// Where a run's output is in the journal: the lines its unit wrote on stdout after the cursor of the
/// run's Starting entry.
pub(crate) struct JournalOutput {
    unit: String,
    cursor: Cursor,
    journal: Arc<dyn Journal>,
}

impl JournalOutput {
    /// Copies the run's output to `out` until the run has ended, which is when `ended` is closed:
    /// each line as it is written, then, once the follower is stopped and the journal synced, the
    /// lines it had not yet delivered. A Starting entry of the unit (the next run's) ends the read.
    ///
    /// A failure ends the stream and is returned.
    pub(crate) fn stream(self, out: &mut dyn Write, ended: &Receiver<()>) -> Result<()> {
        let query = self.query();
        let mut follower = self.journal.follow(&query, Some(&self.cursor))?;
        let stop = follower.stop();
        // Dropped when the live read is over, which releases the thread that waits for the run's end.
        let (finished, watching) = crossbeam_channel::bounded::<()>(0);

        let live = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("goetia run output".to_string())
                .spawn_scoped(scope, || {
                    // Ending the follower is the only thing that wakes a read blocked on it.
                    crossbeam_channel::select! {
                        recv(ended) -> _ => stop(),
                        recv(watching) -> _ => {}
                    }
                })
                .map_err(|source| Error::os("make the thread that ends the journal's follower", source))?;
            let live = self.live(follower.as_mut(), out);
            drop(finished);
            live
        })?;
        match live {
            Live::Ended(last) => self.rest(&last, out),
            Live::NextRun => Ok(()),
        }
    }

    /// `_SYSTEMD_UNIT=<unit> _TRANSPORT=stdout + UNIT=<unit> MESSAGE_ID=<Starting>`: the unit's own
    /// lines, and the manager's entries that start one of its runs.
    fn query(&self) -> Vec<Match> {
        vec![
            Match::new("_SYSTEMD_UNIT", self.unit.as_str()),
            Match::new("_TRANSPORT", "stdout"),
            Match::Or,
            Match::new("UNIT", self.unit.as_str()),
            Match::new("MESSAGE_ID", STARTING),
        ]
    }

    /// Writes each line the follower delivers, until it ends (it was stopped) or a Starting entry
    /// shows the next run began.
    fn live(&self, follower: &mut dyn Follower, out: &mut dyn Write) -> Result<Live> {
        let mut last = self.cursor.clone();
        while let Some(entry) = follower.next()? {
            if is_starting(&entry) {
                return Ok(Live::NextRun);
            }
            last = entry.cursor()?;
            let mut line = Vec::new();
            push_line(&mut line, &entry);
            if !line.is_empty() {
                output::show(out, &line)?;
            }
        }
        Ok(Live::Ended(last))
    }

    /// What the run wrote that the follower had not delivered: after the journal is synced, the lines
    /// after `last`, up to the next run's Starting entry.
    fn rest(&self, last: &Cursor, out: &mut dyn Write) -> Result<()> {
        self.journal.sync()?;
        let mut text = Vec::new();
        for entry in self.journal.read_after(&self.query(), last)? {
            if is_starting(&entry) {
                break;
            }
            push_line(&mut text, &entry);
        }
        if text.is_empty() {
            return Ok(());
        }
        output::show(out, &text)
    }
}

/// How the live read ended.
enum Live {
    /// The follower was stopped: the cursor of the last entry it delivered.
    Ended(Cursor),
    /// The next run began, so this one's output is complete.
    NextRun,
}

fn is_starting(entry: &Entry) -> bool {
    entry.get("MESSAGE_ID") == Some(STARTING)
}

/// `entry`'s line, as the bytes the journal holds, if it has one: an entry with no `MESSAGE` adds
/// nothing.
fn push_line(into: &mut Vec<u8>, entry: &Entry) {
    if let Some(message) = entry.bytes("MESSAGE") {
        into.extend_from_slice(message);
        into.push(b'\n');
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod run_tests;
