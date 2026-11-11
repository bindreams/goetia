//! `goetia daemon install [-f FILE] [--force] [--start] [--enable] [--dry-run]`
//!
//! Values are escaped for reading (see [`super::show`], guarantee 4).

use std::fmt::Write as _;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::refusal::{Lines, Mood, NotRefused};
use super::support::{load_and_warn, print_error, print_line, require_elevation};
use super::wait::WaitArgs;
use crate::backend::Identity;
use crate::decide::Outcome;
use crate::error::Result;
use crate::manager::{Budget, ServiceManager, Step};
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
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    if args.dry_run {
        let _ = writeln!(out, "# {}", spec.id);
        return match preview_artifact(&spec, host) {
            Ok(artifact) => {
                let _ = write!(out, "{artifact}");
                ExitCode::Success
            }
            Err(e) => {
                print_error(err, None, "", &e);
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
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    // Each step (install, `--enable`, `--start`) contributes at most one code; see `ExitCode` for which wins.
    let mut codes: Vec<ExitCode> = Vec::new();
    // One sequence for every step, prepared before the install sends anything: a start that ran out
    // of threads or files after the install's bootout would strand the daemon, and the steps share
    // what the sequence holds (systemd's version gate). Without `--start` the budget is the
    // install's own: unbounded.
    let (steps, budget) = if args.start {
        (&[Step::Install, Step::Start][..], args.wait.budget())
    } else {
        (&[Step::Install][..], Budget::Unbounded)
    };
    let mut sequence = match mgr.prepare(steps, budget) {
        Ok(sequence) => sequence,
        Err(e) => {
            print_error(err, Some(spec.id.as_str()), "", &e);
            return ExitCode::of(&e);
        }
    };
    match sequence.install(&spec, args.force) {
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
            if args.enable
                && let Err(e) = sequence.enable(&spec.id)
            {
                print_error(err, Some(spec.id.as_str()), "enable: ", &e);
                codes.push(ExitCode::of(&e));
            }
            if args.start
                && let Err(e) = sequence.start(&spec.id, args.wait.budget())
            {
                print_error(err, Some(spec.id.as_str()), "start: ", &e);
                codes.push(ExitCode::of(&e));
            }
        }
        Err(e) => {
            print_error(err, Some(spec.id.as_str()), "", &e);
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
    let lines = Lines { id, mood: Mood::Done };
    let settled = match lines.refusal(outcome, out, err) {
        ControlFlow::Break(code) => return code,
        ControlFlow::Continue(settled) => settled,
    };
    match settled {
        NotRefused::Create => {
            let _ = writeln!(out, "{id}: created");
        }
        NotRefused::UpToDate => {
            let _ = writeln!(out, "{id}: up to date");
        }
        NotRefused::Update { spec_diff } => {
            let _ = writeln!(out, "{id}: updated");
            let _ = write!(out, "{spec_diff}");
        }
        NotRefused::Stale { from_version } => {
            print_line(out, None, Some(id.as_str()), |sink| {
                write!(sink, "regenerated (was built by goetia {from_version})")
            });
        }
    }
    ExitCode::Success
}

// --dry-run preview ===================================================================================================
//
// --dry-run never touches a manager or writes anything. It still reads the manifest and `.env` (resolving the spec),
// and it canonicalizes the process's current directory when `-f` is relative. A drive-relative path (`C:bin`) in the
// manifest is resolved against the process's per-drive current directory, which `std::path::absolute` reads. The
// preview step itself, on Windows, asks the shim provider for the host command (`host`) of a `type: simple` daemon,
// and reads no account or file. A real install resolves `spec.user` to a platform account name (see
// `backend::Identity`) with an effectful lookup (a Windows SID needs `LookupAccountSid`; a launchd numeric UID needs
// one too); a preview does not, so this is a literal, lookup-free rendering that may differ from a real install's
// artifact in some cases.

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

/// The generated artifact for `spec`, or the generator's refusal of it: scm refuses `restart: always` and
/// an executable path its registration writer rewrites; systemd refuses the values `refuse_unrepresentable` lists.
///
/// Writes nothing; on Windows a `type: simple` daemon's preview asks `host` for the host command.
/// `host` is called only by that preview, whose artifact names it.
fn preview_artifact(spec: &DaemonSpec, host: &dyn Fn() -> Result<HostCommand>) -> Result<String> {
    let identity = preview_identity(&spec.user);

    #[cfg(target_os = "linux")]
    {
        let _ = host;
        crate::backend::systemd::generate::unit(spec, &identity)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = host;
        Ok(crate::backend::launchd::generate::plist(spec, &identity))
    }
    #[cfg(windows)]
    {
        // The SCM generator's warning (a too-large `restart-delay`) fires at
        // resolve time, from `BackendKind::Scm.warn`, before this runs.
        // Asked only for a `type: simple` daemon: a managed one runs no host, and a provider that
        // cannot name one must not fail its preview. The real host, so the preview refuses a path
        // the install would.
        let command = match spec.kind {
            crate::spec::Kind::Simple => Some(host()?),
            crate::spec::Kind::Managed => None,
        };
        let registration = crate::backend::scm::generate::install_registration(spec, &identity, command.as_ref())?;
        Ok(registration.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = host;
        Ok("# no dry-run preview generator for this platform\n".to_string())
    }
}
