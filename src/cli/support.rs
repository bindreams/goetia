//! Helpers shared by more than one subcommand module.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, State};
use crate::spec::{DaemonSpec, Id, Warning};

/// Load and resolve a manifest, printing every [`Warning`] it produced to
/// `err`. Every subcommand that parses a manifest calls this rather than
/// `spec::load` directly, so a warning is never silently dropped — see the
/// crate-level design notes on why `Warning`s must reach stderr.
pub(crate) fn load_and_warn(file: &Path, err: &mut dyn Write) -> Result<Vec<DaemonSpec>> {
    let (specs, warnings) = crate::spec::load(file)?;
    print_warnings(&warnings, err);
    Ok(specs)
}

pub(crate) fn print_warnings(warnings: &[Warning], err: &mut dyn Write) {
    for warning in warnings {
        let _ = writeln!(err, "warning: {}: {}", warning.id, warning.message);
    }
}

/// Select the entries of `specs` named by `ids`, in `ids`' order. Empty
/// `ids` selects every entry — the "no ids given means every daemon in the
/// file" rule `install`/`show`/`diff` share. Errors naming the first id
/// with no matching entry, so nothing downstream ever silently operates on
/// a subset smaller than what was actually requested.
pub(crate) fn select_by_ids<'a>(
    specs: &'a [DaemonSpec],
    ids: &[String],
) -> std::result::Result<Vec<&'a DaemonSpec>, String> {
    if ids.is_empty() {
        return Ok(specs.iter().collect());
    }
    ids.iter()
        .map(|id| {
            specs
                .iter()
                .find(|spec| spec.id.as_str() == id)
                .ok_or_else(|| format!("no daemon `{id}` in the manifest"))
        })
        .collect()
}

/// Parse a CLI-supplied id string, positionally — never a path. See
/// `positional_is_always_an_id_never_a_path`.
pub(crate) fn parse_id(s: &str) -> Result<crate::spec::Id> {
    crate::spec::Id::try_from(s.to_string())
}

/// One [`ServiceManager::list`] entry, indexed by id. `list`, `status`,
/// `show`, and `diff` all need this same split — spec/state/enabled for
/// what decoded cleanly, plus which names exist but did not — so it lives
/// here rather than as four independently-maintained copies of the same
/// `match`. A prior copy-per-subcommand version of this logic let `diff`
/// silently diverge from the other three, dropping `OursUnreadable`
/// entries instead of warning about them.
pub(crate) struct InstalledIndex {
    pub ours: BTreeMap<String, (DaemonSpec, State, bool)>,
    pub unreadable: BTreeMap<String, String>,
}

/// Partition `installed` into [`InstalledIndex`]. Prints nothing — call
/// [`print_unreadable_warnings`] to report the `unreadable` half, which
/// every caller must do somewhere: an `OursUnreadable` entry silently
/// vanishing is exactly the failure mode that variant exists to prevent
/// (see [`Installed::OursUnreadable`]'s doc comment).
pub(crate) fn partition_installed(installed: Vec<Installed>) -> InstalledIndex {
    let mut ours = BTreeMap::new();
    let mut unreadable = BTreeMap::new();
    for entry in installed {
        match entry {
            Installed::Ours { spec, state, enabled } => {
                ours.insert(spec.id.as_str().to_string(), (spec, state, enabled));
            }
            Installed::OursUnreadable { name, reason } => {
                unreadable.insert(name, reason);
            }
        }
    }
    InstalledIndex { ours, unreadable }
}

pub(crate) fn print_unreadable_warnings(unreadable: &BTreeMap<String, String>, err: &mut dyn Write) {
    for (name, reason) in unreadable {
        let _ = writeln!(err, "warning: {name}: installed but unreadable: {reason}");
    }
}

pub(crate) fn state_str(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Stopped => "stopped",
        State::Failed => "failed",
        State::Unknown => "unknown",
    }
}

/// The message every mutating subcommand prints and the exit code (`1`) it
/// returns when `is_elevated` reports `false`.
pub(crate) fn require_elevation(
    subcommand: &str,
    is_elevated: &dyn Fn() -> bool,
    err: &mut dyn Write,
) -> std::result::Result<(), i32> {
    if is_elevated() {
        return Ok(());
    }
    let _ = writeln!(
        err,
        "error: {}",
        Error::ElevationRequired {
            subcommand: subcommand.to_string()
        }
    );
    Err(1)
}

/// Bundles [`run_id_verb`]'s parameters — it has more of them than clippy's
/// `too_many_arguments` allows individually, and they belong together as one
/// call anyway.
pub(crate) struct IdVerbCall<'a> {
    pub subcommand: &'a str,
    pub ids: &'a [String],
    pub get_manager: &'a dyn Fn() -> Result<Box<dyn ServiceManager>>,
    pub is_elevated: &'a dyn Fn() -> bool,
    pub verb: &'a dyn Fn(&dyn ServiceManager, &Id) -> Result<()>,
    pub verb_past_tense: &'a str,
}

/// Shared shape for the id-list mutating verbs (`uninstall`, `start`,
/// `stop`, `enable`, `disable`): check elevation once, obtain the manager
/// once, then call `verb` per id, printing one result line per id and
/// aggregating the exit code (`0` if every id succeeded, `1` otherwise).
pub(crate) fn run_id_verb(call: IdVerbCall<'_>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if let Err(code) = require_elevation(call.subcommand, call.is_elevated, err) {
        return code;
    }
    let mgr = match (call.get_manager)() {
        Ok(mgr) => mgr,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

    let mut exit = 0;
    for id_str in call.ids {
        let id = match parse_id(id_str) {
            Ok(id) => id,
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                exit = 1;
                continue;
            }
        };
        match (call.verb)(mgr.as_ref(), &id) {
            Ok(()) => {
                let _ = writeln!(out, "{id}: {}", call.verb_past_tense);
            }
            Err(e) => {
                let _ = writeln!(err, "error: {id}: {e}");
                exit = 1;
            }
        }
    }
    exit
}
