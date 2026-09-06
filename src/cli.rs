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
    /// `goetia daemon list --json` work. See [`dispatch`] for the exact
    /// invariant and its clap-level carve-outs.
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
/// code. This doc comment is the one place the whole exit-code vocabulary
/// is written down; nothing else in the crate should re-derive it.
///
/// - `0` success.
/// - `1` error: an operation was attempted and failed, or was refused
///   outright.
/// - `2` usage: clap rejected the command line before `dispatch` ever ran,
///   or (`--json` on a subcommand that does not implement it) `dispatch`
///   refused to run anything. Anchored to clap's own default for a
///   rejected command line — the same code bash and argparse both use for
///   "the parser, not the program, rejected this" — so `main.rs`
///   deliberately keeps calling `Cli::parse()` un-overridden and lets clap
///   return `2` on its own; the absence of an override *is* the decision.
/// - `3` drift: reserved for a determinate "installed state differs from
///   the manifest" answer. Nothing produces it yet.
/// - `4` indeterminate: `list`/`status` could not determine the state of
///   an id Goetia owns — see the design spec's §4 and [`report::exit_code`],
///   which is where `list` and `status` get theirs in *both* output modes.
///   Anchored to the LSB init-script convention's "service status unknown",
///   the one other exit-code vocabulary this one deliberately agrees with.
/// - `5` conflict: an installed artifact was modified outside Goetia and
///   `--force` was not given (`cli::install::run`'s `any_conflict` check —
///   the only place this code is returned). App-specific, anchored to
///   nothing, which is why it is the one that moved: `2` is anchored to
///   three conventions at once (clap, bash, argparse), so moving *usage*
///   errors off it instead would have stayed internally consistent while
///   giving up all three to preserve one number nothing outside Goetia
///   agrees on.
///
/// `list` and `status` compute their code as the precedence-max over every
/// error kind their `Report` collected: `1 > 4 > 5 > 3 > 0`. That is a rule
/// about which outcome wins when more than one applies at once, not an
/// ordering of the integers — `5` outranks `3` despite being the larger
/// number. `2` never enters that ladder: the one thing that produces it
/// there (`--json` on a subcommand that does not implement it) always
/// happens alone, before any other kind could exist in the same report.
///
/// Whenever `--json` is given together with a subcommand **that clap
/// accepted**, stdout is exactly one JSON document: `list` and `status`
/// render one, and every other subcommand is refused with one below, before
/// it runs.
///
/// Anything clap short-circuits on is a deliberate carve-out, because it
/// happens before `dispatch` is ever called: `--help`/`--version` print
/// their own text and exit `0`, and a usage error (`goetia --json daemon
/// uninstall`, with `<IDS>` missing) prints clap's message to stderr, leaves
/// stdout empty, and exits `2`. Rendering those as JSON would mean
/// pre-scanning `std::env::args()` before parsing, or intercepting
/// `try_parse` and re-rendering clap's own diagnostics — both worse than the
/// carve-out. `tests/cli_binary.rs` pins all three cases.
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
