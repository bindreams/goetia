//! The `--json` wire format shared by `daemon list` and `daemon status`.
//!
//! One compact JSON object, newline-terminated, on stdout:
//!
//! ```json
//! {"daemons":[{"id":"frpc","state":"running","enabled":true,"pid":1234}],"errors":[]}
//! ```
//!
//! `daemons` and `errors` are always present, as arrays, possibly empty.
//! `name` is deliberately absent: [`crate::manager::ServiceManager::status`]
//! returns a [`Status`] with no spec, so only `list` could supply it, and one
//! field differing between the two subcommands is worse than sending users to
//! `goetia daemon show`. `pid` is an integer or `null`, and `null` means the
//! manager reports no main process — never "this command could not find out",
//! which is an `errors` entry instead.
//!
//! [`exit_code`] is the *only* exit-code computation `list` and `status` have,
//! with or without `--json`: both build a [`Report`] first and only then pick
//! a renderer. A verb whose exit code depends on its output format is exactly
//! the split this module exists to remove.

use std::io::Write;

use super::support::{InstalledIndex, state_str};
use crate::error::Error;
use crate::manager::Status;

// Wire format =========================================================================================================

#[derive(serde::Serialize)]
pub(crate) struct Report {
    pub daemons: Vec<DaemonReport>,
    pub errors: Vec<ErrorReport>,
}

#[derive(serde::Serialize)]
pub(crate) struct DaemonReport {
    pub id: String,
    pub state: &'static str,
    pub enabled: bool,
    pub pid: Option<u32>,
}

#[derive(serde::Serialize)]
pub(crate) struct ErrorReport {
    /// The daemon id or service name the failure is attributable to, or
    /// `None` when it is not — exactly the `unavailable` and `unsupported`
    /// cases.
    pub id: Option<String>,
    pub kind: Kind,
    pub message: String,
}

/// Every value [`ErrorReport::kind`] can take.
///
/// A `kind` is chosen **at the call site, from which operation failed** —
/// never by matching on [`Error`]'s variant alone. [`Error::Invalid`] is
/// produced both by `support::parse_id` and by blob-content validation, so
/// the variant does not determine the remedy. The constructors below are
/// therefore named for the operation whose failure they classify, not for
/// the error they receive.
///
/// An enum rather than a set of `&'static str` constants so that
/// [`Kind::code`] is an exhaustive `match`: a kind added without a code
/// stops compiling. A test that hand-listed the kinds instead could never
/// be complete, since it would list exactly what it was checking for
/// completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Nothing is installed at that id. Only from `status(&id)`.
    NotInstalled,
    /// Something exists there and demonstrably carries no goetia marker —
    /// established by a read that completed, never inferred from one that
    /// failed, which is [`Kind::Undetermined`]'s case. Only from
    /// `status(&id)`.
    Foreign,
    /// Goetia owns the id but cannot report on it; `message` says why — a
    /// blob it cannot decode, a live state it could not query, or an
    /// ambiguous installation. From [`Installed::OursUnreadable`] out of
    /// `list()`, and from every other `status(&id)` error.
    ///
    /// [`Installed::OursUnreadable`]: crate::manager::Installed::OursUnreadable
    Unreadable,
    /// Goetia could not determine *whether* anything is installed at that
    /// id: a read the answer depends on failed. Claims no ownership, which
    /// is the whole difference from [`Kind::Unreadable`] — see
    /// [`Error::Undetermined`], which is the only thing that produces it.
    /// Only from `status(&id)`.
    Undetermined,
    /// `support::parse_id` rejected a CLI argument: fix the argument.
    InvalidId,
    /// `get_manager()` or `mgr.list()` failed, so no answer was obtained for
    /// any daemon.
    Unavailable,
    /// `--json` was given to a subcommand that does not implement it.
    Unsupported,
    /// Unreachable today; kept so an unclassified failure has a home rather
    /// than being silently dropped, and never constructed for that reason.
    /// Not a placeholder for a future concept.
    #[allow(dead_code)]
    Other,
}

impl Kind {
    /// The wire spelling. The JSON is the stable contract, not the variant
    /// names, and [`Serialize`](serde::Serialize) goes through here so the
    /// two cannot drift.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::NotInstalled => "not-installed",
            Kind::Foreign => "foreign",
            Kind::Unreadable => "unreadable",
            Kind::Undetermined => "undetermined",
            Kind::InvalidId => "invalid-id",
            Kind::Unavailable => "unavailable",
            Kind::Unsupported => "unsupported",
            Kind::Other => "other",
        }
    }

    /// The exit code this kind alone would produce.
    ///
    /// `invalid-id` is `1` rather than `2` deliberately: `2` is anchored to
    /// "the parser rejected the command line and nothing ran", but
    /// `goetia daemon status good bad` queries and prints `good` before
    /// rejecting `bad`, so `2` would mislead exactly the consumer that
    /// anchor is for. `unsupported` is `2` because there the refusal really
    /// does happen before anything runs.
    fn code(self) -> i32 {
        match self {
            // The question was not answered: the partial-answer case `4`
            // exists for. Whether goetia owns the id (`unreadable`) or
            // could not even find that out (`undetermined`) changes the
            // remedy, not the code.
            Kind::Unreadable | Kind::Undetermined => 4,
            Kind::Unsupported => 2,
            // A determinate answer that the command failed, or
            // (`unavailable`) no answer at all — nothing partial about
            // either.
            Kind::NotInstalled | Kind::Foreign | Kind::Other | Kind::Unavailable | Kind::InvalidId => 1,
        }
    }
}

impl serde::Serialize for Kind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

// Constructors ========================================================================================================

/// The report for a whole-host listing: every decoded entry a daemon, every
/// [`Installed::OursUnreadable`] an `unreadable` error. `list` and `status`
/// with no ids both build their report through here, which is what makes the
/// two emit the identical document for the same machine. Both of
/// [`InstalledIndex`]'s maps are `BTreeMap`s, so the ordering is by id.
///
/// [`Installed::OursUnreadable`]: crate::manager::Installed::OursUnreadable
pub(crate) fn from_index(index: &InstalledIndex) -> Report {
    Report {
        daemons: index
            .ours
            .iter()
            .map(|(id, entry)| DaemonReport {
                id: id.clone(),
                state: state_str(entry.state),
                enabled: entry.enabled,
                pid: entry.pid,
            })
            .collect(),
        errors: index
            .unreadable
            .iter()
            .map(|(name, reason)| ErrorReport {
                id: Some(name.clone()),
                kind: Kind::Unreadable,
                message: format!("installed but unreadable: {reason}"),
            })
            .collect(),
    }
}

/// One daemon, from a successful `ServiceManager::status(&id)`.
pub(crate) fn daemon(id: &str, status: &Status) -> DaemonReport {
    DaemonReport {
        id: id.to_string(),
        state: state_str(status.state),
        enabled: status.enabled,
        pid: status.pid,
    }
}

/// Classify a failure of `ServiceManager::status(&id)` — and of nothing
/// else. `NotInstalled` and `Foreign` are two determinate answers, and
/// `Undetermined` is the backend saying it established neither; every
/// remaining error means goetia owns the id and could not report on it,
/// which is precisely what `list` calls `OursUnreadable`. Routing the
/// remainder to `unreadable` is what makes the two subcommands agree about
/// one machine: a unit that decodes but whose live state cannot be queried
/// reaches `list` as `Installed::OursUnreadable` and `status` as
/// [`Error::CommandFailed`].
///
/// The three named arms are still the operation's own vocabulary, not a
/// classification by error variant: `status(&id)` is the one operation that
/// answers "what is at this id", so its three answers — absent, present and
/// foreign, unestablished — are exactly what its failures can mean.
pub(crate) fn status_error(id: &str, e: &Error) -> ErrorReport {
    let kind = match e {
        Error::NotInstalled { .. } => Kind::NotInstalled,
        Error::Foreign { .. } => Kind::Foreign,
        Error::Undetermined { .. } => Kind::Undetermined,
        _ => Kind::Unreadable,
    };
    ErrorReport {
        id: Some(id.to_string()),
        kind,
        message: e.to_string(),
    }
}

/// A CLI argument `support::parse_id` rejected. Attributable to the id it
/// was given, even though that id never became an [`crate::spec::Id`].
///
/// Carries [`Error::Invalid`]'s `message` field rather than its `Display`,
/// which prefixes ``daemon `X`: ``. The `id` field is the attribution, and
/// the text renderer prefixes the id too, so `Display` would put the same
/// id on one line three times.
pub(crate) fn invalid_id(id: &str, e: &Error) -> ErrorReport {
    let message = match e {
        Error::Invalid { message, .. } => message.clone(),
        other => other.to_string(),
    };
    ErrorReport {
        id: Some(id.to_string()),
        kind: Kind::InvalidId,
        message,
    }
}

/// `get_manager()` or `mgr.list()` failed, so no answer was obtained for any
/// daemon: the whole report is this one error, attributable to no id.
/// Without it, both subcommands would leave stdout empty on this path and
/// `json.loads` would raise on the empty string.
pub(crate) fn unavailable(e: &Error) -> Report {
    Report {
        daemons: Vec::new(),
        errors: vec![ErrorReport {
            id: None,
            kind: Kind::Unavailable,
            message: e.to_string(),
        }],
    }
}

/// `--json` given to a subcommand that does not implement it. `dispatch`
/// emits this *before* matching the subcommand, so nothing ran and no other
/// kind can be present alongside it.
pub(crate) fn unsupported(subcommand: &str) -> Report {
    Report {
        daemons: Vec::new(),
        errors: vec![ErrorReport {
            id: None,
            kind: Kind::Unsupported,
            message: format!(
                "`daemon {subcommand}` does not support --json: drop --json, or use `daemon list` or `daemon status`"
            ),
        }],
    }
}

// Rendering ===========================================================================================================

/// Write `report` as one compact line terminated by a newline, and flush.
///
/// Unlike every other `write!` in the CLI, this one's failure is not
/// best-effort: it is the document [`exit_code`] certifies. See [`emit`].
pub(crate) fn write(report: &Report, out: &mut dyn Write) -> std::io::Result<()> {
    // Infallible: every field is a string, bool, or integer — no map key
    // and no float that could fail to serialize.
    let json = serde_json::to_string(report).expect("a Report serializes infallibly");
    writeln!(out, "{json}")?;
    // A buffered `out` reports a broken pipe or a full disk here rather
    // than at the `writeln!` above; without the flush, "the document was
    // delivered" would mean only "it was accepted into a buffer nobody has
    // checked yet".
    out.flush()
}

/// Write `report` as the `--json` document and return the process exit code
/// for it: [`exit_code`] when the document actually reached `out`, or `1`
/// — with a diagnostic on `err` — when it did not.
///
/// The single funnel every `--json` call site goes through, so none of them
/// can render the document and compute the exit code independently. A
/// consumer told "stdout is exactly one JSON document; parse it, then read
/// `errors`" would otherwise meet an empty stdout beside exit `0` on a
/// broken pipe or a full disk, and `json.loads` would raise on the empty
/// string — the exact failure `--json` exists to remove. The write failure
/// bypasses the precedence computation entirely rather than joining it as
/// another kind: the kinds describe daemons, and there is no document left
/// to carry one.
pub(crate) fn emit(report: &Report, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match write(report, out) {
        Ok(()) => exit_code(report),
        Err(e) => {
            let _ = writeln!(err, "error: failed to write the --json document to stdout: {e}");
            1
        }
    }
}

/// The process exit code for `report`: the precedence-max over its
/// `errors[].kind`, never "1 if non-empty". Called with the same `Report` in
/// both output modes — see the module doc comment.
pub(crate) fn exit_code(report: &Report) -> i32 {
    report
        .errors
        .iter()
        .map(|e| e.kind.code())
        .max_by_key(|code| precedence(*code))
        .unwrap_or(0)
}

/// Where an exit code sits in the design spec's `1 > 4 > 5 > 3 > 0`
/// precedence order. Higher wins. `pub(crate)`, not private: `cli::diff`
/// combines its own per-daemon exit codes by this identical rule, and must
/// not grow a second copy of it.
pub(crate) fn precedence(code: i32) -> u8 {
    match code {
        1 => 4,
        4 => 3,
        5 => 2,
        3 => 1,
        0 => 0,
        // `2` is the only other code [`Kind::code`] produces, and the
        // refusal that yields it happens before any subcommand runs, so it
        // is always alone in its report. Ranking it top anyway means a
        // hypothetical combination could never silently demote a usage
        // error.
        _ => u8::MAX,
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;
