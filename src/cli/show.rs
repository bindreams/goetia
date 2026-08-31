//! `goetia daemon show [ID...] [-f FILE]`
//!
//! Read-only: never checks elevation. With `-f`, renders straight from a
//! manifest and touches no manager at all. Without it, reads the spec back
//! out of what is actually installed — both paths render through the same
//! [`crate::diff::render_yaml`], so they agree by construction.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

use super::support::{load_and_warn, partition_installed, print_unreadable_warnings, select_by_ids};
use crate::error::Result;
use crate::manager::ServiceManager;
use crate::spec::DaemonSpec;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to show. With none, shows every matching daemon.
    pub ids: Vec<String>,
    /// Render from this manifest (a file, or a directory containing
    /// goetia.yaml) instead of from what is actually installed.
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match &args.file {
        Some(file) => show_from_file(file, &args.ids, out, err),
        None => show_from_installed(&args.ids, get_manager, out, err),
    }
}

fn show_from_file(file: &Path, ids: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let specs = match load_and_warn(file, err) {
        Ok(specs) => specs,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };
    let selected = match select_by_ids(&specs, ids) {
        Ok(s) => s,
        Err(msg) => {
            let _ = writeln!(err, "error: {msg}");
            return 1;
        }
    };
    print_specs(selected.into_iter(), out);
    0
}

fn show_from_installed(
    ids: &[String],
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
    let installed = match mgr.list() {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    let index = partition_installed(installed);
    print_unreadable_warnings(&index.unreadable, err);

    let wanted: Vec<String> = if ids.is_empty() {
        index.ours.keys().cloned().collect()
    } else {
        ids.to_vec()
    };

    // With no ids given, an unreadable entry never enters `wanted` at all
    // (it has no spec to show), so the loop below can't be what flags it —
    // unlike `list`/`status`, which escalate the same way.
    let mut exit = if ids.is_empty() && !index.unreadable.is_empty() {
        1
    } else {
        0
    };
    let mut specs = Vec::new();
    for id in &wanted {
        if let Some((spec, _, _)) = index.ours.get(id) {
            specs.push(spec.clone());
        } else if index.unreadable.contains_key(id) {
            let _ = writeln!(err, "error: daemon `{id}` is installed but unreadable");
            exit = 1;
        } else {
            let _ = writeln!(err, "error: daemon `{id}` is not installed");
            exit = 1;
        }
    }
    print_specs(specs.iter(), out);
    exit
}

fn print_specs<'a>(specs: impl Iterator<Item = &'a DaemonSpec>, out: &mut dyn Write) {
    for (i, spec) in specs.enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "# {}", spec.id);
        let _ = write!(out, "{}", crate::diff::render_yaml(spec));
    }
}
