//! A task's `install`, `uninstall`, `enable` and `disable`: the job's plist and the timer's, changed
//! together.
//!
//! What to do is decided purely, in [`crate::backend::launchd::task`] (`update_plan`,
//! `uninstall_plan`, `enable_plan`, `disable_plan`), from what is on disk and what launchd holds;
//! [`Executor`] performs the plan's actions in order. An update first reads which labels launchd holds
//! (two `print`s, which decide what to stage); then both plists are staged and linted before the first
//! request that changes anything, so a malformed one stops the install before anything changed. A crash
//! anywhere leaves a state of the read table, and running the verb again finishes it.
//!
//! A task's directories are an argument (`Dirs`), as `read` and `scan` take theirs.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use super::verbs::{self, Quiet};
use super::{
    Account, Artifact, Dirs, Launchd, PersistOutcome, Plist, Role, State, ensure_dir, move_no_clobber, never_expires,
    persist_new, persist_new_unraced, persist_over, plist_in, prepare_parent_dirs, read_artifact, resolve_account,
    staged_tempfile, still_present,
};
use crate::backend::bounded::Reservation;
use crate::backend::launchd::generate::{self, Plists, timer_label};
use crate::backend::launchd::refuse;
use crate::backend::launchd::task::{Action, Old, OnDisk, disable_plan, enable_plan, uninstall_plan, update_plan};
use crate::backend::{Backend as _, WriteOutcome};
use crate::decide::Outcome;
use crate::error::{Error, Result};
use crate::manager::Budget;
use crate::spec::{DaemonKind, DaemonSpec, Id};

impl Launchd {
    /// `install` of a task (or of a daemon that becomes or stops being one), over `dirs`.
    ///
    /// A `Create` stages and lints both plists, boots out a job or timer launchd already holds under
    /// either label (a hand-installed predecessor, or one still tearing down, which `start` would
    /// take for ours), then writes the job into staging and the timer after it, each refusing to
    /// replace anything. A write that fails after the job leaves a job without its timer, which is
    /// ours: the next install is an `Update` that writes the timer.
    ///
    /// An `Update` or `Stale` reads which labels launchd holds, then runs `update_plan`.
    ///
    /// Never `Raced`: the reads of which labels launchd holds have been made by the time a plist is
    /// found changed, and another pass would make more calls than the verb reserved, so a race is
    /// an error instead. A file written at a task's timer name, or a second install of the same id,
    /// while install runs ends it with an error, or, for an `Update` that finds a stranger's file at the
    /// timer's name, a foreign refusal; what it had done by then stays, and a re-run starts from what
    /// it then reads.
    pub(super) fn write_task(
        &self,
        dirs: Dirs,
        spec: &DaemonSpec,
        desired: &Plists,
        outcome: &Outcome,
        snapshot: Option<&Plist>,
        reservation: &mut Reservation,
    ) -> Result<WriteOutcome> {
        // Resolved again: `render` kept only the plists, which name the account, and launchd resolves
        // that name at launch too.
        let account = resolve_account(&spec.common().user)?;
        let id = &spec.common().id;
        let staging = Path::new(dirs[0].0);

        match outcome {
            Outcome::Create => {
                prepare_parent_dirs(spec, &account)?;
                let (job_target, timer_target) = (
                    plist_in(staging, id.as_str()),
                    plist_in(staging, &timer_label(id.as_str())),
                );
                let staged_job = staged_tempfile(staging, &desired.job)?;
                let staged_timer = desired
                    .timer
                    .as_deref()
                    .map(|timer| staged_tempfile(staging, timer))
                    .transpose()?;
                // The timer's label is asked about whether or not this task has a timer: a loaded one
                // would start the job.
                let mut booted_out = false;
                for label in [timer_label(id.as_str()), id.to_string()] {
                    if never_expires(
                        self.launchctl
                            .is_loaded(&label, Budget::Unbounded.start(), reservation)?,
                    ) {
                        never_expires(self.launchctl.bootout(&label, Budget::Unbounded.start(), reservation)?);
                        booted_out = true;
                    }
                }
                persist_new_unraced(staged_job, &job_target, id, booted_out)?;
                if let Some(staged) = staged_timer {
                    match persist_new(staged, &timer_target)? {
                        PersistOutcome::Written => {}
                        PersistOutcome::Raced => return Err(refuse::changed_during_install(id, &timer_target)),
                    }
                }
                Ok(WriteOutcome::Done)
            }
            Outcome::Update { .. } | Outcome::Stale { .. } => {
                let snapshot = snapshot.expect("an id `decide` chose to update was read as marked");
                let old = self.old_of(snapshot, reservation)?;
                let plan = update_plan(&old, desired, spec);
                let (job, timer) = (
                    snapshot.job()?.location.path.clone(),
                    snapshot.timer.as_ref().map(path_of),
                );

                // Where the job will be: staging, once a retype has moved it there. The timer a daemon
                // gains is created beside it.
                let job_dir = if plan.contains(&Action::MoveJobToStaging) {
                    staging
                } else {
                    job.parent().expect("plist path always has a parent")
                };
                let staged_job = plan
                    .contains(&Action::ReplaceJob)
                    .then(|| staged_tempfile(job_dir, &desired.job))
                    .transpose()?;
                let new_timer = |dir: &Path| {
                    let text = desired
                        .timer
                        .as_deref()
                        .expect("a timer is replaced or created for a new timer");
                    staged_tempfile(dir, text)
                };
                let (staged_timer, new_timer_path) = if plan.contains(&Action::CreateTimerFirst) {
                    (
                        Some(new_timer(job_dir)?),
                        Some(plist_in(job_dir, &timer_label(id.as_str()))),
                    )
                } else if plan.contains(&Action::ReplaceTimer) {
                    let path = timer.clone().expect("a timer is replaced where it is");
                    (
                        Some(new_timer(path.parent().expect("plist path always has a parent"))?),
                        None,
                    )
                } else {
                    (None, None)
                };

                // `read` captured these paths before `decide` ran. A concurrent `enable`, `disable` or
                // `uninstall` may have moved or removed one since, and writing here would put a plist
                // in both directories, which `read` errors on, or leave an uninstalled one installed.
                // This narrows the race to the window between it and the write; it does not close it.
                let touched = [
                    (plan.contains(&Action::ReplaceJob) || plan.contains(&Action::MoveJobToStaging)).then_some(&job),
                    (plan.contains(&Action::ReplaceTimer) || plan.contains(&Action::RemoveTimer))
                        .then_some(timer.as_ref())
                        .flatten(),
                ];
                for path in touched.into_iter().flatten() {
                    if !still_present(path)? {
                        return Err(refuse::changed_during_install(id, path));
                    }
                }

                Executor {
                    launchd: self,
                    id,
                    old: &old,
                    staging,
                    enabled: Path::new(dirs[1].0),
                    job: Some(job),
                    timer: timer.or(new_timer_path),
                    staged_job,
                    staged_timer,
                    verb: Verb::Install,
                    timer_out: false,
                    // After a refusal, as the service path has it: a refused update creates nothing.
                    prepare: Some((spec, &account)),
                }
                .run(&plan, reservation)
            }
            other => unreachable!("`flow::install` writes only `Create`, `Update` and `Stale`, got {other:?}"),
        }
    }

    /// `uninstall` of a task, or of an id that has a timer: see `uninstall_plan`. Unbounded, like a
    /// service's: it has no `--timeout`, and bounding the job's `bootout` would turn a slow-stopping run
    /// into a failed uninstall.
    pub(super) fn uninstall_task(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        let old = self.old_of(&snapshot, reservation)?;
        let executed = Executor {
            launchd: self,
            id: &snapshot.id,
            old: &old,
            staging: Path::new(super::STAGING_DIR),
            enabled: Path::new(super::ENABLED_DIR),
            job: snapshot.job.as_ref().map(path_of),
            timer: snapshot.timer.as_ref().map(path_of),
            staged_job: None,
            staged_timer: None,
            verb: Verb::Uninstall,
            timer_out: false,
            prepare: None,
        }
        .run(&uninstall_plan(&old), reservation)?;
        debug_assert!(matches!(executed, WriteOutcome::Done), "an uninstall refuses nothing");
        Ok(())
    }

    /// `enable` of a task over `dirs`: see `enable_plan`. It reads which labels launchd holds and does
    /// what is left, whatever `enabled` says, so that running it again finishes a crash. A timer that a
    /// scheduled task has none of is rendered from the job's metadata, staged and linted before the
    /// first request, and created where the job goes, refusing to replace a stranger's file there
    /// ([`Error::Foreign`]). `reservation` is the verb's own.
    pub(super) fn enable_task(&self, dirs: Dirs, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        let id = &snapshot.id;
        let job = snapshot.job()?.location.path.clone();
        let enabled = Path::new(dirs[1].0);
        let old = self.old_of(&snapshot, reservation)?;
        let plan = enable_plan(&old);
        let mut timer = snapshot.timer.as_ref().map(path_of);
        let mut staged_timer = None;
        if plan.contains(&Action::CreateTimerEnabled) {
            let blob = snapshot
                .blob
                .as_ref()
                .expect("a timer is created for a job whose metadata decodes");
            let text = self
                .render(&blob.spec)?
                .timer
                .expect("a task with a schedule renders a timer");
            staged_timer = Some(staged_tempfile(enabled, &text)?);
            timer = Some(plist_in(enabled, &timer_label(id.as_str())));
        }
        let executed = Executor {
            launchd: self,
            id,
            old: &old,
            staging: Path::new(dirs[0].0),
            enabled,
            job: Some(job),
            timer,
            staged_job: None,
            staged_timer,
            verb: Verb::Enable,
            timer_out: false,
            prepare: None,
        }
        .run(&plan, reservation)?;
        match executed {
            WriteOutcome::Done => Ok(()),
            WriteOutcome::Refused(Outcome::RefuseForeign { recovery }) => Err(Error::Foreign {
                id: id.to_string(),
                recovery,
            }),
            other => unreachable!("an enable is refused only for a stranger's timer, got {other:?}"),
        }
    }

    /// `disable` of a task over `dirs`: see `disable_plan`. The timer is booted out (and waited for)
    /// before it moves, and the job is never booted out. `reservation` is the verb's own.
    pub(super) fn disable_task(&self, dirs: Dirs, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        let job = snapshot.job()?.location.path.clone();
        let old = self.old_of(&snapshot, reservation)?;
        let executed = Executor {
            launchd: self,
            id: &snapshot.id,
            old: &old,
            staging: Path::new(dirs[0].0),
            enabled: Path::new(dirs[1].0),
            job: Some(job),
            timer: snapshot.timer.as_ref().map(path_of),
            staged_job: None,
            staged_timer: None,
            verb: Verb::Disable,
            timer_out: false,
            prepare: None,
        }
        .run(&disable_plan(&old), reservation)?;
        debug_assert!(matches!(executed, WriteOutcome::Done), "a disable refuses nothing");
        Ok(())
    }

    /// `snapshot` as pure facts: its files, and whether launchd holds each label. Two reads, job's
    /// first. A read that fails is an `Err`, never a label that is not loaded.
    fn old_of(&self, snapshot: &Plist, reservation: &mut Reservation) -> Result<Old> {
        let id = snapshot.id.as_str();
        let deadline = Budget::Unbounded.start();
        let on_disk = |artifact: &Artifact| OnDisk {
            enabled: artifact.location.enabled,
            text: artifact.text.clone(),
        };
        let job_loaded = never_expires(self.launchctl.is_loaded(id, deadline, reservation)?);
        let timer_loaded = never_expires(self.launchctl.is_loaded(&timer_label(id), deadline, reservation)?);
        Ok(Old {
            kind: snapshot.kind(),
            job: snapshot.job.as_ref().map(on_disk),
            timer: snapshot.timer.as_ref().map(on_disk),
            job_loaded,
            timer_loaded,
            scheduled: matches!(
                &snapshot.blob,
                Ok(blob) if matches!(&blob.spec, DaemonSpec::Task(task) if task.schedule.is_some())
            ),
        })
    }
}

fn path_of(artifact: &Artifact) -> PathBuf {
    artifact.location.path.clone()
}

/// The timer's name was taken after `read` saw it free. A file with no marker is a stranger's, which a
/// task needs the name of for its schedule: the install ends with that refusal, as `read` would have
/// said it. One with the marker is another install of goetia's, so this one stops and says to re-run.
fn timer_taken(id: &Id, path: &Path) -> Result<WriteOutcome> {
    match read_artifact(path, id.as_str()) {
        // Gone again: the race went on.
        Err(Error::NotInstalled { .. }) => Err(refuse::changed_during_install(id, path)),
        Err(e) => Err(e),
        Ok(text) if generate::extract(&text).transpose().is_some() => Err(refuse::changed_during_install(id, path)),
        Ok(_) => Ok(WriteOutcome::Refused(Outcome::RefuseForeign {
            recovery: super::task::foreign_timer_recovery(&[path.to_path_buf()]),
        })),
    }
}

/// A plan's actions, performed in order over what is on disk and in launchd.
struct Executor<'a> {
    launchd: &'a Launchd,
    id: &'a Id,
    old: &'a Old,
    staging: &'a Path,
    enabled: &'a Path,
    /// Where the job's plist is now, and the timer's: a plan's moves and creations update them.
    job: Option<PathBuf>,
    timer: Option<PathBuf>,
    /// The linted files a `Replace` or a `Create` puts in place, staged before the first request.
    staged_job: Option<NamedTempFile>,
    staged_timer: Option<NamedTempFile>,
    /// Which verb the plan is, for what an error says to run again.
    verb: Verb,
    /// Whether this plan booted out a timer that has a plist and has not loaded it again or removed
    /// it: an error then says the timer is not loaded, and a refusal loads it again first.
    timer_out: bool,
    /// The directories the spec names (`cwd`, the parent of `logs`), made before the first action that
    /// changes anything a refusal could leave untouched.
    prepare: Option<(&'a DaemonSpec, &'a Account)>,
}

/// The verb a plan belongs to.
#[derive(Clone, Copy)]
enum Verb {
    Install,
    Uninstall,
    Enable,
    Disable,
}

impl Executor<'_> {
    fn run(&mut self, plan: &[Action], reservation: &mut Reservation) -> Result<WriteOutcome> {
        // A plan with no action has no refusal to come before: the directories are made, as an
        // install that changes nothing has always made them.
        if plan.is_empty()
            && let Some((spec, account)) = self.prepare.take()
        {
            prepare_parent_dirs(spec, account)?;
        }
        for (at, &action) in plan.iter().enumerate() {
            match self.perform(action, reservation) {
                Ok(Some(refusal)) => return Ok(refusal),
                Ok(None) => {}
                // `plan[at..]` still holds the action that failed: a write that failed is not done.
                Err(e) if self.timer_out => return Err(self.timer_stranded(e, files_written(&plan[at..]))),
                Err(e) => return Err(e),
            }
        }
        Ok(WriteOutcome::Done)
    }

    /// `Some` where the install ends with a refusal that is not an error.
    fn perform(&mut self, action: Action, reservation: &mut Reservation) -> Result<Option<WriteOutcome>> {
        if !matches!(action, Action::BootoutTimer | Action::CheckJobIdle)
            && let Some((spec, account)) = self.prepare.take()
        {
            prepare_parent_dirs(spec, account)?;
        }
        let (launchctl, id) = (&self.launchd.launchctl, self.id.as_str());
        let deadline = Budget::Unbounded.start();
        let timer_label = timer_label(id);
        match action {
            Action::BootoutTimer => {
                never_expires(launchctl.bootout(&timer_label, deadline, reservation)?);
                // A stray has no plist to load again.
                if !self.old.timer_stray() {
                    self.timer_out = true;
                }
            }
            Action::CheckJobIdle => {
                // A service is refused while launchd has it at all (one that has exited is still
                // loaded); a task while it has a process, shows `running`, or has a start launchd has
                // accepted. A state this
                // cannot read as idle or as a run is refused too, saying what launchd showed.
                let verdict = match self.old.kind {
                    DaemonKind::Service if never_expires(launchctl.is_loaded(id, deadline, reservation)?) => {
                        Verdict::InFlight
                    }
                    DaemonKind::Service => Verdict::Idle,
                    DaemonKind::Task => {
                        let print = launchctl.read_task_print(id, deadline, Role::Query, reservation)?;
                        // A state of `running` is a run, as launchd reports it; any other print with no
                        // pid is read as `stop` reads it.
                        if print.pid.is_some() || print.state == State::Running {
                            Verdict::InFlight
                        } else {
                            match verbs::quiet(self.id, &print) {
                                Ok(Quiet::Queued) => Verdict::InFlight,
                                Ok(Quiet::Idle) => Verdict::Idle,
                                Err(_) => Verdict::Unknown(print.shown.describe(print.state)),
                            }
                        }
                    }
                };
                match verdict {
                    Verdict::Idle => {}
                    Verdict::InFlight => {
                        let refusal = refuse::loaded_refusal(self.id, self.old.kind);
                        return Err(self.refuse(refusal, reservation));
                    }
                    Verdict::Unknown(shown) => {
                        let refusal = Error::Step {
                            what: format!(
                                "the changed plists of `{id}` were not installed and the installed ones are \
                                 untouched: re-run install"
                            ),
                            source: Box::new(Error::os(
                                format!("read whether the job of `{id}` is idle"),
                                std::io::Error::other(shown),
                            )),
                        };
                        return Err(self.refuse(refusal, reservation));
                    }
                }
            }
            Action::CreateTimerFirst | Action::CreateTimerEnabled => {
                let target = self.timer.clone().expect("the executor knows where a new timer goes");
                let staged = self
                    .staged_timer
                    .take()
                    .expect("a new timer is staged before the first request");
                match persist_new(staged, &target)? {
                    PersistOutcome::Written => {}
                    PersistOutcome::Raced => return timer_taken(self.id, &target).map(Some),
                }
            }
            Action::BootoutJob => {
                never_expires(launchctl.bootout(id, deadline, reservation)?);
            }
            Action::MoveJobToStaging => {
                let from = self.job.take().expect("a job moved is where it was read");
                let to = plist_in(self.staging, id);
                ensure_dir(self.staging)?;
                move_no_clobber(&from, &to)?;
                self.job = Some(to);
            }
            Action::MoveJobToEnabled => {
                let from = self.job.take().expect("a job moved is where it was read");
                let to = plist_in(self.enabled, id);
                move_no_clobber(&from, &to)?;
                self.job = Some(to);
            }
            Action::MoveTimerToEnabled => {
                let from = self.timer.take().expect("a timer moved is where it was read");
                let to = plist_in(self.enabled, &timer_label);
                move_no_clobber(&from, &to)?;
                self.timer = Some(to);
            }
            Action::MoveTimerToStaging => {
                let from = self.timer.take().expect("a timer moved is where it was read");
                let to = plist_in(self.staging, &timer_label);
                ensure_dir(self.staging)?;
                move_no_clobber(&from, &to)?;
                self.timer = Some(to);
            }
            Action::ReplaceJob => {
                let staged = self
                    .staged_job
                    .take()
                    .expect("a replaced job is staged before the first request");
                persist_over(
                    staged,
                    self.job.as_deref().expect("a replaced job is where it was read"),
                )?;
            }
            Action::ReplaceTimer => {
                let staged = self
                    .staged_timer
                    .take()
                    .expect("a replaced timer is staged before the first request");
                persist_over(
                    staged,
                    self.timer.as_deref().expect("a replaced timer is where it was read"),
                )?;
            }
            Action::RemoveTimer => {
                let path = self.timer.take().expect("a removed timer is where it was read");
                fs::remove_file(&path).map_err(|e| Error::os_at("remove", &path, e))?;
                // Gone for good, as the new spec has no schedule: there is nothing to load again.
                self.timer_out = false;
            }
            Action::RemoveJob => {
                let path = self.job.take().expect("a removed job is where it was read");
                fs::remove_file(&path).map_err(|e| Error::os_at("remove", &path, e))?;
            }
            Action::BootstrapJob => self.load(&self.job.clone().expect("a loaded job has a file"), id, reservation)?,
            Action::BootstrapTimer => {
                // A failure to load says what is left to do, which covers the timer.
                self.timer_out = false;
                self.load(
                    &self.timer.clone().expect("a loaded timer has a file"),
                    &timer_label,
                    reservation,
                )?;
            }
        }
        Ok(None)
    }

    /// Load `label` from `path`, after the plists were replaced or moved: a failure says what is left to
    /// do.
    fn load(&self, path: &Path, label: &str, reservation: &mut Reservation) -> Result<()> {
        let id = self.id;
        let what = match self.verb {
            Verb::Install => format!(
                "the plists of `{id}` were replaced, but launchd could not load `{label}`: run `goetia daemon \
                 enable {id}` to load what is missing"
            ),
            Verb::Enable => format!(
                "enabling `{id}` stopped because launchd could not load `{label}`: run `goetia daemon enable {id}` \
                 again to finish"
            ),
            Verb::Uninstall | Verb::Disable => unreachable!("neither an uninstall nor a disable loads anything"),
        };
        self.launchd
            .launchctl
            .bootstrap(path, Budget::Unbounded.start(), reservation)
            .map(never_expires)
            .map_err(|source| Error::Step {
                what,
                source: Box::new(source),
            })
    }

    /// `refusal`, the job not being idle, with the timer loaded again first if this plan booted it out.
    /// A timer that cannot be loaded again is told beside the refusal: the task is not armed.
    fn refuse(&mut self, refusal: Error, reservation: &mut Reservation) -> Error {
        let ours = std::mem::take(&mut self.timer_out);
        // Before any file changes, the timer's path is where it was read.
        let Some(timer) = self.timer.as_deref().filter(|_| ours) else {
            return refusal;
        };
        match self
            .launchd
            .launchctl
            .bootstrap(timer, Budget::Unbounded.start(), reservation)
        {
            Ok(loaded) => {
                never_expires(loaded);
                refusal
            }
            Err(rearm) => Error::Both {
                first: Box::new(refusal),
                then: Box::new(rearm),
            },
        }
    }

    /// `error`, which ended the verb after the timer was booted out and before it was loaded again or
    /// removed: the timer is not loaded, and running the verb again finishes it. `files_written` is
    /// whether every file the plan writes is in place: an install is then up to date, loads nothing, and
    /// `enable` is what loads.
    fn timer_stranded(&self, error: Error, files_written: bool) -> Error {
        let (id, label) = (self.id, timer_label(self.id.as_str()));
        let what = match self.verb {
            Verb::Install if files_written => format!(
                "`{id}`'s timer (`{label}`) was booted out and this install failed before loading it again, so it \
                 is not loaded; the plists are already replaced, so install would find nothing to change: run \
                 `goetia daemon enable {id}` to load what is missing"
            ),
            Verb::Install => format!(
                "`{id}`'s timer (`{label}`) was booted out and this install failed before loading it again, so it \
                 is not loaded: re-run install"
            ),
            Verb::Uninstall => format!(
                "`{id}`'s timer (`{label}`) was booted out and this uninstall failed before the timer's plist was \
                 removed, so it is not loaded: re-run uninstall"
            ),
            Verb::Disable => format!(
                "`{id}`'s timer (`{label}`) was booted out and this disable failed before the plists were moved to \
                 staging, so the task is disarmed but not fully disabled: re-run disable"
            ),
            // `enable_plan` boots out only a stray, and a stray never sets `timer_out`.
            Verb::Enable => unreachable!("an enable boots out no timer it has a plist for"),
        };
        Error::Step {
            what,
            source: Box::new(error),
        }
    }
}

/// Whether `rest` writes no file: every `Create` and `Replace` of the job's plist and the timer's is behind
/// it.
fn files_written(rest: &[Action]) -> bool {
    !rest.iter().any(|action| {
        matches!(
            action,
            Action::CreateTimerFirst | Action::CreateTimerEnabled | Action::ReplaceJob | Action::ReplaceTimer
        )
    })
}

/// What `CheckJobIdle` read of the job.
enum Verdict {
    Idle,
    /// A service that is loaded, or a task with a process, in state `running`, or with an accepted start.
    InFlight,
    /// A task whose `print` shows a state that is neither, or could not be read: what launchd showed, as
    /// [`super::task::Shown::describe`] words it.
    Unknown(String),
}

#[cfg(test)]
#[path = "write_tests.rs"]
pub(super) mod write_tests;
