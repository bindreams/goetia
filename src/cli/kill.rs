//! `goetia daemon kill <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::{IdVerbCall, run_id_verb};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids whose running process to kill, as a failure (SIGKILL, or the platform's
    /// equivalent). Returns once that is delivered (on Windows, once SCM has recorded the
    /// exit); what follows is the daemon's restart policy. A daemon that is not running is an error.
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
            subcommand: "daemon kill",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| mgr.kill(id),
            verb_past_tense: "kill delivered",
            absent_is_success: false,
        },
        out,
        err,
    )
}
