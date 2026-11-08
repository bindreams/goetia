//! Helpers shared by more than one subcommand module.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::Write;
use std::path::Path;

use super::ExitCode;
use crate::error::{Error, Reason, Result, Sink, escape};
use crate::manager::{Installed, ServiceManager, State};
use crate::spec::{DaemonSpec, Id, Warning};

/// Load and resolve a manifest, printing every [`Warning`] it produced to
/// `err`. Every subcommand that parses a manifest calls this rather than
/// `spec::load`, so a warning is never silently dropped.
pub(crate) fn load_and_warn(file: &Path, err: &mut dyn Write) -> Result<DaemonSpec> {
    let (spec, warnings) = crate::spec::load(file)?;
    print_warnings(&warnings, err);
    Ok(spec)
}

/// `prefix` and then what `body` writes through a [`Sink`], which escapes it. `prefix` is written
/// as it is: the caller has escaped it.
fn render_line(prefix: &str, body: impl FnOnce(&mut Sink) -> fmt::Result) -> String {
    let mut line = prefix.to_string();
    body(&mut Sink::new(&mut line, true)).expect("writing to a String cannot fail");
    line
}

/// Writes one line to `to`: `label: ` and `scope: ` (each when there is one), then `body`. The
/// scope and what `body` writes through its [`Sink`] are escaped; `label` is written as it is.
pub(crate) fn print_line(
    to: &mut dyn Write,
    label: Option<&str>,
    scope: Option<&str>,
    body: impl FnOnce(&mut Sink) -> fmt::Result,
) {
    let _ = writeln!(to, "{}", render_line(&line_prefix(label, scope), body));
}

/// `label: ` and `scope: `, each when there is one; the scope's text is escaped.
fn line_prefix(label: Option<&str>, scope: Option<&str>) -> String {
    let label = label.map(|label| format!("{label}: "));
    let scope = scope.map(|scope| format!("{}: ", escape(scope)));
    label.into_iter().chain(scope).collect()
}

/// Writes `error: [id: ]what` and the report of `e`, which says what failed; `what` is goetia's
/// own words that lead it (`enable: `).
pub(crate) fn print_error(err: &mut dyn Write, id: Option<&str>, what: &str, e: &Error) {
    print_line(err, Some("error"), id, |sink| {
        sink.write_str(what)?;
        e.write_report(sink)
    });
}

/// Renders `warning: <id>: <message>` for a daemon's advisory and
/// `warning: <message>` for a manifest-level one. Only the drive-relative
/// `-f` advisory (`spec::resolve`'s `absolutize`) takes the second arm: it
/// belongs to no daemon.
pub(crate) fn print_warnings(warnings: &[Warning], err: &mut dyn Write) {
    for warning in warnings {
        let id = warning.id.as_ref().map(Id::as_str);
        print_line(err, Some("warning"), id, |sink| sink.write_str(&warning.message));
    }
}

/// Parse a CLI-supplied id string, positionally — never a path.
pub(crate) fn parse_id(s: &str) -> Result<crate::spec::Id> {
    crate::spec::Id::try_from(s.to_string())
}

/// One decoded [`Installed::Ours`] entry, as held by [`InstalledIndex`].
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
    pub reason: Reason,
}

/// [`ServiceManager::list`]'s entries, indexed by id: spec/state/pid/enabled
/// for what decoded cleanly, plus which names exist but did not. Shared by
/// `list`, `status`, `show` and `diff`.
pub(crate) struct InstalledIndex {
    pub ours: BTreeMap<String, InstalledEntry>,
    pub unusable: BTreeMap<String, Reason>,
    /// What goetia could not classify at all: named entries first, sorted
    /// by name, then any aggregate. Not a map: an aggregate carries no name
    /// to key on (see [`Installed::Undetermined`]).
    pub undetermined: Vec<UndeterminedEntry>,
}

impl InstalledIndex {
    /// The entry that makes "`id` is not installed" unsound, if there is one:
    /// the entry naming `id`, or *any* aggregate, which may stand for `id`
    /// itself. Applies the null-name rule on [`Installed::Undetermined`] once.
    pub fn undetermined_for(&self, id: &str) -> Option<&UndeterminedEntry> {
        self.undetermined
            .iter()
            .find(|entry| entry.name.as_deref() == Some(id))
            .or_else(|| self.undetermined.iter().find(|entry| entry.name.is_none()))
    }
}

/// Partition `installed` into [`InstalledIndex`]. Prints nothing; every caller
/// must report the `unusable` half with [`print_unusable_warnings`], since an
/// `OursUnusable` entry silently vanishing is what that variant exists to
/// prevent (see [`Installed::OursUnusable`]).
pub(crate) fn partition_installed(installed: Vec<Installed>) -> InstalledIndex {
    let mut ours = BTreeMap::new();
    let mut unusable = BTreeMap::new();
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
            Installed::OursUnusable { name, reason } => {
                unusable.insert(name, reason);
            }
            Installed::Undetermined { name, reason } => {
                undetermined.push(UndeterminedEntry { name, reason });
            }
        }
    }
    // Named entries by name, then the aggregates. Sorted here, not in
    // `report::write`, because `list`'s text renderer reads this index
    // directly: one sort site keeps both outputs stable.
    undetermined.sort_by(|a, b| (a.name.is_none(), a.name.as_deref()).cmp(&(b.name.is_none(), b.name.as_deref())));
    // The three classes are mutually exclusive claims about one id, and
    // nothing else checks it: otherwise the winner would be whichever branch a
    // caller tests first.
    debug_assert!(
        ours.keys().all(|id| !unusable.contains_key(id))
            && undetermined
                .iter()
                .filter_map(|entry| entry.name.as_deref())
                .all(|name| !ours.contains_key(name) && !unusable.contains_key(name)),
        "a backend put one id in two of list()'s classes"
    );
    InstalledIndex {
        ours,
        unusable,
        undetermined,
    }
}

pub(crate) fn print_unusable_warnings(unusable: &BTreeMap<String, Reason>, err: &mut dyn Write) {
    for (name, reason) in unusable {
        print_line(err, Some("warning"), Some(name), |sink| {
            reason.installed_but_unusable().write_to(sink)
        });
    }
}

/// Report the `undetermined` half, as [`print_unusable_warnings`] does the
/// other. An aggregate's `reason` is a complete sentence and stands alone.
pub(crate) fn print_undetermined_warnings(undetermined: &[UndeterminedEntry], err: &mut dyn Write) {
    for entry in undetermined {
        print_line(err, Some("warning"), entry.name.as_deref(), |sink| {
            if entry.name.is_some() {
                sink.write_str("installation state could not be determined: ")?;
            }
            entry.reason.write_to(sink)
        });
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

/// The message every mutating subcommand prints and the exit code
/// ([`ExitCode::Error`]) it returns when `is_elevated` reports `false`.
pub(crate) fn require_elevation(
    subcommand: &str,
    is_elevated: &dyn Fn() -> bool,
    err: &mut dyn Write,
) -> std::result::Result<(), ExitCode> {
    if is_elevated() {
        return Ok(());
    }
    let e = Error::ElevationRequired {
        what: format!("`{subcommand}`"),
    };
    print_error(err, None, "", &e);
    Err(ExitCode::Error)
}

/// [`run_id_verb`]'s parameters, bundled to stay under clippy's
/// `too_many_arguments`.
pub(crate) struct IdVerbCall<'a> {
    pub subcommand: &'a str,
    pub ids: &'a [String],
    pub get_manager: &'a dyn Fn() -> Result<Box<dyn ServiceManager>>,
    pub is_elevated: &'a dyn Fn() -> bool,
    pub verb: &'a dyn Fn(&dyn ServiceManager, &Id) -> Result<()>,
    /// The word printed after an id that succeeded. It follows the budget: a
    /// verb that waited reports `started`, one that did not `start requested`
    /// (see [`super::wait::reported`]).
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
    /// | `start`, `restart`, `enable`, `kill` | 1 | cannot act on what is not there |
    ///
    /// The table is about an *absent* artifact: an id whose absence was not
    /// established is `4` for all seven verbs, this flag included.
    ///
    /// Lives here, never inside the trait: tolerating absence inside `stop`
    /// itself would let `restart` on an absent id fall through to `start`.
    ///
    /// The obligation this places on a [`ServiceManager`]: `Error::
    /// NotInstalled` must mean *nothing goetia-attributable is at this id*,
    /// not "one particular file is missing". This layer sees only the error
    /// variant and cannot check that; a backend reporting it too eagerly makes
    /// `uninstall x && echo "confirmed gone"` print for an id its own `install`
    /// would refuse as foreign. See `backend::systemd::manager`'s "absence is
    /// about the id" item.
    pub absent_is_success: bool,
}

/// Shared shape for the id-list mutating verbs (`uninstall`, `start`,
/// `stop`, `kill`, `enable`, `disable`, `restart`): check elevation once, obtain
/// the manager once, then call `verb` per id, printing one result line per
/// id and returning the greatest [`ExitCode`] any id contributed, except that
/// when `call.absent_is_success` and `verb` returns `Error::NotInstalled`, the
/// id counts as succeeded; see [`IdVerbCall::absent_is_success`].
///
/// Every id is parsed *before* `verb` is called for any of them, so a bad id
/// at position N cannot surface after ids `1..N-1` were already mutated.
pub(crate) fn run_id_verb(call: IdVerbCall<'_>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let ids: std::result::Result<Vec<Id>, Error> = call.ids.iter().map(|s| parse_id(s)).collect();
    let ids = match ids {
        Ok(ids) => ids,
        Err(e) => {
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    if let Err(code) = require_elevation(call.subcommand, call.is_elevated, err) {
        return code;
    }
    let mgr = match (call.get_manager)() {
        Ok(mgr) => mgr,
        Err(e) => {
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    let mut codes: Vec<ExitCode> = Vec::new();
    for id in &ids {
        match (call.verb)(mgr.as_ref(), id) {
            Ok(()) => {
                let _ = writeln!(out, "{id}: {}", call.verb_past_tense);
                codes.push(ExitCode::Success);
            }
            // Absence already satisfies this verb's goal (see
            // `absent_is_success`). Not the past-tense line: "uninstalled"
            // would be false for a daemon that was never there.
            Err(e) if call.absent_is_success && matches!(e.cause(), Error::NotInstalled { .. }) => {
                let _ = writeln!(out, "{id}: not installed (nothing to do)");
                codes.push(ExitCode::Success);
            }
            Err(e) => {
                print_error(err, Some(id.as_str()), "", &e);
                codes.push(ExitCode::of(&e));
            }
        }
    }
    codes.into_iter().max().unwrap_or(ExitCode::Success)
}

#[cfg(test)]
#[path = "support_tests.rs"]
mod support_tests;
