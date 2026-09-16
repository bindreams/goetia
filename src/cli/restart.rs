//! `goetia daemon restart <ID...>`
//!
//! Not a `ServiceManager` method of its own — `stop` then `start`, using the
//! trait's own verbs (both idempotent, per their doc comments: `stop` on an
//! already-stopped daemon is not an error). Does not change boot-enablement,
//! same as either verb alone.
//!
//! **One budget covers the whole restart, not each leg.** The user invoked a
//! single operation, and taking twice the time it named because the
//! operation happens to have two steps would be wrong — so [`restart`]
//! derives one deadline and hands both legs a budget from it. Per id, not
//! per invocation: `run_id_verb` calls this once per id, so
//! `restart a b c --timeout 30s` promises each of the three 30s rather than
//! leaving `c` whatever `a` and `b` did not spend.

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::{IdVerbCall, run_id_verb};
use super::wait::{self, WaitArgs};
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline, budget_for};
use crate::manager::{Budget, ServiceManager};
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
/// Under a budget that waits the legs need nothing between them: a `stop`
/// that returned `Ok` has confirmed stopped, and one that ran out returned
/// `WaitTimeout` instead, so the start is never reached. Under a budget that
/// does not wait there is nothing to confirm and deliberately nothing is
/// read — "don't wait, just do the steps". Either way no state is queried
/// between the two, which is what keeps the exit code from being a
/// stopwatch question.
fn restart(mgr: &dyn ServiceManager, id: &Id, budget: Budget, start_clock: &dyn Fn(Budget) -> Deadline) -> Result<()> {
    let deadline = start_clock(budget);
    mgr.stop(id, budget_for(deadline)).map_err(abandoned_before_start)?;

    let start_budget = match after_stop(budget, budget_for(deadline)) {
        NextLeg::Start(left) => left,
        NextLeg::Abandon => return Err(spent_before_start(id, budget)),
    };
    mgr.start(id, start_budget)
        .map_err(|e| after_start_failed(id, budget, e))
}

// the abandonment rule ================================================================================================

/// What [`restart`] does after its stop leg returned `Ok`, given the budget
/// the user named and what is left of it.
#[derive(Debug, PartialEq, Eq)]
enum NextLeg {
    /// Issue the start with this budget.
    Start(Budget),
    /// Do not issue it at all.
    Abandon,
}

/// A bounded budget that runs out anywhere is a timeout, and the start leg
/// is not issued: goetia does not start into an unconfirmed stop, and does
/// not silently downgrade a restart the user bounded into a
/// requested-but-unconfirmed start.
///
/// Branching on the *requested* budget and not on `left` alone is
/// load-bearing. `--timeout 0`'s deadline is expired from its first instant,
/// so "nothing left means abandon" would drop the start leg from the one
/// mode whose entire point is to issue both steps.
fn after_stop(requested: Budget, left: Budget) -> NextLeg {
    if requested.waits() && !left.waits() {
        return NextLeg::Abandon;
    }
    NextLeg::Start(left)
}

// wording =============================================================================================================

/// The stop leg's own expiry, re-worded before it propagates. The variant is
/// preserved — this is still exit `4` — and only the remedy changes:
/// [`budget::timed_out`]'s shared text speaks for a bare `stop` and
/// discloses neither that the restart was abandoned nor that the daemon is
/// left down. Every other failure passes through untouched: a stop that
/// determinately failed changed nothing and is still exit `1`.
fn abandoned_before_start(e: Error) -> Error {
    match e {
        Error::WaitTimeout {
            id, awaited, waited, ..
        } => Error::WaitTimeout {
            recovery: format!(
                "the restart was abandoned: no start was issued, so `{id}` may be left stopped — \
                 and the stop was not cancelled, so it may or may not have reached stopped by the \
                 time you look. `goetia daemon status {id}` shows the manager's current view, and \
                 `goetia daemon start {id}` brings it back up. Allow longer with `--timeout \
                 <duration>`, or wait indefinitely with `--no-timeout`."
            ),
            id,
            awaited,
            waited,
        },
        other => other,
    }
}

/// The other way to reach [`NextLeg::Abandon`]: the stop *succeeded* and
/// consumed the whole budget, leaving nothing to watch a start with.
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
        // Neither leg confirmed anything: the stop was issued and not waited
        // for, and the start that followed was refused by a manager that may
        // simply be mid-stop (SCM's `ERROR_SERVICE_ALREADY_RUNNING` over a
        // service still coming down). Whether the daemon cycled, is going
        // down, or never moved is precisely what nobody established — so not
        // `Other`, which is exit `1` and claims the restart determinately
        // failed.
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
            id,
            awaited,
            waited,
        },
        other => Error::Other(format!("stopped but failed to restart: {other}")),
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
