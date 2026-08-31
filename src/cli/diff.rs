//! `goetia daemon diff [ID...] [-f FILE]`
//!
//! Read-only: never checks elevation. Compares each selected daemon's
//! currently-installed spec against what `-f` (default `.`) would apply —
//! the update-level diff [`crate::decide::Outcome::Update`] would show, not
//! the artifact-level one only `install`'s `Conflict` can produce (that one
//! needs the raw on-disk text, which `ServiceManager` does not expose).
//! Always needs a manager (comparing against installed state is the whole
//! point), `-f` or not.

use std::io::Write;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::support::{load_and_warn, partition_installed, print_unreadable_warnings, select_by_ids};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to diff. With none, diffs every daemon in the file.
    pub ids: Vec<String>,
    /// Path to goetia.yaml, or a directory containing it.
    #[arg(short = 'f', long = "file", default_value = ".")]
    pub file: PathBuf,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let specs = match load_and_warn(&args.file, err) {
        Ok(specs) => specs,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let selected = match select_by_ids(&specs, &args.ids) {
        Ok(s) => s,
        Err(msg) => {
            let _ = writeln!(err, "error: {msg}");
            return 1;
        }
    };

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

    let mut exit = 0;
    for new_spec in selected {
        let id_str = new_spec.id.as_str();
        if let Some((old_spec, _, _)) = index.ours.get(id_str) {
            let spec_diff = crate::diff::spec_diff(old_spec, new_spec);
            if spec_diff.is_empty() {
                let _ = writeln!(out, "{}: up to date", new_spec.id);
            } else {
                let _ = writeln!(out, "{}:", new_spec.id);
                let _ = write!(out, "{spec_diff}");
            }
        } else if index.unreadable.contains_key(id_str) {
            let _ = writeln!(out, "{}: installed but unreadable (see warning above)", new_spec.id);
            exit = 1;
        } else {
            let _ = writeln!(out, "{}: not installed (would be created)", new_spec.id);
        }
    }
    exit
}
