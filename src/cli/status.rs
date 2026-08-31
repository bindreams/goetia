//! `goetia daemon status [ID...]`
//!
//! Read-only: never checks elevation.

use std::io::Write;

use clap::Args as ClapArgs;

use super::support::parse_id;
use crate::error::Result;
use crate::manager::{Installed, ServiceManager, State};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to report on. With none, reports every installed daemon.
    pub ids: Vec<String>,
}

fn state_str(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Stopped => "stopped",
        State::Failed => "failed",
        State::Unknown => "unknown",
    }
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

    let mut exit = 0;
    for entry in &installed {
        match entry {
            Installed::Ours { spec, state, enabled } => {
                let _ = writeln!(out, "{}: {} (enabled={enabled})", spec.id, state_str(*state));
            }
            Installed::OursUnreadable { name, reason } => {
                let _ = writeln!(err, "warning: {name}: installed but unreadable: {reason}");
                exit = 1;
            }
        }
    }
    exit
}
