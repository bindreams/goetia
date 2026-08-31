//! `goetia daemon list`
//!
//! Read-only: never checks elevation.

use std::io::Write;

use super::support::{partition_installed, print_unreadable_warnings, state_str};
use crate::error::Result;
use crate::manager::ServiceManager;

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

    let index = partition_installed(installed);
    print_unreadable_warnings(&index.unreadable, err);

    for (id, (spec, state, enabled)) in &index.ours {
        let _ = writeln!(out, "{id}\t{}\t{}\tenabled={enabled}", spec.name, state_str(*state));
    }

    if index.unreadable.is_empty() { 0 } else { 1 }
}
