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
        scheduled: matches!(spec, DaemonSpec::Task(task) if task.schedule.is_some()),
    }
}

fn plan(old: &Old, new: &DaemonSpec) -> Vec<Action> {
    update_plan(old, &plists_of(new), new)
}

/// One row of [`update_cases`]: what the plan is for, the installed task, the new spec, the plan.
type UpdateCase = (&'static str, Old, DaemonSpec, Vec<Action>);

/// Every row of Decision 15's plan, and where Decision 12's order (a timer is created before anything
/// changes, removed before its job is replaced) puts each action.
#[skuld::test]
fn update_plan_table() {
    for (what, old, new, expected) in update_cases() {
        assert_eq!(plan(&old, &new), expected, "{what}");
    }
}

fn update_cases() -> Vec<UpdateCase> {
    let armed = installed(&scheduled("a", "30 3 * * *"), true, BOTH);
    let disarmed = installed(&scheduled("a", "30 3 * * *"), false, NOTHING);

    // `a` and `b` are different programs, so a different job: the timer's metadata holds the command too.
    vec![
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
            "a timer label launchd holds with no timer file, nothing else changing: it goes out all the same",
            installed(&unscheduled("a"), true, BOTH),
            rename(unscheduled("a"), "another name"),
            vec![BootoutTimer, ReplaceJob],
        ),
        (
            "a timer label launchd holds with no timer file, a service updated: it goes out with the job",
            installed(&service("a"), false, BOTH),
            service("b"),
            vec![BootoutTimer, CheckJobIdle, BootoutJob, ReplaceJob],
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
            "schedule added where a timer label is loaded with no timer file, enabled: the stray goes out first, \
             and the new timer is loaded",
            installed(&unscheduled("a"), true, BOTH),
            scheduled("a", "30 3 * * *"),
            vec![BootoutTimer, CreateTimerFirst, ReplaceJob, BootstrapTimer],
        ),
        (
            "schedule added where a timer label is loaded with no timer file, disarmed: the stray goes out and \
             nothing is loaded",
            installed(&unscheduled("a"), false, BOTH),
            scheduled("a", "30 3 * * *"),
            vec![BootoutTimer, CreateTimerFirst, ReplaceJob],
        ),
        (
            "schedule added and command changed, a timer label loaded with no timer file: it goes out once, \
             before the idle check, so no fire can start a run between the check and the job's bootout",
            installed(&unscheduled("a"), true, BOTH),
            scheduled("b", "30 3 * * *"),
            vec![
                BootoutTimer,
                CheckJobIdle,
                CreateTimerFirst,
                BootoutJob,
                ReplaceJob,
                BootstrapJob,
                BootstrapTimer,
            ],
        ),
        (
            "a job without its timer (the crash after the job), its timer label loaded: the stray goes out and the \
             new timer is loaded",
            Old {
                timer: None,
                ..installed(&scheduled("a", "30 3 * * *"), true, BOTH)
            },
            scheduled("a", "30 3 * * *"),
            vec![BootoutTimer, CreateTimerFirst, BootstrapTimer],
        ),
        (
            "schedule dropped, armed: the timer goes before the job is replaced, and the job leaves the enabled \
             directory, as there is nothing left to arm",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            unscheduled("a"),
            vec![BootoutTimer, RemoveTimer, MoveJobToStaging, ReplaceJob],
        ),
        (
            "schedule dropped, disarmed",
            installed(&scheduled("a", "30 3 * * *"), false, NOTHING),
            unscheduled("a"),
            vec![RemoveTimer, ReplaceJob],
        ),
        (
            "schedule dropped and command changed, armed: the job is staged, so it is not loaded again",
            installed(&scheduled("a", "30 3 * * *"), true, BOTH),
            unscheduled("b"),
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
    ]
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

// enable_plan and disable_plan ========================================================================================

const TIMER: Held = Held {
    job: false,
    timer: true,
};

/// A scheduled task with its job in the enabled directory or staging, its timer in the enabled one
/// (`Some(true)`), in staging (`Some(false)`) or not at all (`None`), and `held` loaded.
fn placed(job_enabled: bool, timer_enabled: Option<bool>, held: Held) -> Old {
    let mut old = installed(&scheduled("a", "30 3 * * *"), job_enabled, held);
    match timer_enabled {
        Some(enabled) => old.timer.as_mut().expect("a scheduled task has a timer").enabled = enabled,
        None => old.timer = None,
    }
    old
}

/// One row per placement of the two files and loaded state of the two labels, in Decision 12's order:
/// the job to the enabled directory and loaded, then the timer likewise. The crash states are the ones
/// an early return on `enabled` would never finish.
#[skuld::test]
fn enable_plan_table() {
    for (what, old, expected) in enable_cases() {
        assert_eq!(enable_plan(&old), expected, "{what}");
    }
}

/// One row of [`enable_cases`]: what the plan is for, the installed task, the plan.
type EnableCase = (&'static str, Old, Vec<Action>);

fn enable_cases() -> Vec<EnableCase> {
    vec![
        (
            "everything staged, nothing loaded",
            placed(false, Some(false), NOTHING),
            vec![MoveJobToEnabled, BootstrapJob, MoveTimerToEnabled, BootstrapTimer],
        ),
        (
            "enable run again: the job is in place and both are loaded, so only the timer moves",
            placed(true, Some(false), BOTH),
            vec![MoveTimerToEnabled],
        ),
        (
            "a crash that moved the timer but did not load it: the timer is loaded, the job untouched",
            placed(true, Some(true), JOB),
            vec![BootstrapTimer],
        ),
        (
            "both in place, nothing loaded (a crash before either load, or a restart that loads neither)",
            placed(true, Some(true), NOTHING),
            vec![BootstrapJob, BootstrapTimer],
        ),
        (
            "everything in place and loaded: nothing to do",
            placed(true, Some(true), BOTH),
            vec![],
        ),
        (
            "the job was loaded from staging by a start: it is moved and not loaded again",
            placed(false, Some(false), JOB),
            vec![MoveJobToEnabled, MoveTimerToEnabled, BootstrapTimer],
        ),
        (
            "a timer loaded from staging is moved and not loaded again",
            placed(true, Some(false), Held { job: true, timer: true }),
            vec![MoveTimerToEnabled],
        ),
        (
            "a job without its timer, staged: the timer is created where the job goes",
            placed(false, None, NOTHING),
            vec![MoveJobToEnabled, BootstrapJob, CreateTimerEnabled, BootstrapTimer],
        ),
        (
            "a job without its timer, in place and loaded: the timer is created and loaded",
            placed(true, None, JOB),
            vec![CreateTimerEnabled, BootstrapTimer],
        ),
        (
            "a job without its timer, its timer label loaded: the stray goes out once the timer is created, and the \
             new one is loaded",
            placed(true, None, BOTH),
            vec![CreateTimerEnabled, BootoutTimer, BootstrapTimer],
        ),
        (
            "a job without its timer, staged, its timer label loaded",
            placed(false, None, BOTH),
            vec![MoveJobToEnabled, CreateTimerEnabled, BootoutTimer, BootstrapTimer],
        ),
        (
            "a task with no schedule has no timer to create",
            Old {
                scheduled: false,
                ..placed(false, None, NOTHING)
            },
            vec![MoveJobToEnabled, BootstrapJob],
        ),
        (
            "a timer label alone loaded, its file in place: it is loaded already",
            placed(true, Some(true), TIMER),
            vec![BootstrapJob],
        ),
    ]
}

/// `old` as it is when launchd holds the timer's label and there is no file for it.
fn with_stray_timer(old: &Old) -> Old {
    Old {
        timer: None,
        timer_loaded: true,
        ..old.clone()
    }
}

/// Where the actions of `plan` that `of` picks are.
fn positions(plan: &[Action], of: impl Fn(Action) -> bool) -> Vec<usize> {
    plan.iter()
        .enumerate()
        .filter(|&(_, &action)| of(action))
        .map(|(at, _)| at)
        .collect()
}

/// The rule for a timer label launchd holds with no timer file (`timer_stray`), for every plan: it is
/// booted out once, before the idle check and the job's bootout (a fire between them would start a run
/// that the bootout ends, as for any timer the plan boots out) and before any timer it loads. The shapes
/// are the installed side of every row of the update and enable tables, set to hold a stray label,
/// crossed with every new spec the update table names and, for `enable`, a task with and without a
/// schedule.
#[skuld::test]
fn a_stray_timer_label_is_booted_out_once_before_the_idle_check_the_job_bootout_and_the_load() {
    let check = |what: &str, plan: &[Action]| {
        let out = positions(plan, |action| action == BootoutTimer);
        assert_eq!(out.len(), 1, "one bootout of the stray: {what}: {plan:?}");
        for (name, behind) in [
            ("the idle check", CheckJobIdle),
            ("the job's bootout", BootoutJob),
            ("the load", BootstrapTimer),
        ] {
            let at = positions(plan, |action| action == behind);
            assert!(
                at.iter().all(|&at| out[0] < at),
                "the bootout comes before {name}: {what}: {plan:?}"
            );
        }
    };

    let rows = update_cases();
    let news: Vec<&DaemonSpec> = rows.iter().map(|(_, _, new, _)| new).collect();
    for (what, old, _, _) in &rows {
        let old = with_stray_timer(old);
        assert!(old.timer_stray(), "{what}");
        for new in &news {
            check(&format!("update: {what}"), &plan(&old, new));
        }
    }
    for (what, old, _) in enable_cases() {
        for scheduled in [true, false] {
            let old = Old {
                scheduled,
                ..with_stray_timer(&old)
            };
            check(&format!("enable (scheduled {scheduled}): {what}"), &enable_plan(&old));
        }
    }
}

/// `disable` boots out the timer, waiting for it, before it moves it, then moves the job, and never boots
/// the job out: a run in flight goes on.
#[skuld::test]
fn disable_plan_table() {
    let cases: Vec<(&str, Old, Vec<Action>)> = vec![
        (
            "everything in place and loaded: the timer goes out and to staging, then the job moves",
            placed(true, Some(true), BOTH),
            vec![BootoutTimer, MoveTimerToStaging, MoveJobToStaging],
        ),
        (
            "a crash that moved the timer but not the job: the job moves and nothing is booted out",
            placed(true, Some(false), JOB),
            vec![MoveJobToStaging],
        ),
        (
            "a crash after the timer went out and before it moved",
            placed(true, Some(true), JOB),
            vec![MoveTimerToStaging, MoveJobToStaging],
        ),
        (
            "everything staged: nothing to do",
            placed(false, Some(false), NOTHING),
            vec![],
        ),
        (
            "everything staged, the job loaded with a run in flight: it is left alone",
            placed(false, Some(false), JOB),
            vec![],
        ),
        (
            "a job without its timer: only the job moves",
            placed(true, None, JOB),
            vec![MoveJobToStaging],
        ),
        (
            "a timer label launchd holds with no timer file: it goes out all the same",
            placed(true, None, BOTH),
            vec![BootoutTimer, MoveJobToStaging],
        ),
        (
            "a timer loaded from staging goes out and stays there",
            placed(false, Some(false), BOTH),
            vec![BootoutTimer],
        ),
    ];
    for (what, old, expected) in cases {
        assert_eq!(disable_plan(&old), expected, "{what}");
    }
}
