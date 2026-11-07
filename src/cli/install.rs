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
use crate::manager::{ServiceManager, Step};
use crate::shim::HostCommand;
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
    host: &dyn Fn() -> Result<HostCommand>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let spec = match load_and_warn(&args.file, err) {
        Ok(spec) => spec,
        Err(e) => {
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };

    if args.dry_run {
        let _ = writeln!(out, "# {}", spec.id);
        return match preview_artifact(&spec, host, err) {
            Ok(artifact) => {
                let _ = write!(out, "{artifact}");
                ExitCode::Success
            }
            Err(e) => {
                let _ = writeln!(err, "error: {}", e.report());
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
            let _ = writeln!(err, "error: {}", e.report());
            return ExitCode::Error;
        }
    };

    // Each step (install, `--enable`, `--start`) contributes at most one code; see `ExitCode` for which wins.
    let mut codes: Vec<ExitCode> = Vec::new();
    // Prepared before the install sends anything: a start that ran out of threads or files after
    // the install's bootout would strand the daemon. The install reserves its own.
    let mut sequence = match args
        .start
        .then(|| mgr.prepare(&[Step::Start], args.wait.budget()))
        .transpose()
    {
        Ok(sequence) => sequence,
        Err(e) => {
            let _ = writeln!(err, "error: {}: {}", spec.id, e.report());
            return ExitCode::of(&e);
        }
    };
    match mgr.install(&spec, args.force) {
        Ok(outcome) => {
            match report_outcome(&spec.id, &outcome, out, err) {
                ExitCode::Success => codes.push(ExitCode::Success),
                // Nothing was (re)written: neither flag below has anything to
                // act on.
                other => {
                    debug_assert!(matches!(other, ExitCode::Conflict | ExitCode::Error), "{other:?}");
                    return other;
                }
            }
            if args.enable {
                if let Err(e) = mgr.enable(&spec.id) {
                    let _ = writeln!(err, "error: {}: enable: {}", spec.id, e.report());
                    codes.push(ExitCode::of(&e));
                }
            }
            if let Some(sequence) = sequence.as_mut() {
                if let Err(e) = sequence.start(&spec.id, args.wait.budget()) {
                    let _ = writeln!(err, "error: {}: start: {}", spec.id, e.report());
                    codes.push(ExitCode::of(&e));
                }
            }
        }
        Err(e) => {
            let _ = writeln!(err, "error: {}: {}", spec.id, e.report());
            codes.push(ExitCode::of(&e));
        }
    }
    codes.into_iter().max().unwrap_or(ExitCode::Success)
}

/// Print one outcome and classify it for the exit code. The full text
/// (including any diff) always goes to `out`; a refusal or conflict
/// additionally gets a one-line diagnostic on `err`, so a script that only
/// captures stderr for errors still learns that an exit-1/5 run had one.
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
            // `--force` is offered only where it resolves the conflict. When part
            // of the cause is outside the directory this backend writes, forcing
            // leaves that part in place and the next run reports the same
            // conflict; `decide` says which case this is (see `Outcome::Conflict`).
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
// --dry-run never touches a manager or does I/O. A real install resolves
// `spec.user` to a platform account name (see `backend::Identity`) with an
// effectful lookup (a Windows SID needs `LookupAccountSid`; a launchd numeric
// UID needs one too); a preview does not, so this is a literal, lookup-free
// rendering that may differ from a real install's artifact in some cases.

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
/// `err` is threaded through on every platform so a generator warning cannot
/// be dropped; no generator produces a [`crate::spec::Warning`] here today. `host` is
/// called only by the Windows preview of a `type: simple` daemon, whose artifact names it.
fn preview_artifact(spec: &DaemonSpec, host: &dyn Fn() -> Result<HostCommand>, err: &mut dyn Write) -> Result<String> {
    let identity = preview_identity(&spec.user);

    #[cfg(target_os = "linux")]
    {
        let _ = (err, host);
        Ok(crate::backend::systemd::generate::unit(spec, &identity))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (err, host);
        Ok(crate::backend::launchd::generate::plist(spec, &identity))
    }
    #[cfg(windows)]
    {
        // The SCM generator's warning (a too-large `restart-delay`) fires at
        // resolve time, from `BackendKind::Scm.warn`, before this runs.
        let _ = err;
        // Asked only for a `type: simple` daemon: a managed one runs no host, and a provider that
        // cannot name one must not fail its preview.
        let command = match spec.kind {
            crate::spec::Kind::Simple => Some(host()?),
            crate::spec::Kind::Managed => None,
        };
        let registration = crate::backend::scm::generate::registration(spec, &identity, command.as_ref())?;
        Ok(registration.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (err, host);
        Ok("# no dry-run preview generator for this platform\n".to_string())
    }
}
