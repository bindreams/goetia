//! `goetia daemon status [ID...]`
//!
//! Read-only: never checks elevation.

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::{parse_id, partition_installed, print_unreadable_warnings, state_str};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to report on. With none, reports every installed daemon.
    pub ids: Vec<String>,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    if args.ids.is_empty() {
        return status_all(mgr.as_ref(), out, err);
    }

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
        match mgr.status(&id) {
            Ok(status) => {
                let _ = writeln!(
                    out,
                    "{id}: {} (enabled={}, pid={})",
                    state_str(status.state),
                    status.enabled,
                    status.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string()),
                );
            }
            Err(e) => {
                let _ = writeln!(err, "error: {id}: {e}");
                exit = 1;
            }
        }
    }
    exit
}

fn status_all(mgr: &dyn ServiceManager, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let installed = match mgr.list() {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    let index = partition_installed(installed);
    print_unreadable_warnings(&index.unreadable, err);

    for (id, (_spec, state, enabled)) in &index.ours {
        let _ = writeln!(out, "{id}: {} (enabled={enabled})", state_str(*state));
    }

    if index.unreadable.is_empty() { 0 } else { 1 }
}
