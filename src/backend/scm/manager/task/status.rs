//! A task's status from what Task Scheduler says of it and what is alive. Pure.

use super::api::{Instance, TaskInfo};
use crate::manager::{DaemonStatus, Exit, State, TaskStatus};

/// `SCHED_S_TASK_RUNNING`: what `LastTaskResult` reads while a run is in flight.
const RUNNING: i32 = 0x0004_1301;
/// `SCHED_S_TASK_HAS_NOT_RUN`: what `LastTaskResult` reads before the first run.
const NOT_RUN: i32 = 0x0004_1303;
/// `SCHED_S_TASK_TERMINATED`: what `LastTaskResult` reads after any `Stop`, whatever the program
/// exited with.
const TERMINATED: i32 = 0x0004_1306;
/// What a console close event ends a process with (`STATUS_CONTROL_C_EXIT`): the code of a run that a
/// stop ended.
pub(super) const CLOSED: u32 = 0xC000_013A;

/// How the last finished run ended, from Task Scheduler's `LastTaskResult`.
///
/// The result is the program's exit code verbatim, as the bit pattern of an `i32`, except for the
/// statuses Task Scheduler writes in its place: a run that has not finished, or never started, has no
/// exit, and one that was stopped ended as a console close event ends a process. Never a Task
/// Scheduler status shown as a code.
pub(super) fn exit_of(last_result: i32) -> Option<Exit> {
    match last_result {
        NOT_RUN | RUNNING => None,
        TERMINATED => Some(Exit::Code(CLOSED as i32)),
        code => Some(Exit::Code(code)),
    }
}

/// The state of a task whose live instances are `live`, and the pid it runs as. A task is `Running`
/// while any instance is alive, and `pid` is the first one's. A listing or a probe that failed has no
/// live instances to give: it is the call's error, never a state.
pub(super) fn run_state(live: &[Instance]) -> (State, Option<u32>) {
    match live {
        [] => (State::Idle, None),
        [first, ..] => (State::Running, Some(first.pid)),
    }
}

/// The status of a task whose live instances are `live` (see [`run_state`]). Its `last_result` is
/// [`exit_of`]. A disarmed task has no next run whatever Task Scheduler reports, which computes one
/// for disabled triggers too; `enabled` is `armed`.
pub(super) fn task_status(info: &TaskInfo, live: &[Instance], armed: bool) -> TaskStatus {
    let (state, pid) = run_state(live);
    TaskStatus {
        common: DaemonStatus {
            state,
            pid,
            enabled: armed,
        },
        last_result: exit_of(info.last_result),
        last_run: info.last_run,
        next_run: info.next_run.filter(|_| armed),
    }
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod status_tests;
