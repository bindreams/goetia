//! `goetia daemon list`
//!
//! Read-only: never checks elevation.

use std::io::Write;

use crate::error::Result;
use crate::manager::{Installed, ServiceManager, State};

fn state_str(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Stopped => "stopped",
        State::Failed => "failed",
        State::Unknown => "unknown",
    }
}

pub fn run(get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

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
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\tenabled={enabled}",
                    spec.id,
                    spec.name,
                    state_str(*state)
                );
            }
            Installed::OursUnreadable { name, reason } => {
                let _ = writeln!(err, "warning: {name}: installed but unreadable: {reason}");
                exit = 1;
            }
        }
    }
    exit
}
