//! `goetia daemon install [ID...] [-f FILE] [--force] [--start] [--enable] [--dry-run]`

use std::io::Write;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::support::{load_and_warn, print_warnings, require_elevation, select_by_ids};
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
            let _ = write!(out, "{}", preview_artifact(spec, err));
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

    // Errors/refusals (exit 1) and conflicts (exit 2) are disjoint classes,
    // not a severity ladder: 2 specifically promises "every failure here is
    // force-resolvable", which stops being true the moment even one daemon
    // in the same run hard-failed. So a plain `max()` across per-daemon
    // codes would let a real error hide behind a conflict; track the two
    // classes separately instead and let an error always win.
    let mut any_error = false;
    let mut any_conflict = false;
    for spec in &selected {
        match mgr.install(spec, args.force) {
            Ok(outcome) => {
                let class = report_outcome(&spec.id, &outcome, out, err);
                match class {
                    OutcomeClass::Ok => {}
                    OutcomeClass::Conflict => {
                        any_conflict = true;
                        // Nothing was (re)written: neither flag below has
                        // anything to act on.
                        continue;
                    }
                    OutcomeClass::Refused => {
                        any_error = true;
                        continue;
                    }
                }
                if args.enable {
                    if let Err(e) = mgr.enable(&spec.id) {
                        let _ = writeln!(err, "error: {}: enable: {e}", spec.id);
                        any_error = true;
                    }
                }
                if args.start {
                    if let Err(e) = mgr.start(&spec.id) {
                        let _ = writeln!(err, "error: {}: start: {e}", spec.id);
                        any_error = true;
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(err, "error: {}: {e}", spec.id);
                any_error = true;
            }
        }
    }

    if any_error {
        1
    } else if any_conflict {
        2
    } else {
        0
    }
}

/// Which of the three exit-code buckets an [`Outcome`] belongs to.
enum OutcomeClass {
    Ok,
    Refused,
    Conflict,
}

/// Print one outcome and classify it for the exit code. The full text
/// (including any diff) always goes to `out`; a refusal or conflict
/// additionally gets a concise one-line diagnostic on `err`, consistent
/// with every other failure path in the CLI (`run_id_verb`, `list`,
/// `status`, `diff` all put failures on stderr) — a script that only
/// captures stderr for errors must still learn that this exit-1/2 run had
/// one.
fn report_outcome(id: &Id, outcome: &Outcome, out: &mut dyn Write, err: &mut dyn Write) -> OutcomeClass {
    match outcome {
        Outcome::Create => {
            let _ = writeln!(out, "{id}: created");
            OutcomeClass::Ok
        }
        Outcome::UpToDate => {
            let _ = writeln!(out, "{id}: up to date");
            OutcomeClass::Ok
        }
        Outcome::Update { spec_diff } => {
            let _ = writeln!(out, "{id}: updated");
            let _ = write!(out, "{spec_diff}");
            OutcomeClass::Ok
        }
        Outcome::Stale { from_version } => {
            let _ = writeln!(out, "{id}: regenerated (was built by goetia {from_version})");
            OutcomeClass::Ok
        }
        Outcome::Conflict { artifact_diff } => {
            let _ = writeln!(out, "{id}: conflict (re-run with --force to overwrite)");
            let _ = write!(out, "{artifact_diff}");
            let _ = writeln!(err, "error: {id}: conflict (re-run with --force to overwrite)");
            OutcomeClass::Conflict
        }
        Outcome::RefuseForeign { recovery } => {
            let _ = writeln!(out, "{id}: refused: not a goetia-managed service. {recovery}");
            let _ = writeln!(err, "error: {id}: refused: not a goetia-managed service. {recovery}");
            OutcomeClass::Refused
        }
        Outcome::RefuseUnreadable { reason, recovery } => {
            let _ = writeln!(out, "{id}: refused: {reason}. {recovery}");
            let _ = writeln!(err, "error: {id}: refused: {reason}. {recovery}");
            OutcomeClass::Refused
        }
    }
}

// --dry-run preview ===================================================================================================
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

/// `err` is unused on every platform but Windows today — the systemd and
/// launchd generators never produce a [`crate::spec::Warning`] — but it is
/// threaded through unconditionally so a future one cannot be dropped.
fn preview_artifact(spec: &DaemonSpec, err: &mut dyn Write) -> String {
    let identity = preview_identity(&spec.user);

    #[cfg(target_os = "linux")]
    {
        let _ = err;
        crate::backend::systemd::generate::unit(spec, &identity)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = err;
        crate::backend::launchd::generate::plist(spec, &identity)
    }
    #[cfg(windows)]
    {
        // The real shim path is an open packaging question (see the plan's
        // "Windows shim path" item) — this placeholder is only ever shown
        // in a preview, never installed.
        let shim_path = PathBuf::from("goetia-shim.exe");
        let (registration, warnings) = crate::backend::scm::generate::registration(spec, &identity, &shim_path);
        print_warnings(&warnings, err);
        crate::backend::scm::generate::render(&registration)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = err;
        "# no dry-run preview generator for this platform\n".to_string()
    }
}
