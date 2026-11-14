//! A task's two plists read, scanned and reported as one id: `read` and `scan` over real files in
//! temporary directories (the directories are injectable), and `live_status` and `task_state` over a
//! scripted `launchctl`. Nothing here touches the system's own directories or services.

use std::io;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use super::super::manager_tests::launchd_over;
use super::super::{Dirs, scan_in};
use super::*;
use crate::backend::client::fake::{Fake, exited, reply};
use crate::backend::launchd::generate::Plists;
use crate::backend::launchd::task::{IN_BOTH, JOB_MISSING};
use crate::backend::{Backend as _, Identity, InstalledService as _, Scan, ScanEntry};
use crate::manager::Installed;
use crate::spec::ServiceSpec;
use crate::testing::{base_fields, task};

// Fixtures ============================================================================================================

// `write_tests` builds its trees and specs from these.
pub(in crate::backend::launchd::manager) const ID: &str = "goetia-task-unit";

pub(in crate::backend::launchd::manager) fn id() -> Id {
    Id::try_from(ID).unwrap()
}

pub(in crate::backend::launchd::manager) fn task_spec(schedule: Option<&str>) -> DaemonSpec {
    task(ID, vec!["/bin/true".to_string()], schedule)
}

pub(in crate::backend::launchd::manager) fn service_spec() -> DaemonSpec {
    ServiceSpec::new(base_fields(ID, vec!["/bin/true".to_string()]))
        .map(DaemonSpec::from)
        .unwrap()
}

/// What goetia writes for `spec`, run as root.
pub(in crate::backend::launchd::manager) fn plists_of(spec: &DaemonSpec) -> Plists {
    generate::plists(
        spec,
        &Identity {
            user: "root".to_string(),
        },
    )
    .unwrap()
}

/// A job's plist whose marker is there and whose metadata will not decode.
fn damaged(text: &str) -> String {
    let damaged = text.replace("Spec: ", "Spec: !!");
    assert_ne!(damaged, text);
    damaged
}

/// The two directories a plist can be in.
pub(in crate::backend::launchd::manager) struct Tree {
    staging: tempfile::TempDir,
    enabled: tempfile::TempDir,
}

impl Tree {
    pub(in crate::backend::launchd::manager) fn new() -> Self {
        Self {
            staging: tempfile::tempdir().expect("tempdir"),
            enabled: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub(in crate::backend::launchd::manager) fn dirs(&self) -> Dirs<'_> {
        [
            (self.staging.path().to_str().expect("a UTF-8 path"), false),
            (self.enabled.path().to_str().expect("a UTF-8 path"), true),
        ]
    }

    pub(in crate::backend::launchd::manager) fn dir(&self, enabled: bool) -> &Path {
        if enabled {
            self.enabled.path()
        } else {
            self.staging.path()
        }
    }

    /// Writes `<stem>.plist` in the enabled or the staging directory.
    pub(in crate::backend::launchd::manager) fn put(&self, enabled: bool, stem: &str, content: impl AsRef<[u8]>) {
        std::fs::write(self.dir(enabled).join(format!("{stem}.plist")), content).expect("write the plist");
    }

    pub(in crate::backend::launchd::manager) fn read(&self) -> Result<ReadOutcome<Plist>> {
        read_in(self.dirs(), &id())
    }

    fn scan(&self) -> Scan<Plist> {
        scan_in(self.dirs())
    }
}

fn timer_stem() -> String {
    timer_label(ID)
}

/// A snapshot of `spec`'s plists as `read` would make them, with the job and the timer each in the
/// enabled or the staging directory (no file backs them).
fn snapshot(spec: &DaemonSpec, job_enabled: bool, timer_enabled: bool) -> Plist {
    let plists = plists_of(spec);
    let artifact = |text: String, enabled: bool| Artifact {
        location: Location {
            path: PathBuf::from("/nonexistent/goetia-task-unit.plist"),
            enabled,
        },
        text,
    };
    Plist {
        id: spec.common().id.clone(),
        blob: Ok(generate::extract(&plists.job).unwrap().unwrap()),
        job: Some(artifact(plists.job, job_enabled)),
        timer: plists.timer.map(|text| artifact(text, timer_enabled)),
    }
}

/// A `launchctl print` body of the task's job: `state`, an optional `pid`, and the result line.
fn print_body(state: &str, pid: Option<u32>, result: &str) -> String {
    let pid = pid.map(|pid| format!("\tpid = {pid}\n")).unwrap_or_default();
    format!("system/{ID} = {{\n\tstate = {state}\n{pid}\t{result}\n}}\n")
}

/// A `launchd` whose `launchctl` answers each `print` in turn with `answers` (exit code and body).
fn printing(answers: &[(i32, String)]) -> (Launchd, Fake) {
    let fake = Fake::new(
        answers
            .iter()
            .map(|(code, body)| reply(&["print"], exited(*code, body, ""))),
    );
    (launchd_over(fake.clone()), fake)
}

fn status_of(mgr: &Launchd, snapshot: &Plist) -> TaskStatus {
    match mgr.live_status(snapshot).expect("a status") {
        Status::Task(status) => status,
        other => panic!("a task's status is a task's: {other:?}"),
    }
}

const NOT_LOADED: (i32, &str) = (113, "");

// read ================================================================================================================

pub(in crate::backend::launchd::manager) fn marked(outcome: Result<ReadOutcome<Plist>>) -> Plist {
    match outcome {
        Ok(ReadOutcome::Marked(plist)) => plist,
        Ok(ReadOutcome::Absent) => panic!("absent, not marked"),
        Ok(ReadOutcome::Foreign { recovery }) => panic!("foreign ({recovery:?}), not marked"),
        Err(e) => panic!("the read failed: {e}"),
    }
}

fn foreign(outcome: Result<ReadOutcome<Plist>>) -> Option<String> {
    match outcome {
        Ok(ReadOutcome::Foreign { recovery }) => recovery,
        Ok(ReadOutcome::Absent) => panic!("absent, not foreign"),
        Ok(ReadOutcome::Marked(_)) => panic!("marked, not foreign"),
        Err(e) => panic!("the read failed: {e}"),
    }
}

/// Every row of the read table, over real files.
#[skuld::test]
fn read_matches_the_table() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    let timer = task.timer.clone().expect("a scheduled task has a timer");
    let service = plists_of(&service_spec()).job;
    let stranger = "<plist/>";

    // absent, absent
    assert!(matches!(Tree::new().read(), Ok(ReadOutcome::Absent)));

    // marked, absent
    let tree = Tree::new();
    tree.put(false, ID, &task.job);
    let plist = marked(tree.read());
    assert!(plist.job.is_some() && plist.timer.is_none() && plist.blob.is_ok());

    // marked, marked
    let tree = Tree::new();
    tree.put(false, ID, &task.job);
    tree.put(false, &timer_stem(), &timer);
    let plist = marked(tree.read());
    assert_eq!(plist.job.as_ref().unwrap().text, task.job);
    assert_eq!(plist.timer.as_ref().unwrap().text, timer);

    // a task, a timer that is not goetia's: foreign, and the recovery names the timer's path
    let tree = Tree::new();
    tree.put(false, ID, &task.job);
    tree.put(false, &timer_stem(), stranger);
    let recovery = foreign(tree.read()).expect("the timer's recovery");
    assert!(
        recovery.contains(
            &*tree
                .dir(false)
                .join(format!("{}.plist", timer_stem()))
                .to_string_lossy()
        ),
        "{recovery}"
    );

    // absent, a timer that is not goetia's: the same
    let tree = Tree::new();
    tree.put(true, &timer_stem(), stranger);
    let recovery = foreign(tree.read()).expect("the timer's recovery");
    assert!(
        recovery.contains(&*tree.dir(true).join(format!("{}.plist", timer_stem())).to_string_lossy()),
        "{recovery}"
    );

    // a service, a timer that is not goetia's: marked, the timer ignored
    let tree = Tree::new();
    tree.put(false, ID, &service);
    tree.put(false, &timer_stem(), stranger);
    let plist = marked(tree.read());
    assert!(plist.job.is_some() && plist.timer.is_none());

    // a blob that will not decode, a timer that is not goetia's: marked, the timer ignored
    let tree = Tree::new();
    tree.put(false, ID, damaged(&task.job));
    tree.put(false, &timer_stem(), stranger);
    let plist = marked(tree.read());
    assert!(plist.blob.is_err() && plist.job.is_some() && plist.timer.is_none());

    // absent, marked: ours and unusable, its blob the reason
    let tree = Tree::new();
    tree.put(false, &timer_stem(), &timer);
    let plist = marked(tree.read());
    assert!(plist.job.is_none() && plist.timer.is_some());
    assert!(plist.blob.as_ref().unwrap_err().to_string().contains(JOB_MISSING));

    // not goetia's, any: foreign as it always was
    for timer_text in [None, Some(stranger), Some(timer.as_str())] {
        let tree = Tree::new();
        tree.put(false, ID, stranger);
        if let Some(text) = timer_text {
            tree.put(false, &timer_stem(), text);
        }
        assert_eq!(foreign(tree.read()), None, "{timer_text:?}");
    }

    // two copies: an error naming both paths, for either name
    let tree = Tree::new();
    tree.put(false, ID, &task.job);
    tree.put(true, ID, &task.job);
    let e = tree.read().err().expect("a job in both directories is an error");
    assert!(matches!(e, Error::AmbiguousInstall { .. }), "{e}");
    let tree = Tree::new();
    tree.put(false, ID, &task.job);
    tree.put(false, &timer_stem(), &timer);
    tree.put(true, &timer_stem(), &timer);
    let e = tree.read().err().expect("a timer in both directories is an error");
    assert!(matches!(e, Error::AmbiguousInstall { .. }), "{e}");
}

/// A binary plist is not goetia's, however small, whichever slot it is in.
#[skuld::test]
fn a_binary_plist_at_the_timer_path_is_foreign() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    let tree = Tree::new();
    tree.put(true, ID, &task.job);
    tree.put(true, &timer_stem(), b"bplist00\x00\x01\x02");

    let recovery = foreign(tree.read()).expect("the timer's recovery");
    assert!(recovery.contains(&timer_stem()), "{recovery}");

    // `read` and the verbs say it is foreign; `list` must not lose a task goetia installed, so the
    // scan reports it as goetia's and unusable, naming the timer's file.
    let scan = tree.scan();
    assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
    match scan.reported.as_slice() {
        [entry @ Installed::OursUnusable { name, .. }] => {
            assert_eq!(name, ID);
            let reason = entry.unusable_reason().expect("a reason");
            assert!(reason.contains(&timer_stem()), "{reason}");
        }
        other => panic!("one unusable entry expected, got {other:?}"),
    }
}

/// A timer file that is a stranger's, beside no job of goetia's, is nothing of goetia's: foreign, and
/// not listed.
#[skuld::test]
fn a_foreign_timer_with_no_job_is_not_goetias_to_list() {
    let tree = Tree::new();
    tree.put(true, &timer_stem(), "<plist/>");

    let scan = tree.scan();

    assert!(scan.reported.is_empty(), "{:?}", scan.reported);
    assert!(
        matches!(scan.entries.as_slice(), [(name, ScanEntry::Read(ReadOutcome::Foreign { .. }))] if name == ID),
        "{} entries",
        scan.entries.len()
    );
}

/// A task goetia installed whose timer's name a stranger's file now holds is listed as unusable, in
/// either directory, with the timer's path in the reason.
#[skuld::test]
fn a_task_whose_timer_is_a_strangers_is_listed_unusable() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    for enabled in [false, true] {
        let tree = Tree::new();
        tree.put(false, ID, &task.job);
        tree.put(enabled, &timer_stem(), "<plist/>");

        let scan = tree.scan();

        assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
        match scan.reported.as_slice() {
            [entry @ Installed::OursUnusable { name, .. }] => {
                assert_eq!(name, ID);
                let path = tree.dir(enabled).join(format!("{}.plist", timer_stem()));
                let reason = entry.unusable_reason().expect("a reason");
                assert!(reason.contains(&*path.to_string_lossy()), "{reason}");
            }
            other => panic!("one unusable entry expected, got {other:?}"),
        }
    }
}

/// A service beside a stranger's timer-named file is listed as the service it is.
#[skuld::test]
fn a_service_beside_a_strangers_timer_is_listed() {
    let tree = Tree::new();
    tree.put(false, ID, plists_of(&service_spec()).job);
    tree.put(false, &timer_stem(), "<plist/>");

    let scan = tree.scan();

    assert!(scan.reported.is_empty(), "{:?}", scan.reported);
    assert!(
        matches!(scan.entries.as_slice(), [(name, ScanEntry::Read(ReadOutcome::Marked(_)))] if name == ID),
        "{} entries",
        scan.entries.len()
    );
}

/// A `disable` that moved the timer and not the job, or an `enable` that moved the job and not the
/// timer: both files are goetia's, each where it is, and `enabled` is the timer's.
#[skuld::test]
fn job_and_timer_in_different_directories_read_as_ours() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    let timer = task.timer.clone().unwrap();
    for (job_enabled, timer_enabled) in [(true, false), (false, true)] {
        let tree = Tree::new();
        tree.put(job_enabled, ID, &task.job);
        tree.put(timer_enabled, &timer_stem(), &timer);

        let plist = marked(tree.read());
        assert_eq!(plist.job.as_ref().unwrap().location.enabled, job_enabled);
        assert_eq!(plist.timer.as_ref().unwrap().location.enabled, timer_enabled);

        let scan = tree.scan();
        assert!(scan.reported.is_empty(), "{:?}", scan.reported);
        match scan.entries.as_slice() {
            [(name, ScanEntry::Read(ReadOutcome::Marked(plist)))] => {
                assert_eq!(name, ID);
                assert_eq!(plist.job.as_ref().unwrap().location.enabled, job_enabled);
                assert_eq!(plist.timer.as_ref().unwrap().location.enabled, timer_enabled);
            }
            _ => panic!("one marked entry expected, got {} entries", scan.entries.len()),
        }
    }
}

// scan ================================================================================================================

#[skuld::test]
fn scan_attaches_a_timer_to_its_job_and_never_lists_it_as_an_id() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    let tree = Tree::new();
    tree.put(true, ID, &task.job);
    tree.put(true, &timer_stem(), task.timer.as_ref().unwrap());

    let scan = tree.scan();

    assert!(scan.reported.is_empty(), "{:?}", scan.reported);
    match scan.entries.as_slice() {
        [(name, ScanEntry::Read(ReadOutcome::Marked(plist)))] => {
            assert_eq!(name, ID, "the timer is not an id of its own");
            assert_eq!(plist.id().as_str(), ID);
            assert_eq!(plist.timer.as_ref().unwrap().text, *task.timer.as_ref().unwrap());
        }
        _ => panic!("one marked entry expected, got {} entries", scan.entries.len()),
    }
}

#[skuld::test]
fn a_timer_only_id_is_listed_unusable() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    let tree = Tree::new();
    tree.put(false, &timer_stem(), task.timer.as_ref().unwrap());

    let scan = tree.scan();

    assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
    match scan.reported.as_slice() {
        [entry @ Installed::OursUnusable { name, .. }] => {
            assert_eq!(name, ID, "under the job's id");
            assert_eq!(entry.unusable_reason().as_deref(), Some(JOB_MISSING));
        }
        other => panic!("one unusable entry expected, got {other:?}"),
    }
}

/// A timer in both directories is the same state as a job in both: named, not classified.
#[skuld::test]
fn a_timer_in_both_directories_is_reported_for_its_id() {
    let task = plists_of(&task_spec(Some("30 3 * * *")));
    let tree = Tree::new();
    tree.put(false, ID, &task.job);
    for enabled in [false, true] {
        tree.put(enabled, &timer_stem(), task.timer.as_ref().unwrap());
    }

    let scan = tree.scan();

    assert!(scan.entries.is_empty(), "{} entries", scan.entries.len());
    match scan.reported.as_slice() {
        [entry @ Installed::OursUnusable { name, .. }] => {
            assert_eq!(name, ID);
            assert_eq!(entry.unusable_reason().as_deref(), Some(IN_BOTH));
        }
        other => panic!("one unusable entry expected, got {other:?}"),
    }
}

// installed and regenerate ============================================================================================

#[skuld::test]
fn installed_is_the_text_of_each_plist() {
    let (mgr, _) = printing(&[]);
    let plist = snapshot(&task_spec(Some("30 3 * * *")), false, false);

    let installed = mgr.installed(&plist).expect("installed");

    assert_eq!(installed.definition, plists_of(&task_spec(Some("30 3 * * *"))));
}

/// The job from the spec, the timer from its own metadata: a job and a timer left by two installs are
/// each unmodified, and a job beside no timer regenerates without one.
#[skuld::test]
fn regenerate_takes_the_timer_from_its_own_metadata() {
    let (mgr, _) = printing(&[]);
    let (old, new) = (task_spec(Some("30 3 * * *")), task_spec(Some("0 4 * * *")));
    let mut beside = snapshot(&old, false, false);
    beside.job = snapshot(&new, false, false).job;

    let regenerated = mgr.regenerate(&new, &beside).expect("regenerate");

    assert_eq!(regenerated.job, plists_of(&new).job);
    assert_eq!(
        regenerated.timer,
        plists_of(&old).timer,
        "the timer is the old install's"
    );

    let mut alone = snapshot(&new, false, false);
    alone.timer = None;
    assert_eq!(mgr.regenerate(&new, &alone).expect("regenerate").timer, None);
}

// status ==============================================================================================================

#[skuld::test]
fn a_task_status_reads_print_once() {
    let spec = task_spec(Some("30 3 * * *"));
    let body = print_body("running", Some(4242), "last terminating signal = Killed: 9");
    let (mgr, fake) = printing(&[(0, body)]);

    let status = status_of(&mgr, &snapshot(&spec, true, true));

    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.common.pid, Some(4242));
    assert!(status.common.enabled);
    assert_eq!(status.last_result, Some(Exit::Signal(9)));
    assert_eq!(status.last_run, None);
    fake.assert_spent("one print");
    assert_eq!(fake.calls().len(), 1);
}

#[skuld::test]
fn an_unloaded_job_is_idle() {
    let spec = task_spec(Some("30 3 * * *"));
    let (mgr, fake) = printing(&[(NOT_LOADED.0, NOT_LOADED.1.to_string())]);

    let status = status_of(&mgr, &snapshot(&spec, false, false));

    assert_eq!(status.common.state, State::Idle);
    assert_eq!(status.common.pid, None);
    assert_eq!(status.last_result, None);
    fake.assert_spent("one print");
}

/// A job that ran and exited is idle, and its result is the run's.
#[skuld::test]
fn an_exited_job_is_idle_with_its_last_result() {
    let spec = task_spec(None);
    let (mgr, _) = printing(&[(0, print_body("exited", None, "last exit code = 3"))]);

    let status = status_of(&mgr, &snapshot(&spec, false, false));

    assert_eq!(status.common.state, State::Idle);
    assert_eq!(status.last_result, Some(Exit::Code(3)));
}

#[skuld::test]
fn enabled_is_the_timers_directory() {
    let scheduled = task_spec(Some("30 3 * * *"));
    // (the job's directory, the timer's): the timer's decides.
    for (job_enabled, timer_enabled) in [(true, false), (false, true), (true, true), (false, false)] {
        let (mgr, _) = printing(&[(NOT_LOADED.0, String::new())]);
        let status = status_of(&mgr, &snapshot(&scheduled, job_enabled, timer_enabled));
        assert_eq!(
            status.common.enabled, timer_enabled,
            "job {job_enabled}, timer {timer_enabled}"
        );
    }
    // A task with no timer has the job's.
    for job_enabled in [true, false] {
        let (mgr, _) = printing(&[(NOT_LOADED.0, String::new())]);
        let status = status_of(&mgr, &snapshot(&task_spec(None), job_enabled, false));
        assert_eq!(status.common.enabled, job_enabled);
    }
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// 2026-01-01T00:00:00Z.
const NEW_YEAR: u64 = 1_767_225_600;

fn utc() -> Result<TimeZone> {
    Ok(TimeZone::UTC)
}

fn never_asked() -> Result<TimeZone> {
    panic!("the time zone is read only for an armed timer with a schedule");
}

fn timer_of(spec: &DaemonSpec) -> Option<Artifact> {
    snapshot(spec, true, true).timer
}

#[skuld::test]
fn next_run_is_none_when_disarmed_for_no_schedule_and_for_a_schedule_with_no_date() {
    let scheduled = timer_of(&task_spec(Some("30 3 * * *")));
    assert!(scheduled.is_some());

    // Disarmed: the schedule is not read, nor the zone.
    assert_eq!(
        next_run(scheduled.as_ref(), false, at(NEW_YEAR), never_asked).unwrap(),
        None
    );
    // No timer: a task with no schedule.
    assert_eq!(next_run(None, true, at(NEW_YEAR), never_asked).unwrap(), None);
    assert!(timer_of(&task_spec(None)).is_none());
    // A schedule that names no date has a timer with no calendar.
    let dateless = timer_of(&task_spec(Some("0 0 30 2 *")));
    assert!(dateless.is_some());
    assert_eq!(next_run(dateless.as_ref(), true, at(NEW_YEAR), utc).unwrap(), None);

    // Armed with a date: the next fire, in the zone.
    assert_eq!(
        next_run(scheduled.as_ref(), true, at(NEW_YEAR), utc).unwrap(),
        Some(at(NEW_YEAR + 3 * 3600 + 30 * 60))
    );
}

/// The armed timer is what fires, and after a crash between replacing the job and the timer the job's
/// metadata can hold a newer schedule than the timer does.
#[skuld::test]
fn next_run_comes_from_the_timers_blob() {
    let timers = task_spec(Some("30 3 * * *"));
    let jobs = task_spec(Some("0 0 1 7 *"));
    let mut plist = snapshot(&timers, true, true);
    plist.job = snapshot(&jobs, true, true).job;
    plist.blob = snapshot(&jobs, true, true).blob;
    let zone = system_zone().expect("the machine's time zone");
    let before = SystemTime::now();
    let (mgr, _) = printing(&[(NOT_LOADED.0, String::new())]);

    let next = status_of(&mgr, &plist).next_run.expect("an armed timer's next run");

    let DaemonSpec::Task(timer_task) = &timers else {
        unreachable!()
    };
    let DaemonSpec::Task(job_task) = &jobs else {
        unreachable!()
    };
    let (timer_schedule, job_schedule) = (
        timer_task.schedule.as_ref().unwrap(),
        job_task.schedule.as_ref().unwrap(),
    );
    let just_before = next - Duration::from_secs(60);
    assert!(next > before);
    assert_eq!(
        next_fire(timer_schedule, just_before, &zone),
        Some(next),
        "a fire of the timer's schedule"
    );
    assert_ne!(
        next_fire(job_schedule, just_before, &zone),
        Some(next),
        "and not of the job's"
    );
}

#[skuld::test]
fn a_disarmed_task_has_no_next_run() {
    let (mgr, _) = printing(&[(NOT_LOADED.0, String::new())]);

    let status = status_of(&mgr, &snapshot(&task_spec(Some("30 3 * * *")), false, false));

    assert_eq!(status.next_run, None);
}

/// `next_run` fails when the time zone it needs cannot be read, and `task_state` makes the one `print`
/// and nothing else. That `live_status` returns `next_run`'s failure is not shown here.
#[skuld::test]
fn a_failing_zone_read_fails_status_but_not_task_state() {
    let timer = timer_of(&task_spec(Some("30 3 * * *")));
    let unreadable = || {
        Err(Error::os(
            "read the system time zone",
            io::Error::from_raw_os_error(libc::EACCES),
        ))
    };
    assert!(next_run(timer.as_ref(), true, at(NEW_YEAR), unreadable).is_err());

    let (mgr, fake) = printing(&[(0, print_body("not running", None, "last exit code = 0"))]);
    let state = mgr
        .task_state(&snapshot(&task_spec(Some("30 3 * * *")), true, true))
        .expect("a task's state needs no time zone");
    assert_eq!(state, State::Idle);
    fake.assert_spent("one print, and nothing else");
}

/// A start launchd has accepted and not yet forked shows `spawn scheduled` and no pid. It is `Running`,
/// so `run` straight after `start` is refused as running.
#[skuld::test]
fn task_state_of_a_spawn_scheduled_job_is_running() {
    let (mgr, _) = printing(&[
        (0, print_body("spawn scheduled", None, "last exit code = 0")),
        (0, print_body("spawn scheduled", None, "last exit code = 0")),
    ]);
    let plist = snapshot(&task_spec(None), false, false);

    assert_eq!(mgr.task_state(&plist).expect("task_state"), State::Running);
    let status = status_of(&mgr, &plist);
    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.common.pid, None);
}

/// A task's `task_state` of a job launchd does not hold is idle.
#[skuld::test]
fn task_state_of_an_unloaded_job_is_idle() {
    let (mgr, _) = printing(&[(NOT_LOADED.0, String::new())]);

    assert_eq!(
        mgr.task_state(&snapshot(&task_spec(None), false, false)).unwrap(),
        State::Idle
    );
}

/// A service reads its `print` as it did: an exited job is stopped, an unloaded one too, and
/// `spawn scheduled` is unknown.
#[skuld::test]
fn a_service_status_is_unchanged() {
    let plist = snapshot(&service_spec(), true, false);
    let service_status = |mgr: &Launchd| match mgr.live_status(&plist).expect("a status") {
        Status::Service(status) => status,
        other => panic!("a service's status is a service's: {other:?}"),
    };

    let (mgr, fake) = printing(&[(0, print_body("running", Some(4242), "last exit code = 0"))]);
    let running = service_status(&mgr);
    assert_eq!(
        (running.state, running.pid, running.enabled),
        (State::Running, Some(4242), true)
    );
    fake.assert_spent("one print");

    let (mgr, _) = printing(&[(NOT_LOADED.0, String::new())]);
    assert_eq!(service_status(&mgr).state, State::Stopped);

    let (mgr, _) = printing(&[(0, print_body("exited", None, "last exit code = 0"))]);
    assert_eq!(service_status(&mgr).state, State::Stopped);

    let (mgr, _) = printing(&[(0, print_body("spawn scheduled", None, "last exit code = 0"))]);
    assert_eq!(service_status(&mgr).state, State::Unknown);
}
