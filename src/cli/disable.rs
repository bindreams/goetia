//! `goetia daemon disable <ID...>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::{IdVerbCall, run_id_verb};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to disable at boot. Does not stop them if running.
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
            subcommand: "daemon disable",
            ids: &args.ids,
            get_manager,
            is_elevated,
            verb: &|mgr, id| mgr.disable(id),
            verb_past_tense: "disabled",
        },
        out,
        err,
    )
}
