//! `goetia daemon stop <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{WordedVerbCall, run_worded_verb};
use super::wait::{self, WaitArgs};
use crate::error::Result;
use crate::manager::{Budget, ServiceManager};

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
            // With no flag the wait is each daemon's own, which always waits, so the word follows the flag.
            verb: &|mgr, id| {
                let given = args.wait.given();
                mgr.stop(id, given)?;
                Ok(wait::reported(
                    given.unwrap_or(Budget::DEFAULT),
                    "stopped",
                    "stop requested",
                ))
            },
            absent_is_success: false,
        },
        out,
        err,
    )
}
