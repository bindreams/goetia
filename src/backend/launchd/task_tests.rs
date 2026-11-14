use std::sync::Arc;

use super::*;
use crate::backend::Identity;
use crate::backend::launchd::generate;
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
