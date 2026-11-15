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
//! pid is registered for its exec and exit, and its start time is read at once (it is read of a process
//! that is still `xpcproxy` too). Then its program's path is read, and `launchctl procinfo` is asked
//! whether the pid is the job's launch: launchd's own pid-to-job lookup names the launch from the
//! `xpcproxy` stage on, with no lag, and never a descendant of it, a stranger or a dead pid (KB, "macOS -
//! launchctl procinfo names a launch's job from the xpcproxy stage on, in every call"). That is asked
//! before any signal, wait or follow, once per pass: a pid is a number, and what it names can have changed
//! between `kickstart -p` printing it and goetia reading it. Where the program is `xpcproxy` its first exec
//! is waited for, and then the state is read, which is only a fact of the launch after the exec. A read by
//! pid counts only if the watch shows no exit for the pid after it: otherwise the process is gone at that
//! read. The reads and the rule that decides from them are [`launch_of`], with `ta` the time read after the
//! `launchctl debug` process was reaped:
//!
//! | process | started | the launch is | `run` |
//! | --- | --- | --- | --- |
//! | gone at the registration, or at the read of its start | | not known: this run's, or a scheduled run's | ends: [`run_vanished`], in doubt |
//! | not named by `launchctl procinfo` | | another process's: not known whose launch ended | ends: [`run_vanished`], in doubt |
//! | gone at the read of its path, exited before its exec, or gone at the read of its state | after `ta` | ours, which ended before goetia could follow it | ends: [`run_ended_unfollowed`] |
//! | stopped | after `ta` | the one `debug` held | resumes it, and follows it |
//! | running | after `ta` | our own `kickstart`'s, with no hold | ends: [`run_unfollowed`] |
//! | gone, as above | at or before `ta` | an older one, that is over | starts another pass |
//! | stopped | at or before `ta` | an older one that took a hold | resumes it, waits for its exit, starts another pass |
//! | running | at or before `ta` | a run in flight | waits for its exit, starts another pass |
//!
//! A launch of ours of a task without `logs` whose program cannot be started ends here too, as one that
//! ended before goetia could follow it (a task with `logs` runs `/bin/sh` first, which starts).
//!
//! Another pass is a fresh `debug`: a hold the older launch took is gone, and one it did not take is still
//! pending and the `debug` is refused as pending (exit 37), which is the same hold. No pass count is
//! limited: a pass that does not end the `run` waited for a launch some other `run` or the timer made, and
//! it has now ended. Each pass reserves before its first request.
//!
//! A launch that took a hold before launchd handled our `debug` started before `debug` returned, so its
//! start is at or before `ta`; one of ours is after it. Measured (macOS CI, root): 0 of 3957 older launches
//! started after `ta`, and 1850 of 1850 of ours did, by at least 4.4 ms (KB, "macOS - A launch that took a
//! hold before our debug can have a start time inside the debug call"). That an older launch never starts
//! after `ta` is this measured fact and not something launchd promises.
//!
//! From before the job is loaded to the `SIGCONT`, SIGINT, SIGTERM and SIGHUP are held back
//! ([`Deferred`]): ending goetia after the `debug` would leave a launch held and stopped. The check
//! immediately before each `SIGCONT` decides, the one that resumes our launch and the one that releases an
//! older launch which took the hold: an interrupt recorded by then ends the `run` as [`stranded_hold`] and
//! that `SIGCONT` is not sent. The `SIGCONT` is a `launchctl` subprocess and signals are not blocked around
//! it, so an interrupt can arrive between the check and its effect: one recorded after the check is held
//! back until `begin_run` returns, as one after the resume of our own launch is. After the release of an
//! older launch, the wait for its exit ends on that interrupt. The waits for another launch's exit and for
//! `xpcproxy`'s exec end on an interrupt too, and a pass that follows one starts nothing, with the same
//! end. A signal that was ignored when the window opened, or that the opening thread blocks, is none of
//! these: it stays as it was. A failure in the window carries it too: a launch that took the hold waits
//! stopped until `goetia daemon stop` ends it, and a hold no launch has taken is consumed by the next
//! `goetia daemon run`.
//!
//! [`launch_of`]: crate::backend::launchd::state::launch_of
//! [`run_vanished`]: crate::backend::launchd::refuse::run_vanished
//! [`run_ended_unfollowed`]: crate::backend::launchd::refuse::run_ended_unfollowed
//! [`run_unfollowed`]: crate::backend::launchd::refuse::run_unfollowed

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;

use super::interrupts::Deferred;
use super::proc;
use super::watch::{Added, Registered, Watch, Woke};
use super::{Kickstarted, Launchd, Plist, SignalOutcome, never_expires};
use crate::backend::bounded::Reservation;
use crate::backend::launchd::generate::run_marker;
use crate::backend::launchd::refuse::{run_interrupted, run_lost, run_unresumed, stranded_hold};
use crate::backend::launchd::state::{Evidence, Launch, Then, exit_of, launch_of, launch_unknown};
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
    /// In this order: interrupts are deferred; the kqueue is made, so that failing to make it holds
    /// nothing, and after the deferral, so that a signal that came before it, and that the earlier
    /// disposition handled or ignored, is none of this run's interrupts; the job is loaded; then passes
    /// until one follows a launch. A pass is `debug`, `kickstart -p`, the registration and the kernel's
    /// account of the pid, and then one of: the log's length read and `SIGCONT` of the held launch,
    /// `SIGCONT` of an older launch that took a hold and a wait for its exit, a wait for a run's exit, or
    /// the end of the `run`. The first pass draws on `reservation` (made for [`Step::Start`], which the
    /// longest first pass stays within: the load, `debug`, `kickstart -p`, `launchctl procinfo` and
    /// `SIGCONT`), each later one on its own. A failure of the first `debug`, and an interrupt recorded
    /// before it (while the job is loaded), are their own errors: no hold exists. Any later failure is
    /// [`stranded_hold`].
    pub(super) fn begin_task_run(
        &self,
        snapshot: Plist,
        reservation: &mut Reservation,
        notes: &mut dyn Write,
    ) -> Result<Run> {
        let path = snapshot.job()?.location.path.clone();
        let Plist { id, blob, .. } = snapshot;
        let logs = blob.map_err(Error::Kept)?.spec.common().logs.clone();
        let window = Deferred::begin()?;
        let watch = Watch::new_interruptible(&window)?;
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
            let first = own.is_none();
            let draws = match own.as_mut() {
                Some(own) => own,
                None => &mut *reservation,
            };
            if let Some(run) = attempt.pass(draws, first)? {
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
    /// that is over or has ended and was not this run's, and another pass is to be made. `first` is whether
    /// it is the first pass: before its `debug` no hold exists, so a failure of the `debug` and an interrupt
    /// recorded before it are their own errors, and any other failure is [`stranded_hold`].
    fn pass(&mut self, reservation: &mut Reservation, first: bool) -> Result<Option<Run>> {
        let id = self.id;
        let name = id.as_str();
        let strand = |e: Error| stranded_hold(id, e);
        let before_debug = |e: Error| if first { e } else { strand(e) };
        // An interrupt recorded while the job loaded, or that arrived while an earlier pass waited and its
        // exit came with it: nothing is started after it.
        if self.watch.interrupted(self.window) {
            let (received, left) = if first {
                ("before goetia armed a hold", "no hold was armed")
            } else {
                ("before goetia began another pass", "a hold may be pending")
            };
            return Err(before_debug(self.interrupted(received, left)));
        }

        let ta = self
            .launchd
            .launchctl
            .debug_start_suspended(name, reservation)
            .map_err(before_debug)?;
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

        // Registered before anything reads the pid: what happens to it from here on is an event. A pid that
        // is gone at the registration is a launch with no start time.
        let Added::Watching(registered) = self.watch.add_launch(pid).map_err(strand)? else {
            return Err(launch_unknown(id, pid));
        };
        match self.read_launch(pid, ta, &registered, reservation)? {
            Launch::Ended => Ok(None),
            Launch::Held => self.resume(pid, registered, reservation).map(Some),
            Launch::TookHold => {
                self.unless_interrupted(
                    "before goetia released the launch that took the hold",
                    "that launch waits stopped",
                )?;
                // Whether the launch is still the label's does not matter: it is waited for either way.
                self.launchd
                    .launchctl
                    .signal(name, "SIGCONT", reservation)
                    .map_err(strand)?;
                self.wait_for_exit(registered)
            }
            Launch::InFlight => self.wait_for_exit(registered),
        }
    }

    /// The interrupt that ended the run: `received` and `left` say what it was received before and what is
    /// left ([`run_interrupted`]), and the window names the signals it can be.
    fn interrupted(&self, received: &str, left: &str) -> Error {
        run_interrupted(self.id, &self.window.interrupt_signals(), received, left)
    }

    /// The end of the `run` as [`stranded_hold`] if an interrupt has been received by now, and no `Err`
    /// otherwise: `received` and `left` are for [`Self::interrupted`]. This check, made immediately before a
    /// `SIGCONT`, is what decides: one received by then ends the `run` and no `SIGCONT` is sent. The
    /// `SIGCONT` is a `launchctl` subprocess, so an interrupt can still arrive between the check and its
    /// effect; one received after the check is held back until `begin_run` returns, as one after the
    /// resume of our own launch is.
    fn unless_interrupted(&self, received: &str, left: &str) -> Result<()> {
        if self.watch.interrupted(self.window) {
            return Err(stranded_hold(self.id, self.interrupted(received, left)));
        }
        Ok(())
    }

    /// The [`Launch`] `pid` is, from the kernel: its start time is read at once, and [`Self::then_of`] says
    /// what became of it. A pid with no process at the start time's read has no start time.
    fn read_launch(&self, pid: u32, ta: u64, registered: &Registered, reservation: &mut Reservation) -> Result<Launch> {
        let Seen::Value(started) = self.seen(registered, || kernel(pid, proc::started))? else {
            return Err(launch_unknown(self.id, pid));
        };
        launch_of(started, self.then_of(pid, registered, reservation)?, ta, self.id, pid)
    }

    /// What the reads of `pid` show after its start time was read. Its program's path is read together with
    /// `launchctl procinfo`'s answer to whether it is the job's launch, before anything else: a pid that is
    /// not is [`Then::Stranger`] and nothing more is read or waited for. Where the program is `xpcproxy`
    /// its first exec is waited for, under no bound: the exec is certain to come (measured), and from it the
    /// process is stopped if it is held. A launch that exits before it execs is [`Then::Gone`], with its
    /// status.
    fn then_of(&self, pid: u32, registered: &Registered, reservation: &mut Reservation) -> Result<Then> {
        let id = self.id;
        let strand = |e: Error| stranded_hold(id, e);
        let (path, named) = match self.seen(registered, || {
            let Some(path) = kernel(pid, proc::path_of)? else {
                return Ok(None);
            };
            let named = self.launchd.launchctl.names_launch(id.as_str(), pid, reservation)?;
            Ok(Some((path, named)))
        })? {
            Seen::Value(read) => read,
            Seen::Gone(status) => return Ok(Then::Gone(status)),
        };
        if !named {
            return Ok(Then::Stranger);
        }
        if path.file_name() == Some(OsStr::new("xpcproxy")) {
            match registered.first_exec(self.window).map_err(strand)? {
                Woke::Exec => {}
                Woke::Exited(status) => return Ok(Then::Gone(status)),
                Woke::Interrupted => {
                    return Err(strand(self.interrupted(
                        "before goetia resumed or released the launch it waited to exec its program",
                        "a launch may be held and stopped, or a hold pending",
                    )));
                }
            }
        }
        Ok(match self.seen(registered, || kernel(pid, proc::stopped))? {
            Seen::Value(stopped) => Then::Process { stopped },
            Seen::Gone(status) => Then::Gone(status),
        })
    }

    /// What `reads` say of the pid `registered` watches: [`Seen::Value`] where they count, and
    /// [`Seen::Gone`] where there is no such process, and where the watch shows an exit of the pid after the
    /// reads, as the pid may be another process's by then. The exit's status goes with it only where the watch
    /// has the exit by the time of the check, whichever read it raced; a pid that no read found, and no exit
    /// has been seen of yet, is gone with no status. The reads are made together and checked once: an exit is no later than its event, and until the
    /// pid is reaped it names no other process, so a check that shows none after the last read covers every
    /// read before it. A read that fails is the stranded hold.
    fn seen<T>(&self, registered: &Registered, reads: impl FnOnce() -> Result<Option<T>>) -> Result<Seen<T>> {
        let id = self.id;
        let strand = |e: Error| stranded_hold(id, e);
        let value = reads().map_err(strand)?;
        if let Some(exited) = registered.exit_kept().map_err(strand)? {
            return Ok(Seen::Gone(exited.status));
        }
        Ok(match value {
            Some(value) => Seen::Value(value),
            None => Seen::Gone(None),
        })
    }

    /// Says a scheduled run is in progress, then waits for the launch `registered` to exit, ending on an
    /// interrupt. `None`: it has ended, and another pass is to be made.
    fn wait_for_exit(&mut self, registered: Registered) -> Result<Option<Run>> {
        let id = self.id;
        self.note();
        match registered.exit(self.window).map_err(|e| stranded_hold(id, e))? {
            Woke::Interrupted => Err(stranded_hold(
                id,
                self.interrupted(
                    "by the time goetia stopped waiting for a scheduled run",
                    "a hold may be pending",
                ),
            )),
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
        self.unless_interrupted("before goetia resumed the launch it held", "that launch waits stopped")?;
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

/// What [`Attempt::seen`] made of a read of a pid.
enum Seen<T> {
    /// The read counts.
    Value(T),
    /// There is no such process, or the watch showed it exit after the read: with the `wait(2)` status the
    /// watch has for the exit by the time of the check, where it has one.
    Gone(Option<i32>),
}

/// What the kernel shows of `pid` through `read`, a read of [`proc`]: its failure said as the run's.
fn kernel<T>(pid: u32, read: impl FnOnce(u32) -> io::Result<Option<T>>) -> Result<Option<T>> {
    read(pid).map_err(|source| Error::os(format!("read what the kernel shows of process {pid}"), source))
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
