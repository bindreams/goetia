//! `goetia daemon restart <ID...>`
//!
//! Not a `ServiceManager` method of its own — `stop` then `start`, using the
//! trait's own verbs. Does not change boot-enablement, same as either verb
//! alone.

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::{parse_id, require_elevation};
use crate::error::Result;
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
    if let Err(code) = require_elevation("daemon restart", is_elevated, err) {
        return code;
    }
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    let mut exit = 0;
    for id_str in &args.ids {
        let id = match parse_id(id_str) {
            Ok(id) => id,
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                exit = 1;
                continue;
            }
        };
        match mgr.stop(&id).and_then(|()| mgr.start(&id)) {
            Ok(()) => {
                let _ = writeln!(out, "{id}: restarted");
            }
            Err(e) => {
                let _ = writeln!(err, "error: {id}: {e}");
                exit = 1;
            }
        }
    }
    exit
}
