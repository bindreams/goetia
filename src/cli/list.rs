//! `goetia daemon list`
//!
//! Read-only: never checks elevation.

use std::io::Write;

use super::report;
use super::support::{
    InstalledIndex, partition_installed, print_undetermined_warnings, print_unreadable_warnings, state_str,
};
use crate::error::Result;
use crate::manager::ServiceManager;

pub fn run(
    json: bool,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // The `Report` is built in both modes, and both take their exit code
    // from it — see [`report`]'s module doc comment.
    let index = get_manager().and_then(|mgr| mgr.list()).map(partition_installed);
    let report = match &index {
        Ok(index) => report::from_index(index),
        Err(e) => report::unavailable(e),
    };

    if json {
        return report::emit(&report, out, err);
    }

    match &index {
        Ok(index) => print_text(index, out, err),
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
        }
    }

    report::exit_code(&report)
}

/// The human-readable rendering. Reads the [`InstalledIndex`] rather than
/// the `Report` for one reason: the wire format deliberately omits `name`,
/// and this is the column that carries it.
fn print_text(index: &InstalledIndex, out: &mut dyn Write, err: &mut dyn Write) {
    print_unreadable_warnings(&index.unreadable, err);
    print_undetermined_warnings(&index.undetermined, err);
    for (id, entry) in &index.ours {
        let _ = writeln!(
            out,
            "{id}\t{}\t{}\tenabled={}",
            entry.spec.name,
            state_str(entry.state),
            entry.enabled
        );
    }
}
