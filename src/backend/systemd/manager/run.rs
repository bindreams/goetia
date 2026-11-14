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

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};

use super::journal::{Cursor, Entry, Follower, Journal, Match, STARTING, Stop, exit_of, lacks};
use super::systemctl::Held;
use super::task::Fragment;
use super::{Systemd, UnitKind, unit_name};
use crate::backend::bounded::Reservation;
use crate::backend::standby::Standby;
use crate::backend::{InstalledService as _, OutputSource, RunHandle};
use crate::error::{Error, Result};
use crate::manager::{Exit, output};
use crate::spec::Id;

// begin_run ===========================================================================================================

impl Systemd {
    /// Requests a run of the task `snapshot` is and finds it in the journal.
    ///
    /// In this order: what the run will need once the request is sent is made ([`Spares`]); the
    /// length of the task's `logs` file is measured, if it has one; the journal is synced, so that a
    /// previous run's late output is stored before this run's Starting entry; the request is sent;
    /// and the run is found by the job number the request announced. A failure before the request is
    /// sent is its own error and sends nothing. After it, every failure is [`Error::RequestInDoubt`]
    /// with `reached`, except the determinate [`Error::RunNotStarted`] of a job that ended before any
    /// run began.
    ///
    /// Where journald is not running, `journalctl --sync` fails and so does this, before anything is
    /// sent. Otherwise this is the stated contract (not guarded): On systemd, `goetia daemon run`
    /// finds its run, the run's output and its exit in the system journal. On a host whose journal
    /// does not keep the service manager's messages (`Storage=none`, or `MaxLevelStore=` below
    /// `info`), it waits until interrupted; the run itself still happens. There, `status` shows state
    /// and next run but not the last run or its result; use `start`.
    pub(super) fn begin_run(&self, snapshot: Fragment, reservation: &mut Held) -> Result<Run> {
        let id = snapshot.id().clone();
        let unit = unit_name(id.as_str(), UnitKind::Service);
        let blob = snapshot.blob().map_err(|e| Error::Kept(Arc::clone(e)))?;
        let logs = blob.spec.common().logs.clone();
        let mut spares = Spares::make(&id, logs.is_none())?;
        let logs = match logs {
            Some(path) => {
                let offset = length(&path)?;
                Some((path, offset))
            }
            None => None,
        };
        self.journal.sync()?;
        let job = self.systemctl.start_job(id.as_str(), reservation)?;

        let (invocation, start) = find_start(self.journal.as_ref(), &id, &unit, job, &mut spares.start)
            .map_err(|e| after_the_request(&unit, e))?;
        Ok(Run {
            id,
            unit,
            invocation,
            start,
            logs,
            draws: spares.exit,
            output: Cell::new(spares.output),
            journal: Arc::clone(&self.journal),
        })
    }
}

// Spares ==============================================================================================================

/// What a run needs once its request is sent, made before it is: a request whose follow-up could not
/// be made must not be sent. A `journalctl` child cannot be made ahead and is spawned afterwards (its
/// failure is [`Error::RequestInDoubt`]); the temp file each one writes its diagnostics to, and the
/// thread that ends the output's follower, can be.
///
/// `Held` is not where they come from: `flow::run` reserves for [`Step::Start`](crate::manager::Step),
/// which draws nothing for a journal to follow, so `begin_run` makes these itself with the facilities
/// `Held` is made of: [`Reservation::make`] for the temp files and [`Standby`] for the thread.
struct Spares {
    /// For the follower that finds the run's Starting entry.
    start: Reservation,
    /// For the follower that waits for the run's terminal entry.
    exit: Reservation,
    /// For a run whose output is in the journal, which is one with no `logs` file.
    output: Option<OutputSpares>,
}

/// What [`JournalOutput::stream`] needs: a temp file for its follower and one for the read after it,
/// and the thread that ends the follower.
struct OutputSpares {
    draws: Reservation,
    watcher: Standby<Watch>,
}

impl Spares {
    /// All or none; a failure is [`Error::Reservation`], which sent nothing.
    fn make(id: &Id, journal_output: bool) -> Result<Spares> {
        let failed = |source| Error::Reservation {
            id: Some(id.to_string()),
            source,
        };
        Ok(Spares {
            start: Reservation::make(0, 1).map_err(failed)?,
            exit: Reservation::make(0, 1).map_err(failed)?,
            output: if journal_output {
                Some(OutputSpares {
                    draws: Reservation::make(0, 2).map_err(failed)?,
                    watcher: Standby::new("goetia run output", watch).map_err(failed)?,
                })
            } else {
                None
            },
        })
    }
}

/// What the output's watcher thread is handed.
struct Watch {
    /// Closed when the run has ended.
    ended: Receiver<()>,
    /// Closed when the live read is over.
    finished: Receiver<()>,
    stop: Stop,
    /// Closed when the thread holds no more of the follower.
    done: Sender<()>,
}

/// Ends the follower when the run ends, which is the only thing that wakes a read blocked on it, or
/// does nothing when the live read is over first.
fn watch(share: Watch) {
    let Watch {
        ended,
        finished,
        stop,
        done,
    } = share;
    crossbeam_channel::select! {
        recv(ended) -> _ => stop(),
        recv(finished) -> _ => {}
    }
    drop(stop);
    drop(done);
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
fn find_start(
    journal: &dyn Journal,
    id: &Id,
    unit: &str,
    job: u64,
    draws: &mut Reservation,
) -> Result<(String, Cursor)> {
    let mut follower = journal.follow(
        &[Match::new("UNIT", unit), Match::new("JOB_ID", job.to_string())],
        None,
        draws,
    )?;
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
    /// What the wait draws.
    draws: Reservation,
    /// What the output draws, handed to it by [`RunHandle::output`], which is read once.
    output: Cell<Option<OutputSpares>>,
    journal: Arc<dyn Journal>,
}

impl RunHandle for Run {
    fn output(&self) -> OutputSource {
        match &self.logs {
            Some((path, offset)) => OutputSource::LogFile {
                path: path.clone(),
                offset: *offset,
            },
            None => {
                let OutputSpares { draws, watcher } = self
                    .output
                    .take()
                    .expect("a run whose output is in the journal is asked for it once");
                OutputSource::Journal(JournalOutput {
                    unit: self.unit.clone(),
                    cursor: self.start.clone(),
                    journal: Arc::clone(&self.journal),
                    draws,
                    watcher,
                })
            }
        }
    }

    /// Follows the manager's entries about the unit after the run's start to the run's terminal entry,
    /// and decodes the exit from it. The follower is dropped when it returns.
    fn wait(mut self) -> Result<Exit> {
        self.exit().map_err(|e| Error::RunUnrecorded {
            id: self.id.to_string(),
            reason: e.report().to_string(),
        })
    }
}

impl Run {
    fn exit(&mut self) -> Result<Exit> {
        let mut follower = self.journal.follow(
            &[Match::new("UNIT", self.unit.as_str())],
            Some(&self.start),
            &mut self.draws,
        )?;
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
    draws: Reservation,
    watcher: Standby<Watch>,
}

impl JournalOutput {
    /// Copies the run's output to `out` until the run has ended, which is when `ended` is closed:
    /// each line as it is written, then, once the follower is stopped and the journal synced, the
    /// lines it had not yet delivered. A Starting entry of the unit (the next run's) ends the read.
    ///
    /// A failure ends the stream and is returned.
    pub(crate) fn stream(self, out: &mut dyn Write, ended: &Receiver<()>) -> Result<()> {
        let JournalOutput {
            unit,
            cursor,
            journal,
            mut draws,
            watcher,
        } = self;
        let query = query(&unit);
        let mut follower = journal.follow(&query, Some(&cursor), &mut draws)?;
        // Closed when the live read is over, which releases the thread that waits for the run's end.
        let (finished, watching) = crossbeam_channel::bounded::<()>(0);
        let (done, watcher_done) = crossbeam_channel::bounded::<()>(0);
        let handed = watcher.hand(Watch {
            ended: ended.clone(),
            finished: watching,
            stop: follower.stop(),
            done,
        });
        debug_assert!(
            handed.is_ok(),
            "the output's watcher thread ended before it was handed its share"
        );

        let live = live(&cursor, follower.as_mut(), out);
        drop(finished);
        // Nothing is sent: the watcher closes this when it holds no more of the follower, so that the
        // child is gone when the follower is dropped below.
        while watcher_done.recv().is_ok() {}
        drop(follower);
        match live? {
            Live::Ended(last) => rest(journal.as_ref(), &query, &mut draws, &last, out),
            Live::NextRun => Ok(()),
        }
    }
}

/// `_SYSTEMD_UNIT=<unit> _TRANSPORT=stdout + UNIT=<unit> MESSAGE_ID=<Starting>`: the unit's own
/// lines, and the manager's entries that start one of its runs.
fn query(unit: &str) -> Vec<Match> {
    vec![
        Match::new("_SYSTEMD_UNIT", unit),
        Match::new("_TRANSPORT", "stdout"),
        Match::Or,
        Match::new("UNIT", unit),
        Match::new("MESSAGE_ID", STARTING),
    ]
}

/// Writes each line the follower delivers, until it ends (it was stopped) or a Starting entry shows
/// the next run began. `from` is where the read began.
fn live(from: &Cursor, follower: &mut dyn Follower, out: &mut dyn Write) -> Result<Live> {
    let mut last = from.clone();
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
fn rest(
    journal: &dyn Journal,
    query: &[Match],
    draws: &mut Reservation,
    last: &Cursor,
    out: &mut dyn Write,
) -> Result<()> {
    journal.sync()?;
    let mut text = Vec::new();
    for entry in journal.read_after(query, last, draws)? {
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
