//! The `goetia` command-line interface: argument parsing plus a testable
//! dispatcher.
//!
//! [`dispatch`] is wired to `goetia::manager::native()` by [`crate`'s bin
//! target](../../src/main.rs) — never to [`crate::manager::fake::Fake`]. A
//! CLI that secretly talked to the fake would pass every test here and do
//! nothing on a real machine. On a platform without a backend yet (Linux
//! has one), every mutating and every list/status subcommand therefore
//! fails on a real host with `no backend
//! for <platform> yet` — the message [`crate::manager::native`] returns.
//! `install --dry-run` and `show -f <file>` are the exceptions: they need
//! no backend at all (pure generation, or a spec read straight from a
//! file), so they keep working regardless. `diff` always needs a manager,
//! `-f` or not — comparing against installed state is its entire job.
//!
//! `dispatch` takes `get_manager` and `is_elevated` as lazily-invoked
//! closures for exactly this reason: a subcommand that does not need a
//! manager, or does not need elevation, must never call either — tests
//! prove this by handing in a closure that panics if called.

pub mod diff;
pub mod disable;
mod elevation;
pub mod enable;
pub mod install;
pub mod list;
mod report;
pub mod restart;
pub mod show;
pub mod start;
pub mod status;
pub mod stop;
mod support;
pub mod uninstall;

use std::io::Write;

use clap::{Parser, Subcommand};

pub use elevation::is_elevated;

use crate::error::Result;
use crate::manager::ServiceManager;

// Cli =================================================================================================================

#[derive(Parser, Debug)]
#[command(
    name = "goetia",
    version,
    about = "Install system daemons described in goetia.yaml as native services."
)]
pub struct Cli {
    /// Emit one machine-readable JSON document on stdout instead of text
    /// (design spec §4's global flags). Implemented by `daemon list` and
    /// `daemon status`; every other subcommand refuses it with an
    /// `unsupported` error document and exit `2`, before running anything.
    /// `global` so both `goetia --json daemon list` and
    /// `goetia daemon list --json` work.
    #[arg(long, global = true)]
    pub json: bool,
    /// Reserved for increased output verbosity (repeatable). Accepted and
    /// parsed; not read by `dispatch` yet. Locked in deliberately rather
    /// than half-wired to one subcommand's output:
    /// `cli_accepts_verbose_and_quiet_as_currently_inert` pins this so a
    /// silent behavior change (in either direction) fails a test.
    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Reserved to suppress non-essential output. Accepted and parsed; not
    /// read by `dispatch` yet. See `verbose`'s doc comment.
    #[arg(short = 'q', long = "quiet", global = true)]
    pub quiet: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage daemons described in goetia.yaml.
    #[command(subcommand)]
    Daemon(DaemonCommand),
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Register the services in goetia.yaml. Neither starts nor enables
    /// them at boot; see --start/--enable.
    Install(install::Args),
    /// Stop, remove, and reload — for daemon ids no longer present in any
    /// manifest, so this needs no file.
    Uninstall(uninstall::Args),
    /// Start a daemon now. Does not change its boot-enablement.
    Start(start::Args),
    /// Stop a daemon now. Does not change its boot-enablement.
    Stop(stop::Args),
    /// Stop then start a daemon. Does not change its boot-enablement.
    Restart(restart::Args),
    /// Enable a daemon at boot. Does not start it.
    Enable(enable::Args),
    /// Disable a daemon at boot. Does not stop it if running.
    Disable(disable::Args),
    /// Report the live state of one or more installed daemons.
    Status(status::Args),
    /// List every Goetia-managed daemon installed on this host.
    List,
    /// Render a daemon's resolved spec as YAML.
    Show(show::Args),
    /// Show what `install` would change.
    Diff(diff::Args),
}

// dispatch ============================================================================================================

/// Dispatch a parsed [`Cli`] to its subcommand, returning the process exit
/// code: `0` success, `1` error, `2` either a conflict (an installed
/// artifact was modified outside Goetia and `--force` was not given) or
/// `--json` on a subcommand that does not implement it, `4` a partial
/// answer (`list`/`status` could not determine the state of an id Goetia
/// owns) — see the design spec's §4 and [`report::exit_code`], which is
/// where `list` and `status` get theirs in *both* output modes.
///
/// Whenever `--json` is given together with a subcommand, stdout is exactly
/// one JSON document: `list` and `status` render one, and every other
/// subcommand is refused with one below, before it runs. `--help` and
/// `--version` are a deliberate carve-out — clap short-circuits on them
/// before `dispatch` is ever called, so the invariant is over subcommands,
/// not over the binary.
pub fn dispatch(
    cli: &Cli,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let Command::Daemon(cmd) = &cli.command;

    // Checked before the match, so a refused `--json` runs nothing at all —
    // which is also why `unsupported` can never combine with another kind.
    if cli.json && !matches!(cmd, DaemonCommand::List | DaemonCommand::Status(_)) {
        let report = report::unsupported(subcommand_name(cmd));
        report::write(&report, out);
        return report::exit_code(&report);
    }

    match cmd {
        DaemonCommand::Install(args) => install::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Uninstall(args) => uninstall::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Start(args) => start::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Stop(args) => stop::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Restart(args) => restart::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Enable(args) => enable::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Disable(args) => disable::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Status(args) => status::run(args, cli.json, get_manager, out, err),
        DaemonCommand::List => list::run(cli.json, get_manager, out, err),
        DaemonCommand::Show(args) => show::run(args, get_manager, out, err),
        DaemonCommand::Diff(args) => diff::run(args, get_manager, out, err),
    }
}

/// How each subcommand is spelled on the command line, for the `--json`
/// refusal's message. Derived from the enum rather than from clap so a new
/// variant cannot silently be refused as something else.
fn subcommand_name(cmd: &DaemonCommand) -> &'static str {
    match cmd {
        DaemonCommand::Install(_) => "install",
        DaemonCommand::Uninstall(_) => "uninstall",
        DaemonCommand::Start(_) => "start",
        DaemonCommand::Stop(_) => "stop",
        DaemonCommand::Restart(_) => "restart",
        DaemonCommand::Enable(_) => "enable",
        DaemonCommand::Disable(_) => "disable",
        DaemonCommand::Status(_) => "status",
        DaemonCommand::List => "list",
        DaemonCommand::Show(_) => "show",
        DaemonCommand::Diff(_) => "diff",
    }
}
