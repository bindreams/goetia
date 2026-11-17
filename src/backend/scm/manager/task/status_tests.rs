use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::backend::scm::instance::InstanceId;
use crate::manager::State;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn instance(n: u8, pid: u32) -> Instance {
    Instance {
        id: InstanceId::parse(&format!("{n:032x}")).unwrap(),
        pid,
    }
}

fn info(last_result: i32) -> TaskInfo {
    TaskInfo {
        last_run: Some(at(1_000)),
        last_result,
        next_run: Some(at(2_000)),
    }
}

// exit_of =============================================================================================================

// M1: `LastTaskResult` is the exit code verbatim, beside the `SCHED_S_*` statuses Task Scheduler
// writes in its place.
#[skuld::test]
fn exit_of_reads_the_codes_m1_measured() {
    // `SCHED_S_TASK_HAS_NOT_RUN`: nothing has finished.
    assert_eq!(exit_of(0x0004_1303), None);
    // `SCHED_S_TASK_RUNNING`: nothing has finished either.
    assert_eq!(exit_of(0x0004_1301), None);
    assert_eq!(exit_of(0), Some(Exit::Code(0)));
    assert_eq!(exit_of(7), Some(Exit::Code(7)));
    // `SCHED_S_TASK_TERMINATED`, which any `Stop` leaves whatever the program exited with, is the
    // code a console close event ends a process with.
    assert_eq!(exit_of(0x0004_1306), Some(Exit::Code(0xC000_013A_u32 as i32)));
    // A raw code is its bit pattern, as a `TerminateProcess` leaves it.
    assert_eq!(
        exit_of(0xC0DE_0011_u32 as i32),
        Some(Exit::Code(0xC0DE_0011_u32 as i32))
    );
    assert_eq!(exit_of(-1), Some(Exit::Code(-1)));
}

// task_status =========================================================================================================

#[skuld::test]
fn no_live_instance_is_idle_and_reports_the_last_run() {
    let status = task_status(&info(7), &[], true);

    assert_eq!(status.common.state, State::Idle);
    assert_eq!(status.common.pid, None);
    assert!(status.common.enabled);
    assert_eq!(status.last_result, Some(Exit::Code(7)));
    assert_eq!(status.last_run, Some(at(1_000)));
    assert_eq!(status.next_run, Some(at(2_000)));
}

#[skuld::test]
fn a_live_instance_is_running_with_the_first_instances_pid() {
    let live = [instance(1, 4242), instance(2, 9999)];

    let status = task_status(&info(0x0004_1301), &live, true);

    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.common.pid, Some(4242));
    assert_eq!(status.last_result, None, "a run in flight has no exit yet");
}

/// The status of Task Scheduler alone does not make a task run: a run is a live process.
#[skuld::test]
fn a_running_result_with_no_live_instance_is_idle() {
    let status = task_status(&info(0x0004_1301), &[], true);

    assert_eq!(status.common.state, State::Idle);
    assert_eq!(status.last_result, None);
}

#[skuld::test]
fn a_task_that_never_ran_has_no_result_and_no_last_run() {
    let never = TaskInfo {
        last_run: None,
        last_result: 0x0004_1303,
        next_run: None,
    };

    let status = task_status(&never, &[], false);

    assert_eq!(status.last_result, None);
    assert_eq!(status.last_run, None);
    assert_eq!(status.next_run, None);
}

// M1: Task Scheduler computes a next run for triggers that are disabled too.
#[skuld::test]
fn next_run_is_none_for_a_disarmed_task_whatever_the_scheduler_reports() {
    let status = task_status(&info(0), &[], false);

    assert!(!status.common.enabled);
    assert_eq!(status.next_run, None);
}
