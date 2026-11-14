//! A task's run, followed through the system journal: the run its start request produced, what it
//! wrote and how it ended.
//!
//! The measured facts it rests on are in the knowledge base ("A start request's job number leads to
//! its run's invocation, output and exit in the journal", "A run's terminal journal entry precedes
//! most of its output when it prints a lot", "Output of a fast-exiting run can lack
//! _SYSTEMD_INVOCATION_ID", "A start refused by the start limit has no Starting entry", "A marker
//! line printed by sh before exec shares one _STREAM_ID with the exec'd program's stdout lines", "The
//! INVOCATION_ID that sh prints in a marker line equals the INVOCATION_ID of the run's Starting
//! entry"). In short:
//!
//! - The start request's job number finds the run: the "Starting" entry with that `JOB_ID` has its
//!   `INVOCATION_ID` and cursor. A job that ends with no Starting entry never began a run, and its
//!   entries carry the *previous* run's invocation, which is never used.
//! - The exit is the first terminal entry ([`ending_of`]) of that invocation after the cursor. One that
//!   is "Failed with result" with no exit before it is a run whose exit is not established
//!   ([`Error::RunUnrecorded`]).
//! - Output is the run's stream. A task's `sh` wrapper prints `goetia-run <INVOCATION_ID>` first
//!   (`RUN_MARKER`), and that line and every line of the program share one `_STREAM_ID`, which no
//!   other run's lines carry, and which every stdout line has (`_SYSTEMD_INVOCATION_ID` is missing
//!   from a fast run's lines, so it selects nothing). The unit's stdout lines after the Starting
//!   entry are followed for the marker, and the lines of its stream are the output; the marker is
//!   not shown. Nothing bounds the read at the next run's Starting entry: a run's lines can land
//!   after it.
//! - The terminal entry can precede most of a chatty run's output, so once the run has ended the
//!   output's follower is stopped, the journal is synced, and the rest of the stream is read.
//! - journald's rate limit can drop the marker. Then, and only after the terminal entry, the sync
//!   and one more read of the unit's lines, the run shows a note, and its exit is still the run's.
//! - A task with a `logs` file writes the marker there too, as the first line of each run. The file
//!   is shown from the length it had before the request, from our marker to the next run's
//!   ([`OutputSource::LogFile`]'s `marker`), so a run a timer starts right after ours is not shown. A
//!   request that joined a run begun before that length was measured has its marker earlier: once the
//!   run has ended the file is searched again from byte 0 for it.

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender};

use super::journal::{Cursor, Ending, Entry, Follower, Journal, Match, STARTING, Scope, Stop, ending_of, lacks};
use super::systemctl::Held;
use super::task::Fragment;
use super::{Systemd, UnitKind, unit_name};
use crate::backend::bounded::Reservation;
use crate::backend::standby::Standby;
use crate::backend::systemd::generate::RUN_MARKER;
use crate::backend::{InstalledService as _, Marker, OutputSource, RunHandle};
use crate::error::{Error, Result};
use crate::manager::{Exit, output};
use crate::spec::Id;

// begin_run ===========================================================================================================

impl Systemd {
    /// Requests a run of the task `snapshot` is and finds it in the journal.
    ///
    /// In this order: what the run will need once the request is sent is made ([`Spares`]); the
    /// length of the task's `logs` file is measured, if it has one; the request is sent; and the run
    /// is found by the job number the request announced. A failure before the request is
    /// sent is its own error and sends nothing. After it, every failure is [`Error::RequestInDoubt`]
    /// with `reached`, except the determinate [`Error::RunNotStarted`] of a job that ended before any
    /// run began.
    ///
    /// This is the stated contract (not guarded): On systemd, `goetia daemon run` finds its run, the
    /// run's output and its exit in the system journal. On a host whose journal does not keep the
    /// service manager's messages (`Storage=none`, or `MaxLevelStore=` below `info`), it waits until
    /// interrupted; the run itself still happens. There, `status` shows state and next run but not
    /// the last run or its result; use `start`.
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
        Scope::Manager,
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

/// The digits of an `INVOCATION_ID`: a 128-bit id as 32 lowercase hex digits (`sd_id128_to_string`).
const INVOCATION_ID_DIGITS: usize = 32;

/// The marker line the run of `invocation` writes first, which its `sh` wrapper prints.
fn run_marker(invocation: &str) -> Marker {
    Marker {
        prefix: RUN_MARKER,
        line: format!("{RUN_MARKER} {invocation}"),
        id_len: INVOCATION_ID_DIGITS,
    }
}

impl RunHandle for Run {
    fn output(&self) -> OutputSource {
        match &self.logs {
            Some((path, offset)) => OutputSource::LogFile {
                path: path.clone(),
                offset: *offset,
                marker: Some(run_marker(&self.invocation)),
            },
            None => {
                let OutputSpares { draws, watcher } = self
                    .output
                    .take()
                    .expect("a run whose output is in the journal is asked for it once");
                OutputSource::Journal(JournalOutput {
                    unit: self.unit.clone(),
                    invocation: self.invocation.clone(),
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
    ///
    /// [`Error::RunUnrecorded`] for a terminal entry that does not say how the run ended, and for a
    /// "Failed with result" with no exit before it. Any
    /// other failure (the follower could not be made, failed or ended) loses track of the run, which
    /// may still be going: it is [`Error::RequestInDoubt`], as every failure to follow a run is.
    fn wait(mut self) -> Result<Exit> {
        self.exit()
    }
}

impl Run {
    fn exit(&mut self) -> Result<Exit> {
        let lost = |e| after_the_request(&self.unit, e);
        let mut follower = self
            .journal
            .follow(
                Scope::Manager,
                &[Match::new("UNIT", self.unit.as_str())],
                Some(&self.start),
                &mut self.draws,
            )
            .map_err(lost)?;
        loop {
            let Some(entry) = follower.next().map_err(lost)? else {
                return Err(lost(lacks("the run's terminal entry: the journal ended")));
            };
            if entry.get("INVOCATION_ID") != Some(self.invocation.as_str()) {
                continue;
            }
            match ending_of(&entry) {
                None => {}
                Some(Ending::Exit(exit)) => {
                    return exit.map_err(|e| Error::RunUnrecorded {
                        id: self.id.to_string(),
                        reason: e.report().to_string(),
                    });
                }
                Some(Ending::NoExit { result }) => {
                    return Err(Error::RunUnrecorded {
                        id: self.id.to_string(),
                        reason: format!("systemd failed the unit as `{result}` and recorded no exit for the run"),
                    });
                }
            }
        }
    }
}

// Output ==============================================================================================================

/// What `stream` writes when the run's marker never reached the journal: the run's lines cannot be
/// told from another run's, so none is shown.
const NO_MARKER: &str = "note: the marker line of this run is not in the journal, so its output is not shown: \
     journald drops a unit's lines above its rate limit (`RateLimitBurst` per `RateLimitIntervalSec`, per \
     unit). To lift the limit for the whole host, set `RateLimitBurst=0` under `[Journal]` in a \
     journald.conf.d drop-in and restart systemd-journald\n";

/// Where a run's output is in the journal: the lines of the stream its marker line is in, which is
/// found among the unit's stdout lines after the cursor of the run's Starting entry.
pub(crate) struct JournalOutput {
    unit: String,
    /// The run's `INVOCATION_ID`, which its marker line names.
    invocation: String,
    cursor: Cursor,
    journal: Arc<dyn Journal>,
    draws: Reservation,
    watcher: Standby<Watch>,
}

impl JournalOutput {
    /// Copies the run's output to `out` until the run has ended, which is when `ended` is closed:
    /// each line as it is written, then, once the follower is stopped and the journal synced, the
    /// lines of the stream it had not yet delivered. The marker line is not shown.
    ///
    /// A run whose marker is not in the journal after all that (journald's rate limit dropped it)
    /// shows a note instead of lines ([`NO_MARKER`]); that is no failure. Any other failure ends the
    /// stream and is returned.
    pub(crate) fn stream(self, out: &mut dyn Write, ended: &Receiver<()>, exit_recorded: &AtomicBool) -> Result<()> {
        let JournalOutput {
            unit,
            invocation,
            cursor,
            journal,
            mut draws,
            watcher,
        } = self;
        let query = stdout_query(&unit);
        let mut follower = journal.follow(Scope::Output, &query, Some(&cursor), &mut draws)?;
        // Closed when the live read is over, which releases the thread that waits for the run's end.
        let (finished, watching) = crossbeam_channel::bounded::<()>(0);
        let (done, watcher_done) = crossbeam_channel::bounded::<()>(0);
        // Nothing but the watcher wakes a read blocked on the follower, so with no watcher there is
        // no read: the follower is dropped on this return, which ends its `journalctl`.
        if watcher
            .hand(Watch {
                ended: ended.clone(),
                finished: watching,
                stop: follower.stop(),
                done,
            })
            .is_err()
        {
            return Err(Error::os(
                "follow the run's output",
                std::io::Error::other("the thread that ends the read when the run ends is gone"),
            ));
        }

        let mut selection = Selection::new(&invocation);
        let live = live(&mut selection, follower.as_mut(), out);
        drop(finished);
        // Nothing is sent: the watcher closes this when it holds no more of the follower, so that the
        // child is gone when the follower is dropped below.
        while watcher_done.recv().is_ok() {}
        drop(follower);
        live?;
        rest(
            journal.as_ref(),
            &query,
            &cursor,
            selection,
            &mut draws,
            out,
            exit_recorded.load(Ordering::SeqCst),
        )
    }
}

/// `_SYSTEMD_UNIT=<unit> _TRANSPORT=stdout`: the unit's own lines, of every run it has.
fn stdout_query(unit: &str) -> Vec<Match> {
    vec![Match::new("_SYSTEMD_UNIT", unit), Match::new("_TRANSPORT", "stdout")]
}

/// Which of a unit's stdout lines are one run's: the lines of the stream its marker line is in. A run
/// has one stream, which its marker is the first line of, and no other run's lines are in it.
struct Selection {
    /// The marker line's `MESSAGE`.
    marker: Vec<u8>,
    /// Once the marker is found: its stream, and the cursor of the last entry of that stream seen.
    found: Option<(String, Cursor)>,
}

impl Selection {
    fn new(invocation: &str) -> Selection {
        Selection {
            marker: run_marker(invocation).line.into_bytes(),
            found: None,
        }
    }

    /// Takes the next of the unit's lines, in journal order: adds to `into` what of it is the run's
    /// output. Before the marker nothing is the run's; the marker is the first line with its text and
    /// is not output; after it every line of its stream is, the same text again included.
    fn take(&mut self, entry: &Entry, into: &mut Vec<u8>) -> Result<()> {
        match &mut self.found {
            None if entry.bytes("MESSAGE") == Some(self.marker.as_slice()) => {
                let stream = entry
                    .get("_STREAM_ID")
                    .ok_or_else(|| lacks("the stream of the run's marker line"))?;
                self.found = Some((stream.to_string(), entry.cursor()?));
            }
            None => {}
            Some((stream, last)) => {
                if entry.get("_STREAM_ID") == Some(stream.as_str()) {
                    *last = entry.cursor()?;
                    push_line(into, entry);
                }
            }
        }
        Ok(())
    }
}

/// Writes each line of the run the follower delivers, until it ends (it was stopped).
fn live(selection: &mut Selection, follower: &mut dyn Follower, out: &mut dyn Write) -> Result<()> {
    while let Some(entry) = follower.next()? {
        let mut line = Vec::new();
        selection.take(&entry, &mut line)?;
        if !line.is_empty() {
            output::show(out, &line)?;
        }
    }
    Ok(())
}

/// What the run wrote that the follower had not delivered, after the journal is synced: the rest of
/// the marker's stream after its last line shown, or, when the follower never met the marker, the
/// unit's lines after `from` searched for it once more. With none, the note ([`NO_MARKER`]).
fn rest(
    journal: &dyn Journal,
    query: &[Match],
    from: &Cursor,
    mut selection: Selection,
    draws: &mut Reservation,
    out: &mut dyn Write,
    exit_recorded: bool,
) -> Result<()> {
    journal.sync()?;
    let entries = match &selection.found {
        Some((stream, last)) => {
            journal.read_after(Scope::Output, &[Match::new("_STREAM_ID", stream.as_str())], last, draws)?
        }
        None => journal.read_after(Scope::Output, query, from, draws)?,
    };
    let mut text = Vec::new();
    for entry in &entries {
        selection.take(entry, &mut text)?;
    }
    if selection.found.is_none() {
        // A wait that found no exit is reported as its own error, not with a note beside it.
        return if exit_recorded {
            output::show(out, NO_MARKER.as_bytes())
        } else {
            Ok(())
        };
    }
    if text.is_empty() {
        return Ok(());
    }
    output::show(out, &text)
}

/// `entry`'s line, as the bytes the journal holds, if it has one, ended as the program ended it: an
/// entry has no newline of its own, and journald says in `_LINE_BREAK` where a line ended otherwise
/// (knowledge base: "journald marks a stdout entry that did not end at a newline"). A last line with
/// no newline (`eof`), a piece of a line cut at `LineMax` (`line-max`) and a line left unterminated
/// when another process wrote next (`pid-change`) end with no byte; a line ended by a NUL byte
/// (`nul`) ends with it; any other entry, with no field or one not known, ends with a newline. An
/// entry with no `MESSAGE` adds nothing.
fn push_line(into: &mut Vec<u8>, entry: &Entry) {
    if let Some(message) = entry.bytes("MESSAGE") {
        into.extend_from_slice(message);
        match entry.get("_LINE_BREAK") {
            Some("eof" | "line-max" | "pid-change") => {}
            Some("nul") => into.push(0),
            _ => into.push(b'\n'),
        }
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod run_tests;
