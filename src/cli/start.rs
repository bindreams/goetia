//! `goetia daemon start <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{IdVerbCall, run_id_verb};
use super::wait::{self, WaitArgs};
use crate::error::Result;
use crate::manager::{ServiceManager, Status};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to start. Does not change boot-enablement. A task's start returns as soon as the
    /// manager has accepted the run, so `--timeout` and `--no-timeout` do not apply to one.
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
            subcommand: "daemon start",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| {
                // Read only when a flag was given, so a start without one reads nothing more than before.
                // A status that fails falls through: `start` reports what is wrong with the id.
                if args.wait.was_given() && matches!(mgr.status(id), Ok(Status::Task(_))) {
                    return Err(crate::spec::invalid(id, wait::NOT_FOR_A_TASK));
                }
                mgr.start(id, budget)
            },
            verb_past_tense: wait::reported(budget, "started", "start requested"),
            absent_is_success: false,
        },
        out,
        err,
    )
}
