//! `goetia daemon show [ID...] [-f FILE]`
//!
//! Read-only: never checks elevation. With `-f`, renders straight from a
//! manifest and touches no manager at all. Without it, reads the spec back
//! out of what is actually installed. Four guarantees, declared here as a
//! contract so a future change to either path is checked against them
//! rather than merely inherited by accident:
//!
//! 1. **When both paths can see the daemon**, `show <id>` and
//!    `show -f <file> <id>` render the same resolved spec identically,
//!    byte for byte, because both go through [`crate::diff::render_yaml`]
//!    and neither may grow a renderer of its own. Conditional, not
//!    unconditional agreement: without `-f`, `show` reads
//!    [`ServiceManager::list`], which silently skips a unit this privilege
//!    level cannot enumerate (see
//!    [`crate::manager::Installed::OursUnreadable`]'s doc comment). For a
//!    daemon installed but unreadable unelevated, `show <id>` therefore
//!    reports "not installed" and exits `1`, while `show -f <file> <id>`
//!    still renders it straight from the manifest.
//! 2. Neither path ever checks elevation.
//! 3. `show -f` touches no manager; `show` without `-f` touches no
//!    manifest.
//! 4. Output is YAML, one `# <id>` header per daemon, blank-line
//!    separated.
//!
//! A daemon this privilege level *can* see, but whose blob will not decode,
//! is a different case from "not installed": `show` returns `4` for it —
//! both per id, and, via `index.unreadable`'s escalation, for the no-ids
//! form — the same "goetia owns this id and could not determine its
//! state" code `list`/`status`/`diff` use. "Not installed" stays `1`, a
//! determinate answer, and outranks `4` when a single call names both
//! kinds of id (see `show_from_installed`).

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

use super::report;
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

    // Each id contributes at most one exit-code class, combined by
    // `report::precedence` — the same rule and function `list`/`status`
    // and `diff` use, reused rather than re-derived here (see `dispatch`'s
    // doc comment for the vocabulary and the precedence order itself).
    // Never `max()` on the codes themselves: `1` must outrank `4` even
    // though it is the smaller number, so a later unreadable id cannot
    // downgrade an already-seen absent one.
    let mut codes: Vec<i32> = Vec::new();

    // With no ids given, an unreadable entry never enters `wanted` at all
    // (it has no spec to show), so the loop below can't be what flags it —
    // unlike `list`/`status`, which escalate the same way.
    if ids.is_empty() && !index.unreadable.is_empty() {
        codes.push(4);
    }

    let mut specs = Vec::new();
    for id in &wanted {
        if let Some(entry) = index.ours.get(id) {
            specs.push(entry.spec.clone());
        } else if index.unreadable.contains_key(id) {
            let _ = writeln!(err, "error: daemon `{id}` is installed but unreadable");
            codes.push(4);
        } else {
            let _ = writeln!(err, "error: daemon `{id}` is not installed");
            codes.push(1);
        }
    }
    print_specs(specs.iter(), out);
    codes
        .into_iter()
        .max_by_key(|code| report::precedence(*code))
        .unwrap_or(0)
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
