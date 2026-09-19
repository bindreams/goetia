//! `goetia daemon restart <ID...>`
//!
//! Not a `ServiceManager` method of its own — `stop` then `start`, using the
//! trait's own verbs (both idempotent, per their doc comments: `stop` on an
//! already-stopped daemon is not an error) — except under a budget that does
//! not wait, on a manager with one request-only restart
//! (`ServiceManager::request_restart`). Does not change boot-enablement, same
//! as either verb alone.
//!
//! **One budget covers the whole restart, not each leg.** The user invoked a
//! single operation, and taking twice the time it named because the
//! operation happens to have two steps would be wrong — so [`restart`]
//! derives one deadline and hands both legs a budget from it. Per id, not
//! per invocation: `run_id_verb` calls this once per id, so
//! `restart a b c --timeout 30s` promises each of the three 30s rather than
//! leaving `c` whatever `a` and `b` did not spend.

use std::io::Write;
use std::time::Duration;

use clap::Args as ClapArgs;

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
) -> i32 {
    run_with(args, get_manager, is_elevated, &Budget::start, out, err)
}

/// [`run`] with the clock injected, so the module's two budget rules can be
/// tested by counting deadlines rather than by timing them.
///
/// Both rules are statements about how many deadlines a run derives and
/// when — one per [`restart`], one per id — and neither is observable
/// downstream: `Fake` consumes no wall-clock time, so a budget shared
/// across both legs and a fresh one per leg hand the manager
/// indistinguishable values. Counting the derivations decides it exactly,
/// and reads no clock at all.
fn run_with(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    start_clock: &dyn Fn(Budget) -> Deadline,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let budget = args.wait.budget();
    run_id_verb(
        IdVerbCall {
            subcommand: "daemon restart",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| restart(mgr, id, budget, start_clock),
            verb_past_tense: wait::reported(budget, "restarted", "restart requested"),
            absent_is_success: false,
        },
        out,
        err,
    )
}

/// One daemon's restart, spending `budget` across both legs.
///
/// Under a budget that waits the legs need nothing between them: the stop
/// leg is only ever handed a budget that waits (see [`stop_leg`]), so a
/// `stop` that returned `Ok` has confirmed stopped, and one that ran out
/// returned `WaitTimeout` instead, so the start is never reached. Under a
/// budget that does not wait there is nothing to confirm and deliberately
/// nothing is read — "don't wait, just do the steps", as one request where
/// the manager has one. Either way no state is queried between the two,
/// which is what keeps the exit code from being a stopwatch question.
fn restart(mgr: &dyn ServiceManager, id: &Id, budget: Budget, start_clock: &dyn Fn(Budget) -> Deadline) -> Result<()> {
    let deadline = start_clock(budget);
    // Before either leg sends anything, so that nothing either leg needs can
    // run out between the stop and the start and leave the daemon stopped.
    let _prepared = mgr.prepare(&[Step::Stop, Step::Start], budget)?;
    if !budget.waits() {
        // One request where the manager has one: nothing then happens
        // between the stop and the start, and a refusal changed nothing, so
        // its error stands as it is.
        if let Some(requested) = mgr.request_restart(id) {
            return requested;
        }
    }
    mgr.stop(id, stop_leg(budget, budget_for(deadline)))
        .map_err(|e| abandoned_before_start(id, e, budget))?;

    let started = match leg(budget, budget_for(deadline)) {
        Leg::Under(left) if left.waits() => mgr.start(id, left),
        // Not the idempotent `start`: over a stop nobody confirmed, a manager
        // answering "already running" may be seeing the instance that stop is
        // taking down, and that answer must not read as a restart.
        Leg::Under(_) => mgr.request_start_after_stop(id),
        Leg::Spent => return Err(spent_before_start(id, budget)),
    };
    started.map_err(|e| after_start_failed(id, budget, e))
}

// the abandonment rule ================================================================================================

/// The budget a leg of [`restart`] runs under, given the budget the user
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
/// load-bearing, twice over. `--timeout 0`'s deadline is expired from its
/// first instant, so "nothing left means spent" would drop both legs from
/// the one mode whose entire point is to issue them. And a spent bounded
/// budget must not reach a leg as `left` itself — [`budget_for`]'s
/// `Immediate` — because that means "do not wait": a stop issued under it
/// returns `Ok` having confirmed nothing, and would read as a stop that did.
fn leg(requested: Budget, left: Budget) -> Leg {
    if requested.waits() && !left.waits() {
        return Leg::Spent;
    }
    Leg::Under(left)
}

/// A bounded budget with nothing left, as the stop leg is handed one: the
/// least bound there is. It waits, so every backend takes the path a bounded
/// stop that ran out takes — the stop issued, then `WaitTimeout`, or `Ok` if
/// stopped is confirmed after all — and never `Immediate`'s, which on launchd
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

/// The stop leg's expiry, re-worded before it propagates — whether the stop
/// ran out of budget itself or the budget was spent before it was issued —
/// and so is a stop in doubt, which may have stopped the daemon too. The
/// variant is preserved (still exit `4`); the words change, because the
/// shared texts — [`budget::timed_out`]'s, and the in-doubt request's — speak
/// for a bare `stop` and disclose neither that the restart was abandoned nor
/// that the daemon may be left down, and so does `waited` (see
/// [`as_given`]). Every other failure passes through untouched: a stop that
/// determinately failed changed nothing and is still exit `1`.
fn abandoned_before_start(id: &Id, e: Error, budget: Budget) -> Error {
    match e {
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
        Error::RequestInDoubt {
            request,
            reached,
            detail,
        } => Error::RequestInDoubt {
            request,
            reached,
            detail: format!(
                "the restart was abandoned: no start was issued, so `{id}` may be left stopped. `goetia daemon \
                 status {id}` shows the manager's current view, and `goetia daemon start {id}` brings it back up: \
                 {detail}"
            ),
        },
        other => other,
    }
}

/// The start leg's [`Leg::Spent`]: the stop *succeeded* — confirmed, under a
/// budget that waited — and nothing of the budget was left to watch a start
/// with, whether the stop spent it or it was gone before the stop was issued.
///
/// `awaited` is `"running"` — the state the restart was asked to reach and
/// did not — rather than `"stopped"`, which it did reach and must not be
/// reported as missed.
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
/// Re-wrapped variant by variant rather than flattened to [`Error::Other`]:
/// `run_id_verb` reads the variant, and an `Undetermined` or `WaitTimeout`
/// start leg erased here would exit `1` in the states where the distinction
/// matters most — stopped and not back up, or stopped and still on its way.
fn after_start_failed(id: &Id, budget: Budget, e: Error) -> Error {
    match e {
        // First, and ahead of the non-waiting arm below, so that both paths
        // preserve it. [`Error::Unestablished`] says in as many words that
        // what is installed at the id was never in doubt; absorbing an
        // `Undetermined` start leg into it makes that false. Only the clause
        // differs between the paths, because only what the stop leg
        // established differs.
        Error::Undetermined { id, reason, recovery } => Error::Undetermined {
            id,
            reason: format!("{}: {reason}", after_stop_clause(budget)),
            recovery,
        },
        // The start may have reached the manager: kept as it is, ahead of the
        // non-waiting arm below, whose "refused" it was not, and never called a
        // failure to restart, which is what it did not establish.
        Error::RequestInDoubt {
            request,
            reached,
            detail,
        } => Error::RequestInDoubt {
            request,
            reached,
            detail: format!("{}: {detail}", start_after_stop_clause(id, budget)),
        },
        // The stop was issued and not waited for, and the start that
        // followed was refused — determinately (a launchd `bootstrap` that
        // fails outright) or not (SCM's `ERROR_SERVICE_ALREADY_RUNNING` over a
        // service still coming down). systemd never gets here: its restart is
        // one request. Either way nothing confirmed the stop,
        // which may be in flight, done, or have had nothing to do, so where
        // the daemon ended up is what nobody established — not `Other`,
        // exit `1`, whose answer a refusal alone would be.
        e if !budget.waits() => Error::Unestablished {
            id: id.as_str().to_string(),
            detail: format!(
                "the stop was issued without waiting for it, and the start that followed was \
                 refused: {e}. `goetia daemon status {id}` shows the manager's current view."
            ),
        },
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
        other => Error::Other(format!("stopped but failed to restart: {other}")),
    }
}

/// How long a restart's expiry reports having waited: the `--timeout` the
/// user gave, as plain `stop` reports it. A leg's own budget is only what
/// was left of that one, and `restart` is one operation with one budget.
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
/// off. A budget that waited saw `stop` return `Ok`, which is a confirmed
/// stop; one that did not wait confirmed nothing, and must not borrow the
/// waiting path's wording to claim otherwise.
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
