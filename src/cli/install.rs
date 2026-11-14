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
use super::wait::{NOT_FOR_A_TASK, WaitArgs};
use crate::backend::Identity;
use crate::decide::Outcome;
use crate::error::Result;
use crate::manager::flow::NOTHING_TO_ARM;
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
        let _ = writeln!(out, "# {}", spec.common().id);
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
    // Refused before anything is sent: what a task's `start` and `enable` cannot do is known from the
    // spec alone.
    if let Err(e) = refuse_for_a_task(&spec, args) {
        print_error(err, Some(spec.common().id.as_str()), "", &e);
        return ExitCode::Error;
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
    // Prepared before the install sends anything: a start that ran out of threads would strand the daemon.
    let (steps, budget) = if args.start {
        (&[Step::Install, Step::Start][..], args.wait.budget())
    } else {
        (&[Step::Install][..], Budget::Unbounded)
    };
    let mut sequence = match mgr.prepare(steps, budget) {
        Ok(sequence) => sequence,
        Err(e) => {
            print_error(err, Some(spec.common().id.as_str()), "", &e);
            return ExitCode::of(&e);
        }
    };
    match sequence.install(&spec, args.force) {
        Ok(outcome) => {
            match report_outcome(&spec.common().id, &outcome, out, err) {
                ExitCode::Success => codes.push(ExitCode::Success),
                // Nothing was (re)written: neither flag below has anything to
                // act on.
                other => {
                    debug_assert!(matches!(other, ExitCode::Conflict | ExitCode::Error), "{other:?}");
                    return other;
                }
            }
            if args.enable
                && let Err(e) = sequence.enable(&spec.common().id)
            {
                print_error(err, Some(spec.common().id.as_str()), "enable: ", &e);
                codes.push(ExitCode::of(&e));
            }
            if args.start
                && let Err(e) = sequence.start(&spec.common().id, args.wait.budget())
            {
                print_error(err, Some(spec.common().id.as_str()), "start: ", &e);
                codes.push(ExitCode::of(&e));
            }
        }
        Err(e) => {
            print_error(err, Some(spec.common().id.as_str()), "", &e);
            codes.push(ExitCode::of(&e));
        }
    }
    codes.into_iter().max().unwrap_or(ExitCode::Success)
}

/// What `--start` and `--enable` cannot do for the task `spec` is, so the install is not begun: its
/// start takes no wait flag, and with no schedule it has nothing to arm. A service is never refused.
fn refuse_for_a_task(spec: &DaemonSpec, args: &Args) -> Result<()> {
    let DaemonSpec::Task(task) = spec else {
        return Ok(());
    };
    let id = &spec.common().id;
    if args.start && args.wait.was_given() {
        return Err(crate::spec::invalid(id, NOT_FOR_A_TASK));
    }
    if args.enable && task.schedule.is_none() {
        return Err(crate::spec::invalid(id, NOTHING_TO_ARM));
    }
    Ok(())
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
// preview step itself, on Windows, asks the shim provider for the host command (`host`) of a `protocol: plain` daemon,
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
/// Writes nothing; on Windows a `protocol: plain` daemon's preview asks `host` for the host command.
/// `host` is called only by that preview, whose artifact names it.
fn preview_artifact(spec: &DaemonSpec, host: &dyn Fn() -> Result<HostCommand>) -> Result<String> {
    let identity = preview_identity(&spec.common().user);

    #[cfg(target_os = "linux")]
    {
        let _ = host;
        crate::backend::systemd::generate::units(spec, &identity).map(|units| units.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = host;
        crate::backend::launchd::generate::plist(spec, &identity)
    }
    #[cfg(windows)]
    {
        // The SCM generator's warning (a too-large `restart-delay`) fires at
        // resolve time, from `BackendKind::Scm.warn`, before this runs.
        // `host` is the real one, so the preview refuses a path the install would; the generator asks
        // it only for hosted services.
        let registration = crate::backend::scm::generate::install_registration(spec, &identity, host)?;
        Ok(registration.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = host;
        Ok("# no dry-run preview generator for this platform\n".to_string())
    }
}
