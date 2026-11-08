//! `goetia daemon restart <ID...>`
//!
//! `stop` then `start`, using the trait's own idempotent verbs, except under a
//! budget that does not wait, on a manager with one request-only restart
//! (`ServiceManager::request_restart`). Does not change boot-enablement.
//!
//! **One budget covers the whole restart, not each leg:** [`restart`] starts
//! one deadline and hands both legs a budget from it. Per id, not per
//! invocation: `restart a b c --timeout 30s` gives each of the three 30s.

use std::io::Write;
use std::time::Duration;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{IdVerbCall, run_id_verb};
use super::wait::{self, WaitArgs};
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline, budget_for};
use crate::manager::{Budget, ServiceManager, Step};
use crate::spec::Id;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to restart.
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[command(flatten)]
    pub wait: WaitArgs,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let budget = args.wait.budget();
    run_id_verb(
        IdVerbCall {
            subcommand: "daemon restart",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| restart(mgr, id, budget),
            verb_past_tense: wait::reported(budget, "restarted", "restart requested"),
            absent_is_success: false,
        },
        out,
        err,
    )
}

/// One daemon's restart, under one deadline started here.
fn restart(mgr: &dyn ServiceManager, id: &Id, budget: Budget) -> Result<()> {
    restart_until(mgr, id, budget, budget.start())
}

/// [`restart`], under a `deadline` that may already be spent.
///
/// Under a budget that waits the legs need nothing between them: the stop leg
/// is only ever handed a budget that waits (see [`stop_leg`]), so a `stop` that
/// returned `Ok` has confirmed stopped, and one that ran out returned
/// `WaitTimeout` or `ProcessWaitTimeout`, so the start is never reached. Under a budget that does not
/// wait, nothing is confirmed or read. Either way no state is queried between
/// the legs, which keeps the exit code from being a stopwatch question.
fn restart_until(mgr: &dyn ServiceManager, id: &Id, budget: Budget, deadline: Deadline) -> Result<()> {
    // Before either leg sends anything, so that nothing either leg needs can
    // run out between the stop and the start and leave the daemon stopped.
    let mut sequence = mgr.prepare(&[Step::Stop, Step::Start], budget)?;
    if !budget.waits() {
        // One request where the manager has one; a refusal changed nothing, so
        // its error stands as it is.
        if let Some(requested) = mgr.request_restart(id) {
            return requested;
        }
    }
    sequence
        .stop(id, stop_leg(budget, budget_for(deadline)))
        .map_err(|e| abandoned_before_start(id, e, budget))?;

    let started = match leg(budget, budget_for(deadline)) {
        Leg::Under(left) if left.waits() => sequence.start(id, left),
        // Not the idempotent `start`: over a stop nobody confirmed, a manager
        // answering "already running" may be seeing the instance that stop is
        // taking down, and that answer must not read as a restart.
        Leg::Under(_) => sequence.request_start_after_stop(id),
        Leg::Spent => return Err(spent_before_start(id, budget)),
    };
    started.map_err(|e| after_start_failed(id, budget, e))
}

// the abandonment rule ================================================================================================

/// The budget a leg of [`restart_until`] runs under, given the budget the user
/// named and what is left of it.
#[derive(Debug, PartialEq, Eq)]
enum Leg {
    Under(Budget),
    /// A bounded budget ran out before this leg: a timeout. The start leg
    /// is then not issued; the stop leg still is, under [`SPENT`].
    Spent,
}

/// A bounded budget that runs out anywhere is a timeout, and the start leg
/// is not issued: goetia does not start into an unconfirmed stop, and does
/// not silently downgrade a restart the user bounded into a
/// requested-but-unconfirmed start.
///
/// Branching on the *requested* budget and not on `left` alone is
/// load-bearing. `--timeout 0`'s deadline is expired from its first instant,
/// so "nothing left means spent" would drop both legs from the one mode whose
/// point is to issue them. And a spent bounded budget must not reach a leg as
/// `left` itself ([`budget_for`]'s `Immediate`, "do not wait"): a stop issued
/// under it returns `Ok` having confirmed nothing.
fn leg(requested: Budget, left: Budget) -> Leg {
    if requested.waits() && !left.waits() {
        return Leg::Spent;
    }
    Leg::Under(left)
}

/// A bounded budget with nothing left, as the stop leg is handed one: the
/// least bound there is. It waits, so every backend takes the path of a
/// bounded stop that ran out (stop issued, then `WaitTimeout`, or `Ok` if
/// stopped is confirmed after all), never `Immediate`'s, which on launchd
/// runs `bootout` to completion unbounded.
const SPENT: Budget = Budget::Bounded(Duration::from_nanos(1));

/// The budget the stop leg is issued under. Always one: the stop goes out
/// whatever is left of the budget, and a bounded budget already spent is
/// handed on as [`SPENT`] rather than dropped or turned into `Immediate`.
fn stop_leg(requested: Budget, left: Budget) -> Budget {
    match leg(requested, left) {
        Leg::Under(left) => left,
        Leg::Spent => SPENT,
    }
}

// wording =============================================================================================================

/// The stop leg's expiry (the stop, or the exit of the process it stopped, ran
/// out of budget, or the budget was spent before it was issued), re-worded
/// before it propagates: it is a stop in doubt, which may have stopped the
/// daemon. The variant is preserved (exit `4`); the words change because the
/// shared texts ([`budget::timed_out`]'s, [`budget::process_timed_out`]'s and
/// the in-doubt request's) speak for a bare `stop` and do not say the restart
/// was abandoned or that the daemon may be left down; `waited` is
/// likewise re-based (see [`as_given`]). Every other failure passes through: a
/// stop that determinately failed changed nothing and is still exit `1`.
fn abandoned_before_start(id: &Id, e: Error, budget: Budget) -> Error {
    e.map_cause(|leaf| match leaf {
        Error::WaitTimeout {
            id, awaited, waited, ..
        } => Error::WaitTimeout {
            waited: as_given(budget, waited),
            recovery: format!(
                "the restart was abandoned: no start was issued, so `{id}` may be left stopped — \
                 and the stop was not cancelled, so it may or may not have reached stopped by the \
                 time you look. `goetia daemon status {id}` shows the manager's current view, and \
                 `goetia daemon start {id}` brings it back up. Allow longer with `--timeout \
                 <duration>`, or wait indefinitely with `--no-timeout`."
            ),
            id,
            awaited,
        },
        // SCM confirmed the stop; its process had not exited when the budget ran out.
        Error::ProcessWaitTimeout {
            id,
            pid,
            waited,
            terminate_failed,
            ..
        } => Error::ProcessWaitTimeout {
            terminate_failed,
            waited: as_given(budget, waited),
            recovery: format!(
                "the restart was abandoned: no start was issued, so `{id}` is left stopped, while its \
                 process keeps the files it runs in use until it exits. `goetia daemon status {id}` shows \
                 the manager's current view, and `goetia daemon start {id}` brings it back up. Allow \
                 longer with `--timeout <duration>`, or wait indefinitely with `--no-timeout`."
            ),
            id,
            pid,
        },
        e @ Error::RequestInDoubt { .. } => e.in_doubt_context(format!(
            "the restart was abandoned: no start was issued, so `{id}` may be left stopped. `goetia daemon \
             status {id}` shows the manager's current view, and `goetia daemon start {id}` brings it back up"
        )),
        other => other,
    })
}

/// The start leg's [`Leg::Spent`]: the stop *succeeded* (confirmed, under a
/// budget that waited) and nothing of the budget was left to watch a start
/// with.
///
/// `awaited` is `"running"`, the state the restart did not reach, not
/// `"stopped"`, which it did.
fn spent_before_start(id: &Id, budget: Budget) -> Error {
    match budget::timed_out(id.as_str(), "running", budget) {
        Error::WaitTimeout {
            id, awaited, waited, ..
        } => Error::WaitTimeout {
            recovery: format!(
                "`{id}` was stopped, and the budget ran out before a start could be issued. The \
                 restart was abandoned: no start was issued, so `{id}` is left stopped. `goetia \
                 daemon start {id}` brings it back up. Allow longer with `--timeout <duration>`, \
                 or wait indefinitely with `--no-timeout`."
            ),
            id,
            awaited,
            waited,
        },
        other => other,
    }
}

/// The start leg's failure, re-wrapped to say what the stop leg established.
///
/// `run_id_verb` reads the variant, and an `Undetermined` or `WaitTimeout`
/// start leg erased here would exit `1` in the states where the distinction
/// matters most — stopped and not back up, or stopped and still on its way.
fn after_start_failed(id: &Id, budget: Budget, e: Error) -> Error {
    let reworded = |leaf| rewrapped_start_leg(id, budget, leaf);
    match e.cause() {
        // Ahead of the non-waiting arm below, so both paths preserve these:
        // [`Error::Unestablished`] says what is installed at the id was never
        // in doubt, which an `Undetermined` start leg contradicts; and a
        // `RequestInDoubt` start may have reached the manager, so it was not
        // "refused" and not a failure to restart. Only the clause differs
        // between the paths, because only what the stop leg established differs.
        Error::Undetermined { .. } | Error::RequestInDoubt { .. } => e.map_cause(reworded),
        // The stop was issued and not waited for, and the start that followed
        // was refused: determinately (a launchd `bootstrap` that fails
        // outright) or not (SCM's `ERROR_SERVICE_ALREADY_RUNNING` over a
        // service still coming down). systemd never gets here: its restart is
        // one request. Nothing confirmed the stop, so where the daemon ended
        // up is what nobody established.
        _ if !budget.waits() => Error::Unestablished {
            id: id.as_str().to_string(),
            refused: Box::new(e),
        },
        Error::WaitTimeout { .. } => e.map_cause(reworded),
        _ => Error::Step {
            what: "stopped but failed to restart".to_string(),
            source: Box::new(e),
        },
    }
}

/// The cause of a start leg's failure, re-worded to say what the stop leg established. Called only
/// for the three it re-words.
fn rewrapped_start_leg(id: &Id, budget: Budget, leaf: Error) -> Error {
    match leaf {
        Error::Undetermined { id, reason, recovery } => Error::Undetermined {
            id,
            reason: Box::new(Error::Step {
                what: after_stop_clause(budget).to_string(),
                source: reason,
            }),
            recovery,
        },
        e @ Error::RequestInDoubt { .. } => e.in_doubt_context(start_after_stop_clause(id, budget)),
        Error::WaitTimeout {
            id,
            awaited,
            waited,
            recovery,
        } => Error::WaitTimeout {
            recovery: format!("`{id}` was stopped and the start was issued. {recovery}"),
            waited: as_given(budget, waited),
            id,
            awaited,
        },
        other => {
            debug_assert!(
                false,
                "only the causes `after_start_failed` re-words reach here, got {other:?}"
            );
            other
        }
    }
}

/// How long a restart's expiry reports having waited: the `--timeout` the user
/// gave, as plain `stop` reports it, not the leg's remainder.
fn as_given(budget: Budget, leg_waited: Duration) -> Duration {
    match budget {
        Budget::Bounded(given) => given,
        Budget::Immediate | Budget::Unbounded => leg_waited,
    }
}

/// What the stop leg established, as the clause a start in doubt hangs off:
/// that it was attempted, never that it failed.
fn start_after_stop_clause(id: &Id, budget: Budget) -> String {
    if budget.waits() {
        format!("`{id}` was stopped, and this start was attempted after it")
    } else {
        "the stop was issued without waiting for it, and this start was attempted after it".to_string()
    }
}

/// What the stop leg established, as the clause a start leg's failure hangs
/// off. A budget that waited saw `stop` return `Ok`, a confirmed stop; one
/// that did not wait confirmed nothing and must not claim otherwise.
fn after_stop_clause(budget: Budget) -> &'static str {
    if budget.waits() {
        "stopped but failed to restart"
    } else {
        "the stop was issued without waiting for it, and the start that followed was not determined"
    }
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod restart_tests;
