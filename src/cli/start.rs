//! `goetia daemon start <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{WordedVerbCall, run_worded_verb};
use super::wait::{self, WaitArgs};
use crate::error::Result;
use crate::manager::ServiceManager;
use crate::spec::DaemonKind;

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
    run_worded_verb(
        WordedVerbCall {
            subcommand: "daemon start",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| {
                // A flag is read against the installed blob's kind before anything is sent. A
                // start without one reads no more than it did: it learns the kind from the start.
                if args.wait.was_given() && mgr.kind(id)? == DaemonKind::Task {
                    return Err(crate::spec::invalid(id, wait::NOT_FOR_A_TASK));
                }
                // The word follows what was started: a service reports the budget's, a task only
                // requested its run, and a start that could not read the kind claims no more.
                Ok(match mgr.start(id, budget)? {
                    Some(DaemonKind::Service) => wait::reported(budget, "started", "start requested"),
                    Some(DaemonKind::Task) | None => "start requested",
                })
            },
            absent_is_success: false,
        },
        out,
        err,
    )
}
