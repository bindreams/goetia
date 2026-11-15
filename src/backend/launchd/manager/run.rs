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
//! A scheduled run can slip in between `run`'s read of the state and its `debug`: `kickstart -p` then
//! names the run already in flight, and the hold stays pending. `run` waits for that run to end, then
//! starts again without `debug`: the pending hold is the one the next launch takes, and arming it again
//! after a timer fire took it would leave a second hold for nobody to resume. The retry follows a launch
//! that is held. One that ended, or is alive and not held, is [`run_unfollowed`]: after a pending hold the
//! next launch is held, so a second slipped-in run cannot be what `kickstart -p` names.
//!
//! From the `debug` to the `SIGCONT`, SIGINT, SIGTERM and SIGHUP are held back ([`Deferred`]): ending
//! goetia there would leave a launch held and stopped. So `run` cannot be interrupted by those three while
//! it waits for a slipped-in run. A failure in that window carries [`stranded_hold`]: the hold, or the
//! launch it holds, stays until a `stop`, a boot-out or a later `run` ends it.

use std::io::Write;
use std::path::PathBuf;

use super::interrupts::Deferred;
use super::task::{Shown, TaskPrint};
use super::watch::{Added, Registered, Watch};
use super::{Kickstarted, Launchd, Plist, SignalOutcome, never_expires};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::launchd::generate::run_marker;
use crate::backend::launchd::refuse::{run_lost, run_unfollowed, run_unresumed, run_unwatched, stranded_hold};
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

/// What `kickstart -p` and `print` showed of the launch [`Launchd::started`] made.
enum StartedOutcome {
    /// The launch of this pid is held and `print` shows it.
    Held(u32),
    /// The pid `kickstart -p` named ended, or was a run already in flight that has now ended.
    Ended(u32),
    /// The pid is a run already in flight, not held, and still alive after it was watched.
    InFlight(u32, Registered),
}

impl Launchd {
    /// Holds the job's next launch, starts it, and follows its pid to the end; see the module doc.
    ///
    /// In this order: the kqueue is made, so that failing to make it holds nothing; interrupts are
    /// deferred; the job is loaded; `debug` holds the next launch, once; the launch is started, and if it
    /// is a run already in flight, waited for and started again, once; the pid is watched and `print`
    /// confirms it is still the job's held launch; the log's length is read; `SIGCONT` resumes it. The
    /// first start draws on `reservation` (made for [`Step::Start`], which the longest path stays within),
    /// the retry on one of its own: it makes at most `kickstart -p`, two `print`s, the confirming `print`
    /// and the `SIGCONT`, 5 calls and 2 requests. A failure of the `debug` is its own error; any later
    /// failure is [`stranded_hold`].
    pub(super) fn begin_task_run(
        &self,
        snapshot: Plist,
        reservation: &mut Reservation,
        notes: &mut dyn Write,
    ) -> Result<Run> {
        let path = snapshot.job()?.location.path.clone();
        let Plist { id, blob, .. } = snapshot;
        let logs = blob.map_err(Error::Kept)?.spec.common().logs.clone();
        let watch = Watch::new()?;
        // Held until this returns, which is after the `SIGCONT`.
        let _deferred = Deferred::begin()?;
        self.launchctl.load_job(id.as_str(), &path, reservation)?;
        self.launchctl.debug_start_suspended(id.as_str(), reservation)?;

        let strand = |e: Error| stranded_hold(&id, e);
        let mut retry: Option<Reservation> = None;
        let pid = match self.started(&id, &watch, reservation).map_err(strand)? {
            StartedOutcome::Held(pid) => pid,
            missed => {
                if let StartedOutcome::InFlight(_, registered) = missed {
                    wait_for_slipped_in(&id, registered, notes).map_err(strand)?;
                }
                // The hold is pending, so a retry that cannot reserve is the stranded hold. The retry
                // does not arm `debug`: the pending hold is the one the next launch takes, and so the
                // launch that `kickstart -p` names now is held, or is a pid goetia does not follow.
                let draws = retry.insert(flow::reserved(self, &id, &[Step::Start], Budget::Immediate).map_err(strand)?);
                match self.started(&id, &watch, draws).map_err(strand)? {
                    StartedOutcome::Held(pid) => pid,
                    StartedOutcome::Ended(pid) | StartedOutcome::InFlight(pid, _) => {
                        return Err(run_unfollowed(&id, pid));
                    }
                }
            }
        };
        let draws = retry.as_mut().unwrap_or(reservation);
        self.resume(id, pid, &watch, logs, draws)
    }

    /// `kickstart -p`, then `print`: what the launch it names is.
    fn started(&self, id: &Id, watch: &Watch, reservation: &mut Reservation) -> Result<StartedOutcome> {
        let name = id.as_str();
        let issuing = Budget::Unbounded.start();
        let Kickstarted::Confirmed(pid) =
            never_expires(
                self.launchctl
                    .kickstart(name, Budget::Unbounded, issuing, Evidence::Queued, reservation)?,
            )
        else {
            return Err(unestablished(
                id,
                "launchctl kickstart -p printed no whole pid".to_string(),
            ));
        };
        let print = self
            .launchctl
            .read_task_print(name, issuing, Role::Query, reservation)?;
        match launch_shown(&print, id, pid)? {
            Launch::Held => Ok(StartedOutcome::Held(pid)),
            Launch::Ended => Ok(StartedOutcome::Ended(pid)),
            Launch::InFlight => {
                let Added::Watching(registered) = watch.add(pid)? else {
                    return Ok(StartedOutcome::Ended(pid));
                };
                // A pid can be reused: wait only for the job's process that `print` still shows. A
                // registration left by any other way out of here is deleted as it is dropped.
                let still = self
                    .launchctl
                    .read_task_print(name, issuing, Role::Query, reservation)?;
                match launch_shown(&still, id, pid)? {
                    Launch::InFlight => Ok(StartedOutcome::InFlight(pid, registered)),
                    Launch::Ended => Ok(StartedOutcome::Ended(pid)),
                    // The first read was early: launchd can name a new launch's pid before it shows
                    // `started suspended = 1` for it (seen on CI, a `run` begun the moment another
                    // returned), and a launch that is not held cannot become so. It is the held one.
                    Launch::Held => {
                        drop(registered);
                        Ok(StartedOutcome::Held(pid))
                    }
                    Launch::Unreadable => Err(unestablished(
                        id,
                        format!("launchctl print no longer shows how the launch of process {pid} began"),
                    )),
                }
            }
            Launch::Unreadable => Err(unestablished(
                id,
                format!("launchctl print shows process {pid} and no `started suspended` line"),
            )),
        }
    }

    /// Watches the held launch `pid`, confirms `print` still shows it held, reads the log's length, and
    /// resumes it by label. A launch that is gone before it can be watched or resumed is
    /// [`Error::RunUnrecorded`]: it was ended, and nothing is stranded. Any other failure, and a launch
    /// `print` shows alive without saying it is held, is the stranded hold.
    fn resume(
        &self,
        id: Id,
        pid: u32,
        watch: &Watch,
        logs: Option<PathBuf>,
        reservation: &mut Reservation,
    ) -> Result<Run> {
        let strand = |e: Error| stranded_hold(&id, e);
        let name = id.as_str();
        let Added::Watching(registered) = watch.add(pid).map_err(strand)? else {
            return Err(run_unwatched(&id));
        };
        // Registered, but only a pid `print` shows as the job's held launch is the one to resume: the
        // one that was read may have ended, and its pid been reused, before it was watched.
        let print = self
            .launchctl
            .read_task_print(name, Budget::Unbounded.start(), Role::Query, reservation)
            .map_err(strand)?;
        match launch_shown(&print, &id, pid).map_err(strand)? {
            Launch::Held => {}
            Launch::Ended => return Err(run_unwatched(&id)),
            // Alive, and nothing was resumed: the launch may sit stopped.
            Launch::InFlight | Launch::Unreadable => {
                return Err(strand(unestablished(
                    &id,
                    format!("launchctl print no longer shows the launch of process {pid} as held"),
                )));
            }
        }
        // While the launch is held, launchd has opened the log and the program has written nothing.
        let logs = logs
            .map(|path| output::length_of(&path).map(|offset| (path, offset)))
            .transpose()
            .map_err(strand)?;
        match self.launchctl.signal(name, "SIGCONT", reservation).map_err(strand)? {
            SignalOutcome::Sent => Ok(Run {
                id,
                pid,
                registered,
                logs,
            }),
            SignalOutcome::NoProcess => Err(run_unresumed(&id)),
        }
    }
}

/// Says a scheduled run of `id` is in progress, then waits for it to end: `registered` is its watch.
fn wait_for_slipped_in(id: &Id, registered: Registered, notes: &mut dyn Write) -> Result<()> {
    let note =
        format!("note: a scheduled run of `{id}` is in progress; waiting for it to end before starting this run\n");
    output::show(notes, note.as_bytes())?;
    never_expires(registered.wait(None)?);
    Ok(())
}

/// What `print` shows of the launch `pid`, which `kickstart -p` named. A read that failed or was not
/// whole shows nothing, and is not mistaken for a launch that ended: another attempt would start another
/// run.
fn launch_shown(print: &TaskPrint, id: &Id, pid: u32) -> Result<Launch> {
    if matches!(print.shown, Shown::Failed) {
        return Err(unestablished(id, "launchctl print failed or was not whole".to_string()));
    }
    Ok(launch_of(print.pid, print.started_suspended, pid))
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
