//! The `--json` wire format shared by `daemon list` and `daemon status`.
//!
//! One compact JSON object, newline-terminated, on stdout:
//!
//! ```json
//! {"daemons":[{"id":"frpc","state":"running","enabled":true,"pid":1234}],"errors":[],"undetermined":[]}
//! ```
//!
//! `daemons`, `errors` and `undetermined` are always present, as arrays,
//! possibly empty. A `daemons` entry carries no `name`:
//! [`crate::manager::ServiceManager::status`] returns a [`Status`] with no
//! spec, so only `list` could supply it, and the two subcommands must not
//! differ in fields. `pid` is an integer or `null`; `null` means the manager
//! reports no main process, never "this command could not find out" (that is
//! an `errors` entry).
//!
//! `undetermined` carries what `list()` could not classify at all; an entry
//! whose `name` is `null` stands for more than one id. It is a third key
//! rather than an `errors[].kind` because the command did not fail: it is a
//! stated limit on the answer's completeness, readable without parsing `kind`
//! strings. `status <id>`, asked about one id, reports the same fact as that
//! id's own `errors[].kind: "undetermined"`.
//!
//! [`exit_code`] is the only exit-code computation `list` and `status` have,
//! with or without `--json`: both build a [`Report`] first and then pick a
//! renderer, so the exit code cannot depend on the output format.

use std::fmt::{self, Write as _};
use std::io::Write;

use super::ExitCode;
use super::support::{InstalledIndex, print_line, state_str};
use crate::error::{Error, Reason, Sink};
use crate::manager::Status;

// Wire format =========================================================================================================

#[derive(serde::Serialize)]
pub(crate) struct Report {
    pub daemons: Vec<DaemonReport>,
    pub errors: Vec<ErrorReport>,
    pub undetermined: Vec<UndeterminedReport>,
}

/// One thing `list()` could not classify. A `Vec` and not a map: an
/// aggregate entry has no name to key on, and nothing limits a backend to
/// one aggregate.
#[derive(serde::Serialize)]
pub(crate) struct UndeterminedReport {
    /// `null` for an entry standing for more than one id — while one is
    /// present, no negative conclusion about any id is sound (see
    /// [`Installed::Undetermined`](crate::manager::Installed::Undetermined)).
    pub name: Option<String>,
    pub reason: String,
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
    /// `None` when it is not: `unsupported`, and an `unavailable` from
    /// obtaining the manager or from listing.
    pub id: Option<String>,
    pub kind: Kind,
    /// Exactly as it is, for `--json`: serde encodes it, so escaping first would encode it twice.
    pub message: String,
    /// What `message` was made from, which a person reads written as the report writes it; not in
    /// the document.
    #[serde(skip)]
    origin: Origin,
}

/// What an [`ErrorReport`]'s `message` was made from.
enum Origin {
    /// goetia's own words.
    Words,
    /// The error failed on, which [`Error::report`] writes.
    Error(Error),
    /// A [`Reason`], which `Display` writes as the report does.
    Reason(Reason),
}

impl ErrorReport {
    /// A report of goetia's own words, which a person reads with the control characters escaped.
    pub(crate) fn said(id: Option<String>, kind: Kind, message: String) -> Self {
        ErrorReport {
            id,
            kind,
            message,
            origin: Origin::Words,
        }
    }

    /// A report of `e`: its unescaped chain for the document, its report for a person.
    fn failed(id: Option<String>, kind: Kind, e: Error) -> Self {
        let message = e.report_unescaped().to_string();
        ErrorReport {
            id,
            kind,
            message,
            origin: Origin::Error(e),
        }
    }

    /// Writes `message` for a person, as the report writes it (see [`Error::report`]).
    fn write_shown(&self, out: &mut Sink) -> fmt::Result {
        match &self.origin {
            Origin::Words => out.write_str(&self.message),
            Origin::Error(e) => e.write_report(out),
            Origin::Reason(reason) => reason.write_to(out),
        }
    }
}

/// Every value [`ErrorReport::kind`] can take.
///
/// A `kind` is chosen **at the call site, from which operation failed**,
/// never by matching on [`Error`]'s variant alone: [`Error::Invalid`] comes
/// from both `support::parse_id` and blob-content validation, so the variant
/// does not determine the remedy. The constructors below are named for the
/// operation whose failure they classify.
///
/// An enum so that [`Kind::code`] is an exhaustive `match`: a kind added
/// without a code stops compiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Nothing is installed at that id. Only from `status(&id)`.
    NotInstalled,
    /// Something exists there and demonstrably carries no goetia marker —
    /// established by a read that completed, never inferred from one that
    /// failed, which is [`Kind::Undetermined`]'s case. Only from
    /// `status(&id)`.
    Foreign,
    /// Goetia owns the id but cannot report on it; `message` says why. The causes are listed on
    /// [`Ownership::OursUnusable`]. From [`Installed::OursUnusable`] out of `list()`, and from
    /// every other `status(&id)` error.
    ///
    /// [`Ownership::OursUnusable`]: crate::decide::Ownership::OursUnusable
    ///
    /// [`Installed::OursUnusable`]: crate::manager::Installed::OursUnusable
    Unusable,
    /// Goetia could not determine *whether* anything is installed at that
    /// id: a read the answer depends on failed. Claims no ownership, unlike
    /// [`Kind::Unusable`]. Only [`Error::Undetermined`] produces an `errors[]`
    /// entry of this kind; the `undetermined[]` key is fed separately, from
    /// [`Installed::Undetermined`] out of `list()`. Both exit `4` through
    /// [`Kind::code`].
    ///
    /// [`Installed::Undetermined`]: crate::manager::Installed::Undetermined
    /// Only from `status(&id)`.
    Undetermined,
    /// `support::parse_id` rejected a CLI argument: fix the argument.
    InvalidId,
    /// `get_manager()` or `mgr.list()` failed, so no answer was obtained for
    /// any daemon — or, under one id, `status(&id)` found no manager to ask
    /// about it ([`Error::NoManager`]).
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
            Kind::Unusable => "unusable",
            Kind::Undetermined => "undetermined",
            Kind::InvalidId => "invalid-id",
            Kind::Unavailable => "unavailable",
            Kind::Unsupported => "unsupported",
            Kind::Other => "other",
        }
    }

    /// The exit code this kind alone would produce.
    ///
    /// `invalid-id` is `1` rather than `2`: `2` means "the parser rejected the
    /// command line and nothing ran", but `goetia daemon status good bad`
    /// prints `good` before rejecting `bad`. `unsupported` is `2` because there
    /// the refusal does happen before anything runs.
    fn code(self) -> ExitCode {
        match self {
            // The partial-answer case `4` exists for. Whether goetia owns the
            // id (`unusable`) or could not find that out (`undetermined`)
            // changes the remedy, not the code.
            Kind::Unusable | Kind::Undetermined => ExitCode::Indeterminate,
            Kind::Unsupported => ExitCode::Usage,
            // A determinate failure, or (`unavailable`) no answer at all;
            // nothing partial about either.
            Kind::NotInstalled | Kind::Foreign | Kind::Other | Kind::Unavailable | Kind::InvalidId => ExitCode::Error,
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
/// [`Installed::OursUnusable`] an `unusable` error, and everything
/// goetia could not classify an entry in the third key. `list` and `status`
/// with no ids both build their report here, so they emit the identical
/// document for the same machine. [`InstalledIndex`]'s maps are `BTreeMap`s,
/// so the ordering is by id.
///
/// [`Installed::OursUnusable`]: crate::manager::Installed::OursUnusable
pub(crate) fn from_index(index: &InstalledIndex) -> Report {
    Report {
        daemons: index.ours.iter().map(|(id, (_, status))| daemon(id, status)).collect(),
        errors: index
            .unusable
            .iter()
            .map(|(name, reason)| {
                let reason = reason.installed_but_unusable();
                let message = reason.unescaped().to_string();
                ErrorReport {
                    id: Some(name.clone()),
                    kind: Kind::Unusable,
                    message,
                    origin: Origin::Reason(reason),
                }
            })
            .collect(),
        // Already ordered: `partition_installed` sorts as it builds, so the
        // document and the text renderer cannot disagree.
        undetermined: index
            .undetermined
            .iter()
            .map(|entry| UndeterminedReport {
                name: entry.name.clone(),
                reason: entry.reason.unescaped().to_string(),
            })
            .collect(),
    }
}

/// One daemon, from a successful `ServiceManager::status(&id)`.
pub(crate) fn daemon(id: &str, status: &Status) -> DaemonReport {
    DaemonReport {
        id: id.to_string(),
        state: state_str(status.common().state),
        enabled: status.common().enabled,
        pid: status.common().pid,
    }
}

/// Classify a failure of `ServiceManager::status(&id)` — and of nothing
/// else. `NotInstalled` and `Foreign` are determinate answers, and
/// `Undetermined` is the backend establishing neither; every remaining error
/// means goetia owns the id and could not report on it, which `list` calls
/// `OursUnusable`. Routing the remainder to `unusable` makes the two
/// subcommands agree about one machine: a unit that decodes but whose live
/// state cannot be queried reaches `list` as `Installed::OursUnusable` and
/// `status` as a failure of its own, an [`Error::Command`] or [`Error::Os`],
/// say.
///
/// The three named arms are the operation's own vocabulary (absent, foreign,
/// unestablished), not a classification by error variant.
///
/// [`Error::NoManager`] says nothing about the id: no manager was available to
/// ask. It is `unavailable` under the id, exit `1`, per id like every verb
/// given ids.
pub(crate) fn status_error(id: &str, e: Error) -> ErrorReport {
    let kind = match e.cause() {
        Error::NoManager { .. } => Kind::Unavailable,
        Error::NotInstalled { .. } => Kind::NotInstalled,
        Error::Foreign { .. } => Kind::Foreign,
        Error::Undetermined { .. } => Kind::Undetermined,
        _ => Kind::Unusable,
    };
    ErrorReport::failed(Some(id.to_string()), kind, e)
}

/// A CLI argument `support::parse_id` rejected. Attributable to the id it
/// was given, even though that id never became an [`crate::spec::Id`].
///
/// Carries [`Error::Invalid`]'s `message` field rather than its `Display`,
/// which prefixes ``daemon `X`: ``: the `id` field and the text renderer both
/// carry the id already.
pub(crate) fn invalid_id(id: &str, e: Error) -> ErrorReport {
    let id = Some(id.to_string());
    match e {
        Error::Invalid { message, .. } => ErrorReport::said(id, Kind::InvalidId, message),
        other => ErrorReport::failed(id, Kind::InvalidId, other),
    }
}

/// `get_manager()` or `mgr.list()` failed, so no answer was obtained for any
/// daemon: the whole report is this one error, attributable to no id.
/// Without it, stdout would be empty on this path.
pub(crate) fn unavailable(e: Error) -> Report {
    Report {
        daemons: Vec::new(),
        errors: vec![ErrorReport::failed(None, Kind::Unavailable, e)],
        undetermined: Vec::new(),
    }
}

/// `--json` given to a subcommand that does not implement it. `dispatch`
/// emits this *before* matching the subcommand, so nothing ran and no other
/// kind can be present alongside it.
pub(crate) fn unsupported(subcommand: &str) -> Report {
    Report {
        daemons: Vec::new(),
        errors: vec![ErrorReport::said(
            None,
            Kind::Unsupported,
            format!(
                "`daemon {subcommand}` does not support --json: drop --json, or use `daemon list` or `daemon status`"
            ),
        )],
        undetermined: Vec::new(),
    }
}

// Rendering ===========================================================================================================

/// Writes the entries of `report.errors` as `error:` lines, each under its id when it has one.
pub(crate) fn print_errors(report: &Report, err: &mut dyn Write) {
    for error in &report.errors {
        print_line(err, Some("error"), error.id.as_deref(), |sink| error.write_shown(sink));
    }
}

/// Write `report` as one compact line terminated by a newline, and flush.
///
/// Unlike every other `write!` in the CLI, this one's failure is not
/// best-effort: it is the document [`exit_code`] certifies. See [`emit`].
pub(crate) fn write(report: &Report, out: &mut dyn Write) -> std::io::Result<()> {
    // Infallible: every field is a string, bool, or integer — no map key
    // and no float that could fail to serialize.
    let json = serde_json::to_string(report).expect("a Report serializes infallibly");
    writeln!(out, "{json}")?;
    // A buffered `out` reports a broken pipe or a full disk here rather than
    // at the `writeln!` above.
    out.flush()
}

/// Write `report` as the `--json` document and return the process exit code
/// for it: [`exit_code`] when the document actually reached `out`, or
/// [`ExitCode::Error`] — with a diagnostic on `err` — when it did not.
///
/// Every `--json` call site goes through here, so none can render the
/// document and compute the exit code independently: otherwise a broken pipe
/// or full disk would leave an empty stdout beside exit `0`. The write failure
/// is not a kind: kinds describe daemons, and there is no document left to
/// carry one.
pub(crate) fn emit(report: &Report, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match write(report, out) {
        Ok(()) => exit_code(report),
        Err(e) => {
            print_line(err, Some("error"), None, |sink| {
                write!(sink, "failed to write the --json document to stdout: {e}")
            });
            ExitCode::Error
        }
    }
}

/// The process exit code for `report`: the greatest [`ExitCode`] over its
/// `errors[].kind` *and* its `undetermined` entries, never "1 if non-empty".
/// Called with the same `Report` in both output modes — see the module doc
/// comment.
pub(crate) fn exit_code(report: &Report) -> ExitCode {
    report
        .errors
        .iter()
        .map(|e| e.kind.code())
        // From `Kind::code`, never a literal: a second assignment site would
        // retire the exhaustive-match guarantee (see `Kind`).
        .chain(report.undetermined.iter().map(|_| Kind::Undetermined.code()))
        .max()
        .unwrap_or(ExitCode::Success)
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;
