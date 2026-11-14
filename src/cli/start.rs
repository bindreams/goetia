//! `goetia daemon start <ID...>`

use std::cell::Cell;
use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{IdVerbCall, run_id_verb};
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
    // Whether the id `verb` last started is a task, which only requests a run.
    let task = Cell::new(false);
    run_id_verb(
        IdVerbCall {
            subcommand: "daemon start",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| {
                task.set(false);
                // The kind is the installed blob's. A start without a flag has nothing to refuse, so
                // it only takes the word for its report from it: a blob that will not decode is
                // `start`'s to report, as it was before the kind was asked.
                match mgr.kind(id) {
                    Ok(kind) => task.set(kind == DaemonKind::Task),
                    Err(e) if args.wait.was_given() => return Err(e),
                    Err(_) => {}
                }
                if task.get() && args.wait.was_given() {
                    return Err(crate::spec::invalid(id, wait::NOT_FOR_A_TASK));
                }
                mgr.start(id, budget)
            },
            verb_past_tense: &|| {
                if task.get() {
                    "start requested"
                } else {
                    wait::reported(budget, "started", "start requested")
                }
            },
            absent_is_success: false,
        },
        out,
        err,
    )
}
