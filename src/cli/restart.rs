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
use crate::manager::ServiceManager;

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
                mgr.stop(id)?;
                // Distinguish "never stopped, restart failed outright" from
                // "stopped, but did not come back up" — the two have
                // opposite operational consequences (still running vs. now
                // down), and `mgr.start`'s own error alone cannot tell them
                // apart once relayed through this closure.
                mgr.start(id)
                    .map_err(|e| Error::Other(format!("stopped but failed to restart: {e}")))
            },
            verb_past_tense: "restarted",
        },
        out,
        err,
    )
}
