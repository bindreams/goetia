//! `goetia daemon run <ID>`

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{parse_id, print_error, require_elevation};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The idle task to run once now. Its output is shown as it is written, and goetia exits with
    /// the program's own exit code, or `128` plus the signal that ended it.
    pub id: String,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let id = match parse_id(&args.id) {
        Ok(id) => id,
        Err(e) => {
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };
    if let Err(code) = require_elevation("daemon run", is_elevated, err) {
        return code;
    }
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    match mgr.run(&id, out) {
        Ok(exit) => ExitCode::of_run(exit),
        Err(e) => {
            print_error(err, Some(id.as_str()), "", &e);
            ExitCode::of(&e)
        }
    }
}
