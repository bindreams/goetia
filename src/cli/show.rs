//! `goetia daemon show [ID...]` / `goetia daemon show -f FILE`
//!
//! Read-only: never checks elevation. With `-f`, renders straight from a
//! manifest and touches no manager at all. Without it, reads the spec back
//! out of what is actually installed. Four guarantees, declared here as a
//! contract so a future change to either path is checked against them rather
//! than merely inherited by accident:
//!
//! 1. **When both paths can see the daemon**, `show <id>` and
//!    `show -f <file>` render the same resolved spec byte for byte, because
//!    both go through [`crate::diff::render_yaml`] and neither may grow a
//!    renderer of its own. Conditional: without `-f`, `show` reads
//!    [`ServiceManager::list`], which cannot describe a unit this privilege
//!    level could not read. Such a unit reaches `show` as
//!    [`crate::manager::Installed::Undetermined`], so `show <id>` exits `4`,
//!    while `show -f <file>` still renders it from the manifest.
//!    On a manifest with a `backend-specific:` block, the spec rendered is the
//!    merged, host-effective one (what would be installed *here*, not a
//!    transcription of the file); both paths merge for the same host's native
//!    backend, so they still agree.
//! 2. Neither path ever checks elevation.
//! 3. `show -f` touches no manager; `show` without `-f` touches no manifest.
//! 4. Output is YAML, one `# <id>` header per daemon, blank-line separated. It is for reading, so it
//!    is not byte-exact when a value holds a control character, U+2028, U+2029 or a bidirectional
//!    control: `dispatch`'s stream writes each as `\u{XXXX}`. The artifact file keeps the exact
//!    value.
//!
//! A daemon this privilege level *can* see, but whose blob will not decode,
//! is not "not installed": `show` returns `4` for it, per id and for the
//! no-ids form, the same code `list`/`status`/`diff` use. "Not installed"
//! stays `1`, a determinate answer, and outranks `4` when a single call names
//! both kinds of id (see `show_from_installed`).
//!
//! What `show` may never do is call an id absent on a listing that did not
//! establish absence. An id `list()` reported as undetermined is `4`, and so
//! is *any* id `show` cannot find while an aggregate entry — one with no
//! name — is present, since such an entry may stand for that very id.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{load_and_warn, partition_installed, print_undetermined_warnings, print_unusable_warnings};
use crate::error::{Result, escape};
use crate::manager::ServiceManager;
use crate::spec::DaemonSpec;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to show. With none, shows every matching daemon. Not
    /// combinable with `-f`: a manifest is one daemon.
    #[arg(conflicts_with = "file")]
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
) -> ExitCode {
    match &args.file {
        Some(file) => show_from_file(file, out, err),
        None => show_from_installed(&args.ids, get_manager, out, err),
    }
}

fn show_from_file(file: &Path, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let spec = match load_and_warn(file, err) {
        Ok(spec) => spec,
        Err(e) => {
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };
    print_specs(std::iter::once(&spec), out);
    ExitCode::Success
}

fn show_from_installed(
    ids: &[String],
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };
    let installed = match mgr.list() {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };

    let index = partition_installed(installed);
    print_unusable_warnings(&index.unusable, err);
    print_undetermined_warnings(&index.undetermined, err);

    let wanted: Vec<String> = if ids.is_empty() {
        index.ours.keys().cloned().collect()
    } else {
        ids.to_vec()
    };

    // Each id contributes at most one code; the greatest wins (see `dispatch`'s doc comment), so a
    // later unusable id cannot downgrade an already-seen absent one.
    let mut codes: Vec<ExitCode> = Vec::new();

    // With no ids, neither an unusable nor an undetermined entry enters
    // `wanted` (neither has a spec), so the loop below cannot flag it.
    // Without this, `show` would exit `0` on a complete-looking dump from a
    // host where it could not read half the daemons.
    if ids.is_empty() && (!index.unusable.is_empty() || !index.undetermined.is_empty()) {
        codes.push(ExitCode::Indeterminate);
    }

    let mut specs = Vec::new();
    for id in &wanted {
        // One-line messages quote the id and the reason, both from outside: written whole through
        // the strict writer.
        if let Some(entry) = index.ours.get(id) {
            specs.push(entry.spec.clone());
        } else if index.unusable.contains_key(id) {
            let _ = writeln!(err, "error: daemon `{}` is installed but unusable", escape(id));
            codes.push(ExitCode::Indeterminate);
        } else if let Some(entry) = index.undetermined_for(id) {
            // Either the listing could not classify this id, or an aggregate
            // entry may stand for it: absence is unestablished either way.
            let _ = writeln!(
                err,
                "error: daemon `{}`: installation state could not be determined: {}",
                escape(id),
                escape(&entry.reason)
            );
            codes.push(ExitCode::Indeterminate);
        } else {
            let _ = writeln!(err, "error: daemon `{}` is not installed", escape(id));
            codes.push(ExitCode::Error);
        }
    }
    print_specs(specs.iter(), out);
    codes.into_iter().max().unwrap_or(ExitCode::Success)
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
