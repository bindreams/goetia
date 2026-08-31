//! The `goetia` command-line interface: argument parsing plus a testable
//! dispatcher.
//!
//! [`dispatch`] is wired to `goetia::manager::native()` by [`crate`'s bin
//! target](../../src/main.rs) — never to [`crate::manager::fake::Fake`]. A
//! CLI that secretly talked to the fake would pass every test here and do
//! nothing on a real machine. Until Tasks 11-13 land, every mutating and
//! every list/status subcommand therefore fails on a real host with `no
//! backend for <platform> yet` — the message [`crate::manager::native`]
//! returns. `install --dry-run` and `show -f <file>` are the exceptions:
//! they need no backend at all (pure generation, or a spec read straight
//! from a file), so they keep working today. `diff` always needs a
//! manager, `-f` or not — comparing against installed state is its entire
//! job.
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
    /// Reserved for machine-readable JSON output (design spec §4's global
    /// flags). Accepted and parsed, but `dispatch` does not read it yet —
    /// every subcommand still renders text regardless. Locked in
    /// deliberately rather than half-wired to one subcommand's output:
    /// `cli_accepts_json_verbose_quiet_as_currently_inert` pins this so a
    /// silent behavior change (in either direction) fails a test.
    #[arg(long, global = true)]
    pub json: bool,
    /// Reserved for increased output verbosity (repeatable). Accepted and
    /// parsed; not read by `dispatch` yet. See `json`'s doc comment.
    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Reserved to suppress non-essential output. Accepted and parsed; not
    /// read by `dispatch` yet. See `json`'s doc comment.
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
/// code: `0` success, `1` error, `2` conflict (an installed artifact was
/// modified outside Goetia and `--force` was not given) — see the design
/// spec's §4.
pub fn dispatch(
    cli: &Cli,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let Command::Daemon(cmd) = &cli.command;
    match cmd {
        DaemonCommand::Install(args) => install::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Uninstall(args) => uninstall::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Start(args) => start::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Stop(args) => stop::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Restart(args) => restart::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Enable(args) => enable::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Disable(args) => disable::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Status(args) => status::run(args, get_manager, out, err),
        DaemonCommand::List => list::run(get_manager, out, err),
        DaemonCommand::Show(args) => show::run(args, get_manager, out, err),
        DaemonCommand::Diff(args) => diff::run(args, get_manager, out, err),
    }
}
