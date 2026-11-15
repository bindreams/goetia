//! A task's `run`: starts one run of the job and follows the process it is.
//!
//! launchd names a run's process only once it is running, and a process that exits at once is gone before
//! anything can be registered for its exit (measured). So `run` holds the launch: `launchctl debug
//! --start-suspended` makes the job's next launch start stopped, `kickstart -p` starts it and names the
//! pid, and the pid is watched ([`Watch`]) before `SIGCONT`, by label, lets it go. The exit status comes
//! from the watch, and the output, where the task has `logs`, from the file after the length it had while
//! the launch was held: that length is before everything this run writes, and the first marker after it is
//! this run's.
//!
//! `kickstart -p` names the pid of the job's process, which is the launch `debug` held only if no other
//! launch got there first: a scheduled run can be in flight, or can have taken a pending hold. So a pass
//! tells what the pid is from the kernel, not from `print`, which lags the launch ([`super::proc`]). The
//! pid is registered for its exec and exit; where its program is still `xpcproxy` its first exec is
//! waited for, and then its state and start time are read. With `ta` the time read after the `launchctl
//! debug` process was reaped:
//!
//! | process | started | the launch is | `run` |
//! | --- | --- | --- | --- |
//! | stopped | after `ta` | the one `debug` held | resumes it, and follows it |
//! | stopped | at or before `ta` | an older one that took a hold | resumes it, waits for its exit, starts over |
//! | running | at or before `ta` | a run in flight | waits for its exit, starts over |
//! | running | after `ta` | our own `kickstart`'s, with no hold | [`run_unfollowed`] |
//! | gone | | | starts over |
//!
//! Starting over is another pass with a fresh `debug`: a hold the older launch took is gone, and one it
//! did not take is still pending and the `debug` is refused as pending (exit 37), which is the same hold.
//! No pass count is limited: a pass ends in a launch followed, or in a launch some other `run` or the
//! timer made that has now ended. Each pass reserves before its first request.
//!
//! A launch that took a hold before launchd handled our `debug` started before `debug` returned, so its
//! start is at or before `ta`; one of ours is after it. Measured (macOS CI, root): 0 of 3957 older launches
//! started after `ta`, and 1850 of 1850 of ours did, by at least 4.4 ms (KB, "macOS - A launch that took a
//! hold before our debug can have a start time inside the debug call"). That an older launch never starts
//! after `ta` is this measured fact and not something launchd promises.
//!
//! From before the job is loaded to the `SIGCONT`, SIGINT, SIGTERM and SIGHUP are held back
//! ([`Deferred`]): ending goetia after the `debug` would leave a launch held and stopped. The waits for
//! another launch's exit and for `xpcproxy`'s exec end on an interrupt instead, and a pass that follows
//! one starts nothing: it is [`stranded_hold`]. A failure in the window carries it too: a launch that took
//! the hold waits stopped until `goetia daemon stop` ends it, and a hold no launch has taken is consumed
//! by the next `goetia daemon run`.

use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;

use super::interrupts::Deferred;
use super::proc;
use super::watch::{Added, Registered, Watch, Woke};
use super::{Kickstarted, Launchd, Plist, SignalOutcome, never_expires};
use crate::backend::bounded::Reservation;
use crate::backend::launchd::generate::run_marker;
use crate::backend::launchd::refuse::{run_interrupted, run_lost, run_unfollowed, run_unresumed, stranded_hold};
use crate::backend::launchd::state::{Evidence, Launch, exit_of, launch_of};
use crate::backend::{OutputSource, RunHandle};
use crate::error::{Error, Result};
use crate::manager::{Budget, Exit, Step, flow, output};
use crate::spec::Id;

/// A run of a task that [`Launchd::begin_task_run`] resumed: the process it is, watched.
pub(crate) struct Run {
    id: Id,
    pid: u32,
    /// The pid, registered for its exit before it was resumed.
    registered: Registered,
    /// The task's `logs` file and its length while the launch was held, where it has one.
    logs: Option<(PathBuf, u64)>,
}

impl RunHandle for Run {
    fn output(&self) -> OutputSource {
        match &self.logs {
            Some((path, offset)) => OutputSource::LogFile {
                path: path.clone(),
                offset: *offset,
                marker: Some(run_marker(self.pid)),
            },
            None => OutputSource::None,
        }
    }

    /// Blocks on the watch, under no bound: the run lasts as long as it lasts. A wait that failed has lost
    /// the run, which may still be going ([`run_lost`]).
    fn wait(self) -> Result<Exit> {
        let status = self.registered.wait(None).map_err(|e| run_lost(&self.id, e))?;
        Ok(exit_of(never_expires(status)))
    }
}

impl Launchd {
    /// Holds the job's next launch, starts it, and follows its pid to the end; see the module doc.
    ///
    /// In this order: the kqueue is made, so that failing to make it holds nothing; interrupts are
    /// deferred; the job is loaded; then passes until one follows a launch. A pass is `debug`, `kickstart
    /// -p`, the registration and the kernel's account of the pid, and then one of: the log's length read
    /// and `SIGCONT` of the held launch, `SIGCONT` of an older launch that took a hold and a wait for its
    /// exit, a wait for a run's exit, or the end of the `run`. The first pass draws on `reservation` (made
    /// for [`Step::Start`], which the longest first pass stays within: the load, `debug`, `kickstart -p`
    /// and `SIGCONT`), each later one on its own. A failure of the first `debug` is its own error; any
    /// later failure is [`stranded_hold`].
    pub(super) fn begin_task_run(
        &self,
        snapshot: Plist,
        reservation: &mut Reservation,
        notes: &mut dyn Write,
    ) -> Result<Run> {
        let path = snapshot.job()?.location.path.clone();
        let Plist { id, blob, .. } = snapshot;
        let logs = blob.map_err(Error::Kept)?.spec.common().logs.clone();
        let watch = Watch::new_interruptible()?;
        let window = Deferred::begin()?;
        self.launchctl.load_job(id.as_str(), &path, reservation)?;

        let mut attempt = Attempt {
            launchd: self,
            id: &id,
            watch: &watch,
            window: &window,
            logs,
            notes,
        };
        let mut own: Option<Reservation> = None;
        loop {
            let armed = own.is_some();
            let draws = match own.as_mut() {
                Some(own) => own,
                None => &mut *reservation,
            };
            if let Some(run) = attempt.pass(draws, armed)? {
                return Ok(run);
            }
            own =
                Some(flow::reserved(self, &id, &[Step::Start], Budget::Immediate).map_err(|e| stranded_hold(&id, e))?);
        }
    }
}

/// What one `run` holds across its passes.
struct Attempt<'a> {
    launchd: &'a Launchd,
    id: &'a Id,
    watch: &'a Watch,
    window: &'a Deferred,
    logs: Option<PathBuf>,
    notes: &'a mut dyn Write,
}

impl Attempt<'_> {
    /// One pass, which `reservation` covers: `Some` is the launch `debug` held, resumed. `None` is a launch
    /// that is over or has ended and was not this run's, and another pass is to be made. `armed` is whether
    /// an earlier pass armed a hold.
    fn pass(&mut self, reservation: &mut Reservation, armed: bool) -> Result<Option<Run>> {
        let id = self.id;
        let name = id.as_str();
        let strand = |e: Error| stranded_hold(id, e);
        // An interrupt that arrived while an earlier pass waited and its exit came with it: nothing is
        // started after it.
        if armed && self.window.interrupted() {
            return Err(strand(run_interrupted(id, "a scheduled run")));
        }

        let ta = self
            .launchd
            .launchctl
            .debug_start_suspended(name, reservation)
            .map_err(|e| if armed { strand(e) } else { e })?;
        let issuing = Budget::Unbounded.start();
        let Kickstarted::Confirmed(pid) = never_expires(
            self.launchd
                .launchctl
                .kickstart(name, Budget::Unbounded, issuing, Evidence::Queued, reservation)
                .map_err(strand)?,
        ) else {
            return Err(strand(unestablished(
                id,
                "launchctl kickstart -p printed no whole pid".to_string(),
            )));
        };

        // Registered before anything reads the pid: what happens to it from here on is an event.
        let Added::Watching(registered) = self.watch.add_launch(pid).map_err(strand)? else {
            return Ok(None);
        };
        let Some(launch) = self.read_launch(pid, ta, &registered)? else {
            return Ok(None);
        };
        match launch {
            Launch::Held => self.resume(pid, registered, reservation).map(Some),
            Launch::TookHold => {
                // Whether the launch is still the label's does not matter: it is waited for either way.
                self.launchd
                    .launchctl
                    .signal(name, "SIGCONT", reservation)
                    .map_err(strand)?;
                self.wait_for_exit(registered)
            }
            Launch::InFlight => self.wait_for_exit(registered),
            Launch::Unheld => Err(run_unfollowed(id, pid)),
        }
    }

    /// The [`Launch`] `pid` is, from the kernel. Where its program is `xpcproxy` its first exec is waited
    /// for first, under no bound: the exec is certain to come (measured), and from it the process is
    /// stopped if it is held. `None` where the process is gone, or exits before it execs.
    fn read_launch(&self, pid: u32, ta: u64, registered: &Registered) -> Result<Option<Launch>> {
        let id = self.id;
        let strand = |e: Error| stranded_hold(id, e);
        let read = |source: std::io::Error| {
            strand(Error::os(
                format!("read what the kernel shows of process {pid}"),
                source,
            ))
        };
        let Some(path) = proc::path_of(pid).map_err(read)? else {
            return Ok(None);
        };
        if path.file_name() == Some(OsStr::new("xpcproxy")) {
            match registered.first_exec(self.window).map_err(strand)? {
                Woke::Exec => {}
                Woke::Exited(_) => return Ok(None),
                Woke::Interrupted => return Err(strand(run_interrupted(id, "a launch to start"))),
            }
        }
        let Some(process) = proc::read(pid).map_err(read)? else {
            return Ok(None);
        };
        Ok(Some(launch_of(process.stopped, process.started, ta)))
    }

    /// Says a scheduled run is in progress, then waits for the launch `registered` to exit, ending on an
    /// interrupt. `None`: it has ended, and another pass is to be made.
    fn wait_for_exit(&mut self, registered: Registered) -> Result<Option<Run>> {
        let id = self.id;
        self.note();
        match registered.exit(self.window).map_err(|e| stranded_hold(id, e))? {
            Woke::Interrupted => Err(stranded_hold(id, run_interrupted(id, "a scheduled run"))),
            Woke::Exited(_) | Woke::Exec => Ok(None),
        }
    }

    /// Writes the note that a scheduled run is waited for. The run does not depend on it: a note that
    /// cannot be written is a warning, as other failures of goetia's side channels are.
    fn note(&mut self) {
        let note = format!(
            "note: a scheduled run of `{}` is in progress; waiting for it to end before starting this run\n",
            self.id
        );
        if let Err(e) = output::show(self.notes, note.as_bytes()) {
            eprintln!(
                "warning: the note that a scheduled run is being waited for could not be written: {}",
                e.report()
            );
        }
    }

    /// Reads the log's length and resumes the held launch `pid`, which `registered` watches, by label. A
    /// launch that is gone by the `SIGCONT` is [`Error::RunUnrecorded`]: it was ended, and nothing is
    /// stranded. Any other failure is the stranded hold.
    fn resume(&mut self, pid: u32, registered: Registered, reservation: &mut Reservation) -> Result<Run> {
        let id = self.id;
        let strand = |e: Error| stranded_hold(id, e);
        // While the launch is held, launchd has opened the log and the program has written nothing.
        let logs = self
            .logs
            .clone()
            .map(|path| output::length_of(&path).map(|offset| (path, offset)))
            .transpose()
            .map_err(strand)?;
        match self
            .launchd
            .launchctl
            .signal(id.as_str(), "SIGCONT", reservation)
            .map_err(strand)?
        {
            SignalOutcome::Sent => Ok(Run {
                id: id.clone(),
                pid,
                registered,
                logs,
            }),
            SignalOutcome::NoProcess => Err(run_unresumed(id)),
        }
    }
}

/// What `run` could not establish of the launch it started, `why`: the start was accepted and its state is
/// not known, which is not a failure of the operating system.
fn unestablished(id: &Id, why: String) -> Error {
    Error::Step {
        what: why,
        source: Box::new(Error::StartUnconfirmed { id: id.to_string() }),
    }
}

#[cfg(test)]
#[path = "run_tasks_tests.rs"]
mod run_tasks_tests;
