//! `goetia daemon stop <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{WordedVerbCall, run_worded_verb};
use super::wait::{self, WaitArgs};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to stop. Does not change boot-enablement.
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
    run_worded_verb(
        WordedVerbCall {
            subcommand: "daemon stop",
            ids: &args.ids,
            get_manager,
            is_elevated,
            // The wait is each daemon's own, so the word follows it.
            verb: &|mgr, id| {
                let budget = args.wait.stop_budget(|| mgr.stop_action(id));
                mgr.stop(id, budget)?;
                Ok(wait::reported(budget, "stopped", "stop requested"))
            },
            absent_is_success: false,
        },
        out,
        err,
    )
}
