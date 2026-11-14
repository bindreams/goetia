use std::sync::Arc;

use super::*;
use crate::backend::Identity;
use crate::backend::launchd::generate::{self, Plists};
use crate::backend::launchd::{ENABLED_DIR, STAGING_DIR};
use crate::error::Error;
use crate::spec::DaemonSpec;
use crate::spec::test_support::program;
use crate::testing::{base_fields, task};

// Fixtures ============================================================================================================

fn decoded(spec: &DaemonSpec) -> Decoded {
    let text = generate::plists(
        spec,
        &Identity {
            user: "root".to_string(),
        },
    )
    .unwrap()
    .job;
    Ok(generate::extract(&text)
        .unwrap()
        .expect("a generated plist carries the marker"))
}

fn task_job() -> Slot {
    Slot::Marked(decoded(&task("job", vec![program("job")], Some("30 3 * * *"))))
}

fn service_job() -> Slot {
    let spec = crate::spec::ServiceSpec::new(crate::spec::ServiceFields {
        protocol: crate::spec::Protocol::Plain,
        ..base_fields("job", vec![program("job")])
    })
    .map(DaemonSpec::from)
    .unwrap();
    Slot::Marked(decoded(&spec))
}

/// A marker whose metadata will not decode.
fn undecodable_job() -> Slot {
    Slot::Marked(Err(Arc::new(Error::blob("the metadata is damaged"))))
}

fn timer() -> Slot {
    Slot::Marked(decoded(&task("job", vec![program("job")], Some("30 3 * * *"))))
}

/// `Some(true)` for a task, `Some(false)` for a service, `None` for a blob that will not decode and for no job.
fn is_task(job: &Slot) -> Option<bool> {
    match job {
        Slot::Marked(Ok(blob)) => Some(blob.spec.kind() == crate::spec::DaemonKind::Task),
        _ => None,
    }
}

fn classified(job: &Slot, timer: &Slot) -> Classified {
    classify(job, timer, is_task(job))
}

// classify ============================================================================================================

/// Every row of the read table, the answer and the reason it carries.
#[skuld::test]
fn classify_table() {
    // | job | timer | result |
    let cases: Vec<(&str, Slot, Slot, Classified)> = vec![
        ("absent, absent", Slot::Absent, Slot::Absent, Classified::Absent),
        ("a task, no timer", task_job(), Slot::Absent, Classified::Marked),
        ("a service, no timer", service_job(), Slot::Absent, Classified::Marked),
        ("a task, its timer", task_job(), timer(), Classified::Marked),
        (
            "a task, a timer that is not goetia's",
            task_job(),
            Slot::NotOurs,
            Classified::Foreign { timer: true },
        ),
        (
            "no job, a timer that is not goetia's",
            Slot::Absent,
            Slot::NotOurs,
            Classified::Foreign { timer: true },
        ),
        (
            "a service, a timer that is not goetia's: the timer is ignored",
            service_job(),
            Slot::NotOurs,
            Classified::Marked,
        ),
        (
            "a blob that will not decode, a timer that is not goetia's: the timer is ignored",
            undecodable_job(),
            Slot::NotOurs,
            Classified::Marked,
        ),
        (
            "no job, its timer",
            Slot::Absent,
            timer(),
            Classified::Unusable("the timer's job is missing"),
        ),
        (
            "not goetia's job, no timer",
            Slot::NotOurs,
            Slot::Absent,
            Classified::Foreign { timer: false },
        ),
        (
            "not goetia's job, a timer that is",
            Slot::NotOurs,
            timer(),
            Classified::Foreign { timer: false },
        ),
        (
            "not goetia's job, not goetia's timer",
            Slot::NotOurs,
            Slot::NotOurs,
            Classified::Foreign { timer: false },
        ),
        (
            "a job in both directories",
            Slot::Both,
            Slot::Absent,
            Classified::Unusable(IN_BOTH),
        ),
        (
            "a task, its timer in both directories",
            task_job(),
            Slot::Both,
            Classified::Unusable(IN_BOTH),
        ),
        (
            "no job, a timer in both directories",
            Slot::Absent,
            Slot::Both,
            Classified::Unusable(IN_BOTH),
        ),
        (
            "a service, its timer in both directories: the timer is ignored",
            service_job(),
            Slot::Both,
            Classified::Marked,
        ),
    ];
    for (what, job, timer, expected) in cases {
        assert_eq!(classified(&job, &timer), expected, "{what}");
    }
}

/// `read` of a service whose neighbour at `<id>@timer.plist` is a stranger's leaves both alone: the
/// service reads as it always did.
#[skuld::test]
fn a_foreign_timer_beside_a_service_is_ignored() {
    assert_eq!(classified(&service_job(), &Slot::NotOurs), Classified::Marked);
    assert_eq!(classified(&undecodable_job(), &Slot::NotOurs), Classified::Marked);
}

#[skuld::test]
fn a_timer_without_its_job_is_unusable() {
    assert_eq!(
        classified(&Slot::Absent, &timer()),
        Classified::Unusable("the timer's job is missing")
    );
}

/// The reason a slot in both directories gives is the one `scan` always gave a job there.
#[skuld::test]
fn the_reason_for_both_directories_names_them() {
    assert_eq!(IN_BOTH, format!("present in both {STAGING_DIR} and {ENABLED_DIR}"));
}

// update_plan and uninstall_plan ======================================================================================

use Action::*;

fn plists_of(spec: &DaemonSpec) -> Plists {
    generate::plists(
        spec,
        &Identity {
            user: "root".to_string(),
        },
    )
    .unwrap()
}

fn scheduled(command: &str, schedule: &str) -> DaemonSpec {
    task("job", vec![program(command)], Some(schedule))
}

fn unscheduled(command: &str) -> DaemonSpec {
    task("job", vec![program(command)], None)
}

fn service(command: &str) -> DaemonSpec {
    crate::spec::ServiceSpec::new(crate::spec::ServiceFields {
        protocol: crate::spec::Protocol::Plain,
        ..base_fields("job", vec![program(command)])
    })
    .map(DaemonSpec::from)
    .unwrap()
}

/// What launchd holds of an installed `spec`.
#[derive(Clone, Copy, Default)]
struct Held {
    job: bool,
    timer: bool,
}

const NOTHING: Held = Held {
    job: false,
    timer: false,
};
const BOTH: Held = Held { job: true, timer: true };
const JOB: Held = Held {
    job: true,
    timer: false,
};

/// `spec` as installed, its plists in the enabled directory or the staging one.
fn installed(spec: &DaemonSpec, enabled: bool, held: Held) -> Old {
    let plists = plists_of(spec);
    let on_disk = |text: String| OnDisk { enabled, text };
    Old {
        kind: spec.kind(),
        job: Some(on_disk(plists.job)),
        timer: plists.timer.map(on_disk),
        job_loaded: held.job,
        timer_loaded: held.timer,
    }
}

fn plan(old: &Old, new: &DaemonSpec) -> Vec<Action> {
    update_plan(old, &plists_of(new), new)
}

/// Every row of Decision 15's plan, and where Decision 12's order (a timer is created before anything
/// changes, removed before its job is replaced) puts each action.
#[skuld::test]
fn update_plan_table() {
    let armed = installed(&scheduled("a", "30 3 * * *"), true, BOTH);
    let disarmed = installed(&scheduled("a", "30 3 * * *"), false, NOTHING);

    // `a` and `b` are different programs, so a different job: the timer's metadata holds the command too.
    let cases: Vec<(&str, Old, DaemonSpec, Vec<Action>)> = vec![
        (
            "no change",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            scheduled("a", "30 3 * * *"),
            vec![],
        ),
        (
            "only the metadata changed: both files are rewritten and launchd is not asked",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            rename(scheduled("a", "30 3 * * *"), "another name"),
            vec![ReplaceJob, ReplaceTimer],
        ),
        (
            "schedule only, armed: the job is left alone, the timer goes out and comes back",
            armed,
            scheduled("a", "45 4 * * *"),
            vec![BootoutTimer, ReplaceJob, ReplaceTimer, BootstrapTimer],
        ),
        (
            "schedule only, disarmed",
            disarmed,
            scheduled("a", "45 4 * * *"),
            vec![ReplaceJob, ReplaceTimer],
        ),
        (
            "command only, armed: the timer out, the job idle and out, then job before timer back in",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            scheduled("b", "30 3 * * *"),
            vec![
                BootoutTimer,
                CheckJobIdle,
                BootoutJob,
                ReplaceJob,
                ReplaceTimer,
                BootstrapJob,
                BootstrapTimer,
            ],
        ),
        (
            "command and schedule, armed",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            scheduled("b", "45 4 * * *"),
            vec![
                BootoutTimer,
                CheckJobIdle,
                BootoutJob,
                ReplaceJob,
                ReplaceTimer,
                BootstrapJob,
                BootstrapTimer,
            ],
        ),
        (
            "command only, enabled but nothing loaded (an earlier install failed after booting out): loaded now",
            installed(&scheduled("a", "30 3 * * *"), true, NOTHING),
            scheduled("b", "30 3 * * *"),
            vec![ReplaceJob, ReplaceTimer, BootstrapJob, BootstrapTimer],
        ),
        (
            "command only, disarmed with its job loaded: boot the job out and leave it out",
            installed(&scheduled("a", "30 3 * * *"), false, JOB),
            scheduled("b", "30 3 * * *"),
            vec![CheckJobIdle, BootoutJob, ReplaceJob, ReplaceTimer],
        ),
        (
            "command only, enabled with the job loaded and the timer not: the timer is loaded as well",
            installed(&scheduled("a", "30 3 * * *"), true, JOB),
            scheduled("b", "30 3 * * *"),
            vec![
                CheckJobIdle,
                BootoutJob,
                ReplaceJob,
                ReplaceTimer,
                BootstrapJob,
                BootstrapTimer,
            ],
        ),
        (
            "metadata only, enabled with the timer's job loaded and the timer not: the timer is loaded",
            installed(&scheduled("a", "30 3 * * *"), true, JOB),
            rename(scheduled("a", "30 3 * * *"), "another name"),
            vec![ReplaceJob, ReplaceTimer, BootstrapTimer],
        ),
        (
            "a loaded job, idle or with a run in flight, gets the same plan: CheckJobIdle reads which",
            installed(&unscheduled("a"), true, JOB),
            unscheduled("b"),
            vec![CheckJobIdle, BootoutJob, ReplaceJob, BootstrapJob],
        ),
        (
            "a timer label launchd holds with no timer file: it goes out with the job, and nothing is loaded for it",
            installed(&unscheduled("a"), true, BOTH),
            unscheduled("b"),
            vec![BootoutTimer, CheckJobIdle, BootoutJob, ReplaceJob, BootstrapJob],
        ),
        (
            "a task with no schedule, no change but the metadata",
            installed(&unscheduled("a"), false, NOTHING),
            rename(unscheduled("a"), "another name"),
            vec![ReplaceJob],
        ),
        (
            "schedule added where there was none, disarmed: the timer is created first",
            installed(&unscheduled("a"), false, NOTHING),
            scheduled("a", "30 3 * * *"),
            vec![CreateTimerFirst, ReplaceJob],
        ),
        (
            "schedule added where there was none, enabled: the new timer is armed",
            installed(&unscheduled("a"), true, JOB),
            scheduled("a", "30 3 * * *"),
            vec![CreateTimerFirst, ReplaceJob, BootstrapTimer],
        ),
        (
            "schedule dropped, armed: the timer goes before the job is replaced",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            unscheduled("a"),
            vec![BootoutTimer, RemoveTimer, ReplaceJob],
        ),
        (
            "schedule dropped, disarmed",
            installed(&scheduled("a", "30 3 * * *"), false, NOTHING),
            unscheduled("a"),
            vec![RemoveTimer, ReplaceJob],
        ),
        (
            "schedule dropped and command changed, armed",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            unscheduled("b"),
            vec![
                BootoutTimer,
                CheckJobIdle,
                BootoutJob,
                RemoveTimer,
                ReplaceJob,
                BootstrapJob,
            ],
        ),
        (
            "a job without its timer (the crash after the job): the timer is created, the job is as written",
            Old {
                timer: None,
                ..installed(&scheduled("a", "30 3 * * *"), false, NOTHING)
            },
            scheduled("a", "30 3 * * *"),
            vec![CreateTimerFirst],
        ),
        (
            "service to task, staged: the timer is created before the job is replaced",
            installed(&service("a"), false, NOTHING),
            scheduled("a", "30 3 * * *"),
            vec![CreateTimerFirst, ReplaceJob],
        ),
        (
            "service to task, enabled and not loaded: the job leaves boot enrolment before it is replaced",
            installed(&service("a"), true, NOTHING),
            scheduled("a", "30 3 * * *"),
            vec![CreateTimerFirst, MoveJobToStaging, ReplaceJob],
        ),
        (
            "service to task, loaded: the check refuses a loaded service before anything is written",
            installed(&service("a"), true, JOB),
            scheduled("a", "30 3 * * *"),
            vec![CheckJobIdle, CreateTimerFirst, BootoutJob, MoveJobToStaging, ReplaceJob],
        ),
        (
            "task to service, armed: no arming and no boot enrolment survive",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            service("a"),
            vec![
                BootoutTimer,
                CheckJobIdle,
                BootoutJob,
                RemoveTimer,
                MoveJobToStaging,
                ReplaceJob,
            ],
        ),
        (
            "task to service, enabled and not loaded",
            installed(&scheduled("a", "30 3 * * *"), true, NOTHING),
            service("a"),
            vec![RemoveTimer, MoveJobToStaging, ReplaceJob],
        ),
        (
            "task to service, staged",
            installed(&unscheduled("a"), false, NOTHING),
            service("a"),
            vec![ReplaceJob],
        ),
    ];
    for (what, old, new, expected) in cases {
        assert_eq!(plan(&old, &new), expected, "{what}");
    }
}

/// `spec` named `name`: a change to the metadata only, which no plist key renders.
fn rename(spec: DaemonSpec, name: &str) -> DaemonSpec {
    let DaemonSpec::Task(task) = spec else {
        panic!("only a task is renamed here");
    };
    let mut fields = task.into_fields();
    fields.common.name = name.to_string();
    crate::spec::TaskSpec::new(fields).map(DaemonSpec::from).unwrap()
}

/// An uninstall boots out what launchd holds, timer first, then removes the files, timer first: a crash
/// anywhere leaves a state the read table names.
#[skuld::test]
fn uninstall_plan_boots_out_then_removes_timer_before_job() {
    let task = scheduled("a", "30 3 * * *");
    assert_eq!(
        uninstall_plan(&installed(&task, true, BOTH)),
        vec![BootoutTimer, BootoutJob, RemoveTimer, RemoveJob]
    );
    assert_eq!(
        uninstall_plan(&installed(&task, false, NOTHING)),
        vec![RemoveTimer, RemoveJob],
        "launchd is asked only about what it holds"
    );
    assert_eq!(
        uninstall_plan(&installed(&unscheduled("a"), false, JOB)),
        vec![BootoutJob, RemoveJob]
    );
    assert_eq!(
        uninstall_plan(&Old {
            job: None,
            ..installed(&task, false, NOTHING)
        }),
        vec![RemoveTimer],
        "a timer alone at the id"
    );
}

/// A task is enabled where its timer is, and where its job is when it has no timer.
#[skuld::test]
fn a_task_is_enabled_where_its_timer_is() {
    let mut old = installed(&scheduled("a", "30 3 * * *"), false, NOTHING);
    assert!(!old.enabled());
    old.timer.as_mut().unwrap().enabled = true;
    assert!(old.enabled(), "the timer's directory decides");
    old.timer = None;
    assert!(!old.enabled(), "no timer: the job's");
}
