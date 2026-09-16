//! `goetia daemon restart <ID...>`
//!
//! Not a `ServiceManager` method of its own — `stop` then `start`, using the
//! trait's own verbs (both idempotent, per their doc comments: `stop` on an
//! already-stopped daemon is not an error). Does not change boot-enablement,
//! same as either verb alone.

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::{IdVerbCall, run_id_verb};
use crate::error::{Error, Result};
use crate::manager::{Budget, ServiceManager};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to restart.
    #[arg(required = true)]
    pub ids: Vec<String>,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    run_id_verb(
        IdVerbCall {
            subcommand: "daemon restart",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| {
                mgr.stop(id, Budget::DEFAULT)?;
                // Distinguish "never stopped, restart failed outright" from
                // "stopped, but did not come back up" — the two have
                // opposite operational consequences (still running vs. now
                // down), and `mgr.start`'s own error alone cannot tell them
                // apart once relayed through this closure.
                // Re-wrapped variant by variant rather than flattened to
                // `Other`: `run_id_verb` reads the variant, and an
                // `Undetermined` start leg erased here would exit `1` in the
                // one state where the distinction matters most.
                mgr.start(id, Budget::DEFAULT).map_err(|e| match e {
                    Error::Undetermined { id, reason, recovery } => Error::Undetermined {
                        id,
                        reason: format!("stopped but failed to restart: {reason}"),
                        recovery,
                    },
                    other => Error::Other(format!("stopped but failed to restart: {other}")),
                })
            },
            verb_past_tense: "restarted",
            absent_is_success: false,
        },
        out,
        err,
    )
}
