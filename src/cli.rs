//! The `goetia` command-line interface: argument parsing plus a testable
//! dispatcher.
//!
//! [`dispatch`] is wired to `goetia::manager::native()` by [`crate`'s bin
//! target](../../src/main.rs), never to [`crate::manager::fake::Fake`]: a CLI
//! that secretly talked to the fake would pass every test here and do nothing
//! on a real machine. On a platform without a backend, every subcommand
//! that needs a manager therefore fails with the `no backend for <platform>
//! yet` message [`crate::manager::native`] returns. `install --dry-run` and
//! `show -f <file>` need no backend (pure generation, or a spec read from a
//! file). `diff` always needs one, `-f` or not.
//!
//! `get_manager`, `is_elevated` and `host` are lazily-invoked closures so that a
//! subcommand that needs none of them never calls it; tests hand in a closure that
//! panics if called. `host` names the command a Windows `type: simple` service is
//! registered to run: it is called only by `install --dry-run` of such a daemon,
//! which has no manager to ask. Linux and macOS never call it.

pub mod diff;
pub mod disable;
pub mod enable;
mod exit;
pub mod install;
pub mod kill;
pub mod list;
mod refusal;
mod report;
pub mod restart;
pub mod show;
pub mod start;
pub mod status;
pub mod stop;
mod support;
pub mod uninstall;
pub mod wait;

use std::io::Write;

use clap::{Parser, Subcommand};

pub use exit::ExitCode;

use crate::error::{EscapedStream, Result};
use crate::manager::ServiceManager;
use crate::shim::HostCommand;
#[cfg(windows)]
use crate::shim::{CheckPermissions, ShimProvider};

// Windows providers ===================================================================================================

/// The Windows manager whose `type: simple` services run the host `provider` supplies, with the
/// folder permission check on.
#[cfg(windows)]
pub fn manager(provider: impl ShimProvider + 'static) -> Result<Box<dyn ServiceManager>> {
    Ok(Box::new(crate::backend::scm::manager::Scm::with_shim(
        provider,
        CheckPermissions::Enable,
    )))
}

/// goetia.exe's own provider for a build without the stock shim embedded (`cargo install`): it
/// reads `goetia-shim.exe` beside `current_exe()` and places a copy under Program Files. The type
/// stays crate-private; a consumer supplies its own provider.
#[cfg(windows)]
pub fn sibling_stock_shim() -> impl ShimProvider {
    crate::backend::scm::shim::embedded::SiblingStockShim::new()
}

// Cli =================================================================================================================

/// Parses `args` as clap does, except that what clap would print goes through the escaping streams
/// like all other output: a usage error quotes the argument as typed. `Err` is the exit code, with
/// the text already written, to `out` for help and version and to `err` otherwise, as clap does.
pub fn parse<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> std::result::Result<Cli, i32>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(args).map_err(|e| {
        // `StyledStr`'s `Display` drops clap's colour, whose ESC bytes the stream would escape.
        let mut stream = EscapedStream::new(if e.use_stderr() { err } else { out });
        let _ = write!(stream, "{}", e.render());
        let _ = stream.flush();
        e.exit_code()
    })
}

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
    /// invariant.
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
    /// Register the service in goetia.yaml. Neither starts nor enables
    /// it at boot; see --start/--enable.
    Install(install::Args),
    /// Stop, remove, and reload — for daemon ids no longer present in any
    /// manifest, so this needs no file.
    Uninstall(uninstall::Args),
    /// Start a daemon now. Does not change its boot-enablement.
    Start(start::Args),
    /// Stop a daemon now. Does not change its boot-enablement.
    Stop(stop::Args),
    /// Kill a daemon's running process as a failure, leaving any restart to its restart policy.
    Kill(kill::Args),
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
/// is written down; nothing else in the crate should re-derive it. Each name
/// below is an [`ExitCode`] variant.
///
/// - `0` success. `uninstall` alone also counts an already-absent artifact
///   as success — `Error::NotInstalled` says nothing about the *process*,
///   only the artifact, which is why every other id-verb keeps that same
///   error at `1` instead; see `IdVerbCall::absent_is_success`'s doc
///   comment for the full seven-verb table. An id whose absence was not
///   established is `4` for all seven verbs, `uninstall` included.
/// - `1` error: an operation was attempted and failed, or was refused
///   outright. `kill` of a daemon with no process is this
///   ([`Error::NotRunning`](crate::error::Error::NotRunning)): nothing was killed.
/// - `2` usage: clap rejected the command line before `dispatch` ever ran,
///   or `dispatch` refused to run anything: `--json` on a subcommand that
///   does not implement it, or a wait flag on an `install` with no `--start`
///   to bound. Anchored to clap's default for a rejected command line (also
///   bash's and argparse's), so `main.rs` keeps calling `Cli::parse()`
///   un-overridden and lets clap return `2` on its own.
/// - `3` drift: a determinate "installed state differs from the manifest"
///   answer — `diff` returns it when the daemon would change
///   (`cli::diff::run`, the only place this code is returned).
/// - `4` indeterminate: a question Goetia could not answer, or an operation
///   whose outcome it could not establish. For the id verbs, `install` and
///   `diff`, a failed operation is this exactly when
///   [`Error::is_indeterminate`](crate::error::Error::is_indeterminate).
///   `status <id>` instead maps its errors through `report::status_error`, where
///   anything it cannot attribute is `unusable` (`4`). The rest come
///   from the answer itself: `list`/`status` report an id they own but cannot
///   report on, and an entry `list()` could not classify, as `4`; `diff` and
///   `show` do the same for an "installed but unusable" id, and `show` also for
///   *any* id it cannot find while an aggregate undetermined entry is present,
///   since a null-named entry makes "not installed" unsound for every id at
///   once. `install` reports `Outcome::RefuseUnusable` as `1` because it
///   genuinely failed to install. Anchored to the LSB init-script convention's
///   "service status unknown", the one other exit-code vocabulary this one
///   deliberately agrees with.
/// - `5` conflict: an installed artifact was modified outside Goetia and
///   `--force` was not given. Returned by `cli::install::run` and by
///   `cli::diff::run` for the identical `Outcome::Conflict` — the two places
///   this code is returned. App-specific and anchored to nothing, unlike `2`.
///
/// When more than one outcome applies at once (several ids, or the steps of
/// one `install`), the verb returns the greatest [`ExitCode`], which is
/// declared in precedence order: `1 > 4 > 5 > 3 > 0`. That is a rule about
/// which outcome wins, not an ordering of the integers — `5` outranks `3`
/// despite being the larger number. `diff` answers about one daemon and has
/// nothing to combine. `2` never enters that ladder: both refusals that
/// produce it there (`--json` on a subcommand that does not implement it,
/// and a wait flag on an `install` with no `--start`) always happen alone,
/// before any other kind could exist in the same report.
///
/// Whenever `--json` is given together with a subcommand **that clap
/// accepted**, stdout is exactly one JSON document: `list` and `status`
/// render one, and every other subcommand is refused with one below, before
/// it runs.
///
/// What clap short-circuits on never reaches `dispatch`; [`parse`] answers it: `--help` and
/// `--version` print their text and exit `0`, and a usage error (`goetia --json daemon uninstall`,
/// with `<IDS>` missing) is plain text on stderr, with stdout empty, and exits `2`.
pub fn dispatch(
    cli: &Cli,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    host: &dyn Fn() -> Result<HostCommand>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // `out` is left alone under `--json`, which carries the exact text: U+2028 and the direction
    // controls are valid JSON as they are, where the wrapper's `\u{2028}` is not.
    let mut err = EscapedStream::new(err);
    let mut escaped_out;
    let out: &mut dyn Write = if cli.json {
        out
    } else {
        escaped_out = EscapedStream::new(out);
        &mut escaped_out
    };
    run(cli, get_manager, is_elevated, host, out, &mut err).code()
}

fn run(
    cli: &Cli,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
    host: &dyn Fn() -> Result<HostCommand>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let Command::Daemon(cmd) = &cli.command;

    // Checked before the match, so a refused `--json` runs nothing at all —
    // which is also why `unsupported` can never combine with another kind.
    if cli.json && !matches!(cmd, DaemonCommand::List | DaemonCommand::Status(_)) {
        let report = report::unsupported(subcommand_name(cmd));
        return report::emit(&report, out, err);
    }

    // A command line goetia will not act on must run nothing, and these rules
    // live here rather than split with clap. `install` without `--start`
    // starts nothing, so a budget named for that start can never be spent.
    // `requires = "start"` cannot express it: `--dry-run` makes `--start` and
    // the wait flags alike inert, and must keep accepting all three.
    // After the `--json` refusal, so that one wins when both apply.
    if matches!(cmd, DaemonCommand::Install(args) if args.wait.was_given() && !args.start && !args.dry_run) {
        let _ = writeln!(
            err,
            "error: `--timeout`/`--no-timeout` need `--start`: `daemon install` on its own starts \
             nothing, so there is nothing to wait for"
        );
        return ExitCode::Usage;
    }

    match cmd {
        DaemonCommand::Install(args) => install::run(args, get_manager, is_elevated, host, out, err),
        DaemonCommand::Uninstall(args) => uninstall::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Start(args) => start::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Stop(args) => stop::run(args, get_manager, is_elevated, out, err),
        DaemonCommand::Kill(args) => kill::run(args, get_manager, is_elevated, out, err),
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
        DaemonCommand::Kill(_) => "kill",
        DaemonCommand::Restart(_) => "restart",
        DaemonCommand::Enable(_) => "enable",
        DaemonCommand::Disable(_) => "disable",
        DaemonCommand::Status(_) => "status",
        DaemonCommand::List => "list",
        DaemonCommand::Show(_) => "show",
        DaemonCommand::Diff(_) => "diff",
    }
}
