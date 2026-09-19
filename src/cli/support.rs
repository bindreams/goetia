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

/// Renders `warning: <id>: <message>` for a daemon's advisory and
/// `warning: <message>` for a manifest-level one. Only the drive-relative
/// `-f` advisory (`spec::resolve`'s `absolutize`) ever takes the second
/// arm: it belongs to no daemon, so prefixing it with one would name a
/// daemon the warning is not about.
pub(crate) fn print_warnings(warnings: &[Warning], err: &mut dyn Write) {
    for warning in warnings {
        let _ = match &warning.id {
            Some(id) => writeln!(err, "warning: {id}: {}", warning.message),
            None => writeln!(err, "warning: {}", warning.message),
        };
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

/// One decoded [`Installed::Ours`] entry, as held by [`InstalledIndex`].
/// Named rather than a tuple: a four-tuple at three call sites is exactly
/// where a field gets read back in the wrong position.
pub(crate) struct InstalledEntry {
    pub spec: DaemonSpec,
    pub state: State,
    pub pid: Option<u32>,
    pub enabled: bool,
}

/// One [`Installed::Undetermined`] entry, as held by [`InstalledIndex`].
pub(crate) struct UndeterminedEntry {
    /// `None` for an entry standing for more than one id — see
    /// [`Installed::Undetermined`], whose doc comment carries the rule
    /// every caller owes such an entry.
    pub name: Option<String>,
    pub reason: String,
}

/// One [`ServiceManager::list`] entry, indexed by id. `list`, `status`,
/// `show`, and `diff` all need this same split — spec/state/pid/enabled for
/// what decoded cleanly, plus which names exist but did not — so it lives
/// here rather than as four independently-maintained copies of the same
/// `match`.
pub(crate) struct InstalledIndex {
    pub ours: BTreeMap<String, InstalledEntry>,
    pub unreadable: BTreeMap<String, String>,
    /// What goetia could not classify at all: named entries first, sorted
    /// by name, then any aggregate. Not a map — an entry may stand for more
    /// than one id and so carry no name (see [`Installed::Undetermined`]),
    /// which leaves nothing to key on.
    pub undetermined: Vec<UndeterminedEntry>,
}

impl InstalledIndex {
    /// The entry that makes "`id` is not installed" an unsound conclusion,
    /// if there is one: the entry naming `id`, or *any* aggregate — which
    /// may stand for `id` itself. The null-name rule on
    /// [`Installed::Undetermined`] applied once, so no caller has to know
    /// which backend produced the listing.
    pub fn undetermined_for(&self, id: &str) -> Option<&UndeterminedEntry> {
        self.undetermined
            .iter()
            .find(|entry| entry.name.as_deref() == Some(id))
            .or_else(|| self.undetermined.iter().find(|entry| entry.name.is_none()))
    }
}

/// Partition `installed` into [`InstalledIndex`]. Prints nothing — call
/// [`print_unreadable_warnings`] to report the `unreadable` half, which
/// every caller must do somewhere: an `OursUnreadable` entry silently
/// vanishing is exactly the failure mode that variant exists to prevent
/// (see [`Installed::OursUnreadable`]'s doc comment).
pub(crate) fn partition_installed(installed: Vec<Installed>) -> InstalledIndex {
    let mut ours = BTreeMap::new();
    let mut unreadable = BTreeMap::new();
    let mut undetermined = Vec::new();
    for entry in installed {
        match entry {
            Installed::Ours {
                spec,
                state,
                pid,
                enabled,
            } => {
                ours.insert(
                    spec.id.as_str().to_string(),
                    InstalledEntry {
                        spec,
                        state,
                        pid,
                        enabled,
                    },
                );
            }
            Installed::OursUnreadable { name, reason } => {
                unreadable.insert(name, reason);
            }
            Installed::Undetermined { name, reason } => {
                undetermined.push(UndeterminedEntry { name, reason });
            }
        }
    }
    // Named entries by name, then the aggregates. Sorted here rather than in
    // `report::write`, because `list`'s text renderer reads this index
    // directly and never goes through the `Report`: two sort sites could
    // drift, and the ordering is what makes either output stable.
    undetermined.sort_by(|a, b| (a.name.is_none(), a.name.as_deref()).cmp(&(b.name.is_none(), b.name.as_deref())));
    // The three classes are mutually exclusive claims about one id —
    // decoded, ours-but-unreadable, unclassifiable — so a backend reporting
    // an id in two of them has answered its own question twice. Nothing else
    // checks it, and without this the winner would be whichever branch a
    // caller happens to test first.
    debug_assert!(
        ours.keys().all(|id| !unreadable.contains_key(id))
            && undetermined
                .iter()
                .filter_map(|entry| entry.name.as_deref())
                .all(|name| !ours.contains_key(name) && !unreadable.contains_key(name)),
        "a backend put one id in two of list()'s classes"
    );
    InstalledIndex {
        ours,
        unreadable,
        undetermined,
    }
}

pub(crate) fn print_unreadable_warnings(unreadable: &BTreeMap<String, String>, err: &mut dyn Write) {
    for (name, reason) in unreadable {
        let _ = writeln!(err, "warning: {name}: installed but unreadable: {reason}");
    }
}

/// Report the `undetermined` half, as [`print_unreadable_warnings`] does
/// the other. An aggregate's `reason` is a complete sentence and stands
/// alone: there is no name to prefix it with.
pub(crate) fn print_undetermined_warnings(undetermined: &[UndeterminedEntry], err: &mut dyn Write) {
    for entry in undetermined {
        let _ = match &entry.name {
            Some(name) => writeln!(
                err,
                "warning: {name}: installation state could not be determined: {}",
                entry.reason
            ),
            None => writeln!(err, "warning: {}", entry.reason),
        };
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
    /// The word printed after an id that succeeded. Supplied by the caller
    /// because it follows the budget: a verb that waited confirmed something
    /// and reports `started`, while one that did not reports
    /// `start requested` — see [`super::wait::reported`].
    pub verb_past_tense: &'a str,
    /// Whether an absent *artifact* already satisfies this verb's goal.
    /// True for `uninstall` alone; every other id-verb keeps `Error::
    /// NotInstalled` as a plain failure:
    ///
    /// | verb | absent artifact | why |
    /// |---|---|---|
    /// | `uninstall` | **0** | artifact absence is exactly what it asks for |
    /// | `stop` | 1 | a running unit whose fragment was deleted keeps running, so `stop x && echo "confirmed down"` would print that with `x` alive |
    /// | `disable` | 1 | disabling after the fragment is gone is impossible, leaving a dangling `.wants` symlink: exit 0 while still enabled at boot |
    /// | `start`, `restart`, `enable` | 1 | cannot act on what is not there |
    ///
    /// The table is about an *absent* artifact, so it never reaches an id
    /// whose absence was not established: an undetermined id is `4` for all
    /// six verbs, this flag included.
    ///
    /// Lives here, never inside the trait: `restart`'s closure calls
    /// `mgr.stop(id)?` before `mgr.start(id)`, and tolerating absence
    /// inside `stop` itself would let `restart` on an absent id fall
    /// through to `start`.
    ///
    /// The obligation this places on a [`ServiceManager`]: `Error::
    /// NotInstalled` must mean *nothing goetia-attributable is at this id*,
    /// not "one particular file is missing". This layer cannot check that —
    /// it sees an error variant, never the backend's own evidence — so a
    /// backend reporting it too eagerly makes `uninstall x && echo
    /// "confirmed gone"` print for an id its own `install` would refuse as
    /// foreign. See `backend::systemd::manager`'s obligation 7.
    pub absent_is_success: bool,
}

/// Shared shape for the id-list mutating verbs (`uninstall`, `start`,
/// `stop`, `enable`, `disable`, `restart`): check elevation once, obtain
/// the manager once, then call `verb` per id, printing one result line per
/// id and combining what each id contributed by [`super::report::precedence`] —
/// the same rule `install`, `diff` and `show` use — except that when
/// `call.absent_is_success` and `verb` returns `Error::NotInstalled`, the
/// id counts as succeeded rather than failed; see
/// [`IdVerbCall::absent_is_success`].
///
/// Every id is parsed *before* `verb` is called for any of them — the same
/// all-or-nothing rule `select_by_ids` documents above. Parsing lazily,
/// inside the loop, would mean a bad id at position N only surfaces after
/// ids `1..N-1` have already been mutated (e.g. uninstalled): a partially
/// executed, irreversible operation the caller never asked for and the
/// nonzero exit code alone does not disclose.
pub(crate) fn run_id_verb(call: IdVerbCall<'_>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let ids: std::result::Result<Vec<Id>, Error> = call.ids.iter().map(|s| parse_id(s)).collect();
    let ids = match ids {
        Ok(ids) => ids,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return 1;
        }
    };

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

    let mut codes: Vec<i32> = Vec::new();
    for id in &ids {
        match (call.verb)(mgr.as_ref(), id) {
            Ok(()) => {
                let _ = writeln!(out, "{id}: {}", call.verb_past_tense);
                codes.push(0);
            }
            // Absence already satisfies this verb's goal (`uninstall`
            // alone — see `absent_is_success`'s doc comment). Stdout only,
            // never the past-tense line: claiming e.g. "uninstalled" for a
            // daemon that was never there trades one wrong report for
            // another. Does not affect the aggregated exit code.
            Err(Error::NotInstalled { .. }) if call.absent_is_success => {
                let _ = writeln!(out, "{id}: not installed (nothing to do)");
                codes.push(0);
            }
            // What is at the id was not established, which is one condition
            // with one remedy for every verb here — including `uninstall`,
            // whose exemption is about absence, not about the question going
            // unanswered. Not "nothing was done": `restart` keeps an
            // `Undetermined` start leg as it is after a stop it did issue
            // (`restart::after_start_failed`), and its reason says so.
            Err(e @ Error::Undetermined { .. }) => {
                let _ = writeln!(err, "error: {id}: {e}");
                codes.push(4);
            }
            // A wait that ran out of budget. The same class as the arm
            // above, for the same reason — the code is about whether the
            // question was answered — but a different condition: this one
            // answered what is at the id and left the *outcome* open. The
            // catch-all below would report `1`, which says the verb
            // determinately failed, and nothing goetia issued was cancelled,
            // so that is exactly what was not established. `restart`'s start
            // leg is the one expiry with no request behind it — its budget ran
            // out before it — and it is still a timeout, not a failure (see
            // `restart::spent_before_start`).
            Err(e @ Error::WaitTimeout { .. }) => {
                let _ = writeln!(err, "error: {id}: {e}");
                codes.push(4);
            }
            // Steps that were issued and whose outcome nobody established —
            // `restart` with no budget, whose start leg was refused. The same
            // class again, and again a different condition: here both steps
            // were issued and neither settled where the daemon ended up, even
            // when the start's refusal was itself determinate.
            Err(e @ Error::Unestablished { .. }) => {
                let _ = writeln!(err, "error: {id}: {e}");
                codes.push(4);
            }
            // A request goetia lost track of: it may or may not have reached the manager, or did
            // and its outcome is unconfirmed. The same class once more: `1` says the verb was
            // attempted and failed, or was refused, and neither is what was established.
            Err(e @ Error::RequestInDoubt { .. }) => {
                let _ = writeln!(err, "error: {id}: {e}");
                codes.push(4);
            }
            Err(e) => {
                let _ = writeln!(err, "error: {id}: {e}");
                codes.push(1);
            }
        }
    }
    codes
        .into_iter()
        .max_by_key(|code| super::report::precedence(*code))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "support_tests.rs"]
mod support_tests;
