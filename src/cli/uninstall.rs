//! `goetia daemon uninstall <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{IdVerbCall, run_id_verb};
use crate::error::Result;
use crate::manager::{RunInFlight, ServiceManager};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to uninstall. Reads the installed artifact, not any
    /// manifest — this works on a machine where the original clone is gone.
    #[arg(required = true)]
    pub ids: Vec<String>,

    /// Leave a run in flight alone: remove the daemon and whatever would start another run, send the run
    /// nothing. For a program that uninstalls its own daemon from inside a run.
    #[arg(long)]
    pub no_stop: bool,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let in_flight = if args.no_stop {
        RunInFlight::Leave
    } else {
        RunInFlight::Stop
    };
    run_id_verb(
        IdVerbCall {
            subcommand: "daemon uninstall",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| mgr.uninstall(id, in_flight),
            verb_past_tense: "uninstalled",
            absent_is_success: true,
        },
        out,
        err,
    )
}
