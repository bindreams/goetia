//! `goetia daemon install [-f FILE] [--force] [--start] [--enable] [--dry-run]`

use std::io::Write;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::support::{load_and_warn, require_elevation};
use super::wait::WaitArgs;
use crate::backend::Identity;
use crate::decide::{self, Outcome};
use crate::error::Result;
use crate::manager::{Prepared, ServiceManager, Step};
use crate::spec::{AccountId, DaemonSpec, Id, User};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to goetia.yaml, or a directory containing it.
    #[arg(short = 'f', long = "file", default_value = ".")]
    pub file: PathBuf,
    /// Overwrite an artifact hand-edited outside Goetia.
    #[arg(long)]
    pub force: bool,
    /// Also start the daemon after installing it.
    #[arg(long)]
    pub start: bool,
    /// Also enable the daemon at boot after installing it.
    #[arg(long)]
    pub enable: bool,
    /// Print the artifact text install would write for this host, then
    /// exit. Needs no elevation and touches no manager — see the crate-level
    /// design notes on why.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Bounds the `--start` above and nothing else. Without `--start` there
    /// is nothing to wait for, which `dispatch` refuses at exit `2`; under
    /// `--dry-run` these are inert exactly as `--start` itself is.
    #[command(flatten)]
    pub wait: WaitArgs,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let spec = match load_and_warn(&args.file, err) {
        Ok(spec) => spec,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return ExitCode::Error;
        }
    };

    if args.dry_run {
        let _ = writeln!(out, "# {}", spec.id);
        return match preview_artifact(&spec, err) {
            Ok(artifact) => {
                let _ = write!(out, "{artifact}");
                ExitCode::Success
            }
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                ExitCode::Error
            }
        };
    }

    if let Err(code) = require_elevation("daemon install", is_elevated, err) {
        return code;
    }
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return ExitCode::Error;
        }
    };

    // Each step (install, `--enable`, `--start`) contributes at most one code; the greatest wins.
    let mut codes: Vec<ExitCode> = Vec::new();
    // Before the install sends anything, so that nothing the start needs
    // can run out after the install booted a loaded job out.
    let _prepared = if args.start {
        match mgr.prepare(&[Step::Install, Step::Start], args.wait.budget()) {
            Ok(prepared) => prepared,
            Err(e) => {
                let _ = writeln!(err, "error: {}: {e}", spec.id);
                return ExitCode::of(&e);
            }
        }
    } else {
        Prepared::nothing()
    };
    match mgr.install(&spec, args.force) {
        Ok(outcome) => {
            match report_outcome(&spec.id, &outcome, out, err) {
                ExitCode::Success => codes.push(ExitCode::Success),
                // Nothing was (re)written: neither flag below has anything to
                // act on.
                refused => return refused,
            }
            if args.enable {
                if let Err(e) = mgr.enable(&spec.id) {
                    let _ = writeln!(err, "error: {}: enable: {e}", spec.id);
                    codes.push(ExitCode::of(&e));
                }
            }
            if args.start {
                if let Err(e) = mgr.start(&spec.id, args.wait.budget()) {
                    let _ = writeln!(err, "error: {}: start: {e}", spec.id);
                    codes.push(ExitCode::of(&e));
                }
            }
        }
        Err(e) => {
            let _ = writeln!(err, "error: {}: {e}", spec.id);
            codes.push(ExitCode::of(&e));
        }
    }
    codes.into_iter().max().unwrap_or(ExitCode::Success)
}

/// Print one outcome and classify it for the exit code. The full text
/// (including any diff) always goes to `out`; a refusal or conflict
/// additionally gets a concise one-line diagnostic on `err`, consistent
/// with every other failure path in the CLI (`run_id_verb`, `list`,
/// `status`, `diff` all put failures on stderr) — a script that only
/// captures stderr for errors must still learn that this exit-1/5 run had
/// one.
fn report_outcome(id: &Id, outcome: &Outcome, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match outcome {
        Outcome::Create => {
            let _ = writeln!(out, "{id}: created");
            ExitCode::Success
        }
        Outcome::UpToDate => {
            let _ = writeln!(out, "{id}: up to date");
            ExitCode::Success
        }
        Outcome::Update { spec_diff } => {
            let _ = writeln!(out, "{id}: updated");
            let _ = write!(out, "{spec_diff}");
            ExitCode::Success
        }
        Outcome::Stale { from_version } => {
            let _ = writeln!(out, "{id}: regenerated (was built by goetia {from_version})");
            ExitCode::Success
        }
        Outcome::Conflict {
            artifact_diff,
            unclearable_recovery,
        } => {
            // `--force` is published as the way out of a conflict, so it is
            // offered only where it is one. When part of the cause is
            // outside the directory this backend writes, forcing rewrites
            // the artifact, leaves that part in place, and the next run
            // reports the identical conflict — `decide` says which case this
            // is (see `Outcome::Conflict`), and nothing here re-derives it.
            let line = match unclearable_recovery {
                None => format!("{id}: conflict (re-run with --force to overwrite)"),
                Some(recovery) => format!("{id}: conflict. {recovery}"),
            };
            let _ = writeln!(out, "{line}");
            let _ = write!(out, "{artifact_diff}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Conflict
        }
        Outcome::RefuseForeign { recovery } => {
            let _ = writeln!(out, "{id}: refused: not a goetia-managed service. {recovery}");
            let _ = writeln!(err, "error: {id}: refused: not a goetia-managed service. {recovery}");
            ExitCode::Error
        }
        Outcome::RefuseOtherDaemon { installed } => {
            let line = format!("{id}: refused: {}", decide::other_daemon_refusal(installed.as_str()));
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Error
        }
        Outcome::RefuseUnusable { reason, recovery } => {
            let line = match recovery {
                Some(recovery) => format!("{id}: refused: {reason}. {recovery}"),
                None => format!("{id}: refused: {reason}"),
            };
            let _ = writeln!(out, "{line}");
            let _ = writeln!(err, "error: {line}");
            ExitCode::Error
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
// real install's own effectful identity resolution would write
// byte-for-byte in every case.

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

/// The generated artifact for `spec`, or the generator's refusal of it (scm refuses `restart: always`).
///
/// `err` is unused on every platform but Windows today — the systemd and
/// launchd generators never produce a [`crate::spec::Warning`] — but it is
/// threaded through unconditionally so a future one cannot be dropped.
fn preview_artifact(spec: &DaemonSpec, err: &mut dyn Write) -> Result<String> {
    let identity = preview_identity(&spec.user);

    #[cfg(target_os = "linux")]
    {
        let _ = err;
        Ok(crate::backend::systemd::generate::unit(spec, &identity))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = err;
        Ok(crate::backend::launchd::generate::plist(spec, &identity))
    }
    #[cfg(windows)]
    {
        // The SCM generator's own warning (a too-large `restart-delay`)
        // fires at resolve time, from `BackendKind::Scm.warn`, before this ever
        // runs — see `spec/backend.rs`. `err` stays threaded through and
        // unused so a future generator warning cannot be dropped silently.
        let _ = err;
        // The real shim path is an open packaging question — this placeholder
        // is only ever shown in a preview, never installed.
        let shim_path = PathBuf::from("goetia-shim.exe");
        let registration = crate::backend::scm::generate::registration(spec, &identity, &shim_path)?;
        Ok(registration.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = err;
        Ok("# no dry-run preview generator for this platform\n".to_string())
    }
}
