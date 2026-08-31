//! `goetia daemon install [ID...] [-f FILE] [--force] [--start] [--enable] [--dry-run]`

use std::io::Write;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::support::{load_and_warn, require_elevation, select_by_ids};
use crate::backend::Identity;
use crate::decide::Outcome;
use crate::error::Result;
use crate::manager::ServiceManager;
use crate::spec::{AccountId, DaemonSpec, Id, User};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to install. With none, installs every daemon in the file.
    pub ids: Vec<String>,
    /// Path to goetia.yaml, or a directory containing it.
    #[arg(short = 'f', long = "file", default_value = ".")]
    pub file: PathBuf,
    /// Overwrite an artifact hand-edited outside Goetia.
    #[arg(long)]
    pub force: bool,
    /// Also start each daemon after installing it.
    #[arg(long)]
    pub start: bool,
    /// Also enable each daemon at boot after installing it.
    #[arg(long)]
    pub enable: bool,
    /// Print the artifact text install would write for this host, then
    /// exit. Needs no elevation and touches no manager — see the crate-level
    /// design notes on why.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
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

    if args.dry_run {
        for (i, spec) in selected.iter().enumerate() {
            if i > 0 {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "# {}", spec.id);
            let _ = write!(out, "{}", preview_artifact(spec));
        }
        return 0;
    }

    if let Err(code) = require_elevation("daemon install", is_elevated, err) {
        return code;
    }
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    let mut worst = 0;
    for spec in &selected {
        match mgr.install(spec, args.force) {
            Ok(outcome) => {
                let code = report_outcome(&spec.id, &outcome, out);
                worst = worst.max(code);
                // Nothing was (re)written on a refusal or conflict: neither
                // flag below has anything to act on.
                if code != 0 {
                    continue;
                }
                if args.enable {
                    if let Err(e) = mgr.enable(&spec.id) {
                        let _ = writeln!(err, "error: {}: enable: {e}", spec.id);
                        worst = worst.max(1);
                    }
                }
                if args.start {
                    if let Err(e) = mgr.start(&spec.id) {
                        let _ = writeln!(err, "error: {}: start: {e}", spec.id);
                        worst = worst.max(1);
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(err, "error: {}: {e}", spec.id);
                worst = worst.max(1);
            }
        }
    }
    worst
}

/// Print one outcome and return its exit-code contribution: `0` for
/// anything that succeeded or was a no-op, `1` for a refusal, `2` for a
/// conflict.
fn report_outcome(id: &Id, outcome: &Outcome, out: &mut dyn Write) -> i32 {
    match outcome {
        Outcome::Create => {
            let _ = writeln!(out, "{id}: created");
            0
        }
        Outcome::UpToDate => {
            let _ = writeln!(out, "{id}: up to date");
            0
        }
        Outcome::Update { spec_diff } => {
            let _ = writeln!(out, "{id}: updated");
            let _ = write!(out, "{spec_diff}");
            0
        }
        Outcome::Stale { from_version } => {
            let _ = writeln!(out, "{id}: regenerated (was built by goetia {from_version})");
            0
        }
        Outcome::Conflict { artifact_diff } => {
            let _ = writeln!(out, "{id}: conflict (re-run with --force to overwrite)");
            let _ = write!(out, "{artifact_diff}");
            2
        }
        Outcome::RefuseForeign { recovery } => {
            let _ = writeln!(out, "{id}: refused: not a goetia-managed service. {recovery}");
            1
        }
        Outcome::RefuseUnreadable { reason, recovery } => {
            let _ = writeln!(out, "{id}: refused: {reason}. {recovery}");
            1
        }
    }
}

// --dry-run preview ====================================================================================================
//
// Generation is pure and "nearly free" by design (see the CLI spec's §4), so
// --dry-run deliberately never touches a manager or does any I/O of its
// own. The one piece real installation needs that generation itself cannot
// provide — resolving `spec.user` to a platform account name (see
// `backend::Identity`) — is genuinely effectful for a real install (a
// Windows SID needs `LookupAccountSid`; a launchd numeric UID needs a
// lookup too), which a *preview* has no business doing. What follows is
// therefore a literal, lookup-free rendering: correct for the common
// `root`/`name`/numeric-`uid` cases, and clearly a preview — never what a
// real install's own effectful identity resolution (Tasks 11-13) would
// write byte-for-byte in every case.

fn preview_identity(user: &User) -> Identity {
    Identity {
        user: match user {
            User::Root => preview_root_account(),
            User::Name(name) => name.clone(),
            User::Id(AccountId::Uid(uid)) => uid.to_string(),
            User::Id(AccountId::Sid(sid)) => sid.clone(),
        },
    }
}

#[cfg(target_os = "linux")]
fn preview_root_account() -> String {
    "0".to_string()
}

#[cfg(not(target_os = "linux"))]
fn preview_root_account() -> String {
    "root".to_string()
}

fn preview_artifact(spec: &DaemonSpec) -> String {
    let identity = preview_identity(&spec.user);

    #[cfg(target_os = "linux")]
    {
        crate::backend::systemd::generate::unit(spec, &identity)
    }
    #[cfg(target_os = "macos")]
    {
        crate::backend::launchd::generate::plist(spec, &identity)
    }
    #[cfg(windows)]
    {
        // The real shim path is an open packaging question (see the plan's
        // "Windows shim path" item) — this placeholder is only ever shown
        // in a preview, never installed.
        let shim_path = PathBuf::from("goetia-shim.exe");
        let (registration, _warnings) = crate::backend::scm::generate::registration(spec, &identity, &shim_path);
        crate::backend::scm::generate::render(&registration)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        "# no dry-run preview generator for this platform\n".to_string()
    }
}
