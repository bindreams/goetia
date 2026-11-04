//! Crate-wide error type.

use std::fmt;
use std::path::{Path, PathBuf};

/// What a variant's `#[source]` holds when the cause is another library's error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Every goetia error. A variant is named for the layer that raised it, and its `Display` is its
/// own context only, never its source's text: print the whole chain with [`Error::report`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An OS call (filesystem, registry, Win32, a syscall) failed. `what` is the action
    /// (`"read"`, `"enable `x`"`), and `path` is what it acted on, where there is one.
    #[error("failed to {what}{}", path_suffix(path))]
    Os {
        what: String,
        path: Option<PathBuf>,
        #[source]
        source: BoxError,
    },

    /// A tool goetia runs (`systemctl`, `launchctl`, `plutil`, `sc.exe`) ran and exited
    /// unsuccessfully. `stderr` is its own diagnostic, trimmed.
    #[error("`{command}` failed ({status}){}", stderr_suffix(stderr))]
    Command {
        command: String,
        status: std::process::ExitStatus,
        stderr: String,
    },

    /// A tool goetia runs could not be run, or its run was lost. `source` is the spawn or wait
    /// error.
    #[error("failed to run `{command}`")]
    Spawn {
        command: String,
        #[source]
        source: BoxError,
    },

    /// A goetia operation failed because a step under it did.
    #[error("{what}")]
    Step {
        what: String,
        #[source]
        source: Box<Error>,
    },

    /// A step failed, and so did the one after it. `report()` prints both, `first` then `then`;
    /// `source()` is `first`.
    #[error("a step failed, and so did the one after it")]
    Both {
        #[source]
        first: Box<Error>,
        then: Box<Error>,
    },

    /// What goetia read at `id` changed before it acted on it, so it did nothing further: re-run.
    #[error(
        "`{id}` changed after goetia read it{}, so goetia did nothing further: re-run the command",
        read_at(path)
    )]
    ConcurrentChange { id: String, path: Option<PathBuf> },

    /// goetia's quarantined original of `id` could not go back, because another file holds the
    /// unit path. Resolve by hand.
    #[error(
        "the original fragment of `{id}` is quarantined at {quarantine}, but a different file occupies {occupied}, so \
         it cannot go back: resolve manually",
        quarantine = quarantine.display(),
        occupied = occupied.display()
    )]
    QuarantineStranded {
        id: String,
        quarantine: PathBuf,
        occupied: PathBuf,
    },

    /// A daemon's definition exists in two places at once, which no goetia operation produces.
    #[error(
        "daemon `{id}` has a plist in both {} and {}; remove one by hand (they should never both exist) before retrying",
        paths[0].display(),
        paths[1].display()
    )]
    AmbiguousInstall { id: String, paths: [PathBuf; 2] },

    /// The manager accepted a start, and goetia established that no process runs, before the
    /// deadline. Determinate, unlike [`WaitTimeout`](Error::WaitTimeout).
    #[error("`{id}` did not start: the manager accepted the start, but no process is running")]
    NotStarted { id: String },

    /// launchd cannot update a loaded job, so the changed definition was not installed.
    #[error(
        "`{id}` is loaded in launchd, which cannot update a loaded job, so the changed plist was not installed and \
         the installed plist is untouched: run `goetia daemon stop {id}` (a job that has exited is still loaded), \
         re-run install, then start it"
    )]
    LoadedJob { id: String },

    /// A real account needs a password that was not given.
    #[error(
        "service account `{account}` needs a password (it is not LocalSystem/LocalService/NetworkService, nor an \
         `NT SERVICE\\`/`NT AUTHORITY\\` account); set GOETIA_SERVICE_PASSWORD before installing"
    )]
    PasswordRequired { account: String },

    /// goetia cannot confirm the manager or its client is at least its floor; nothing was sent.
    /// The sentences are written from what each source reported.
    #[error("{}", unsupported_manager(client, manager))]
    UnsupportedManager {
        client: VersionReading,
        manager: VersionReading,
    },

    /// The manager's client exited `0` but announced no job, so the manager did nothing.
    #[error(
        "`{request}` exited 0 without enqueuing a job, so the manager did not act on it{}",
        why(diagnostic)
    )]
    NotEnqueued {
        request: String,
        diagnostic: Option<String>,
    },

    /// The manager answered without properties goetia asked for.
    #[error("`{command}` answered without {}, so goetia cannot tell its live state", missing.join(", "))]
    IncompleteAnswer { command: String, missing: Vec<String> },

    /// The manager does not know a unit goetia finds installed.
    #[error(
        "systemd has not loaded the unit file goetia finds installed for `{unit}` (`LoadState=not-found`), so what it \
         reports is not that unit's state"
    )]
    UnitNotLoaded { unit: String },

    /// A thread or temp file a request needs could not be made, so nothing was sent.
    #[error(
        "{}no thread or temp file could be made for a request, so none was sent",
        id_prefix(id)
    )]
    Reservation {
        id: Option<String>,
        #[source]
        source: std::io::Error,
    },

    /// A path goetia needs as a directory exists and is not one. No OS call failed.
    #[error(
        "{} already exists and is not a directory; goetia cannot use it as a daemon's cwd or as its log directory",
        path.display()
    )]
    NotADirectory { path: PathBuf },

    /// A `.env` file beside the manifest could not be parsed. Distinct from
    /// [`Os`](Error::Os), which means the file could not be read at all.
    #[error("{path}:{line}: {message}", path = path.display())]
    EnvFile {
        path: PathBuf,
        line: usize,
        message: String,
    },

    /// A `${VAR}` reference in the manifest could not be resolved, or a `$`
    /// appeared where the interpolation grammar forbids one.
    #[error("{path}: {message}")]
    Interpolate { path: String, message: String },

    /// The document is not valid YAML, or fails a shape-level constraint
    /// checked during deserialization: a missing or duplicate `id`, an
    /// unknown field such as a leftover `daemons:`, or a malformed `user`
    /// field. `yaml_serde` attaches a line/column to the message.
    #[error(transparent)]
    Yaml(#[from] yaml_serde::Error),

    /// A `resolve()`-time validation failure: an invalid id, a control
    /// character in a user-supplied string, an `=` in an env key, an
    /// empty command, an unrecognized `restart` or `type`, or a malformed
    /// `restart-delay`. Also produced by `blob::decode`, which re-runs
    /// these same checks against a spec deserialized from an untrusted
    /// artifact.
    #[error("daemon `{daemon}`: {message}")]
    Invalid { daemon: String, message: String },

    /// A metadata blob (`blob::decode`) is not usable: malformed base64,
    /// malformed JSON, or a schema this build does not understand.
    /// Distinct from `Invalid`, which a *structurally valid* blob can
    /// still trigger once its envelope decodes cleanly.
    #[error("blob: {0}")]
    Blob(String),

    /// No daemon named `id` is managed by Goetia, per a [`ServiceManager`]
    /// mutating or querying verb (`uninstall`/`start`/`stop`/`enable`/
    /// `disable`/`status`/`show`) that needs one to already exist.
    ///
    /// The strong reading, and the only correct one: **nothing
    /// goetia-attributable is at this id**, not merely that the backend's
    /// primary artifact file is missing. `cli::uninstall` maps this variant
    /// — and only this variant — to exit `0` and "nothing to do", so a
    /// backend that reports it while the platform still applies a leftover
    /// (systemd's fragmentless `<id>.service.d` drop-in, or its
    /// `multi-user.target.wants` enablement link) certifies "confirmed
    /// gone" for a service still loaded, still running and still enrolled at
    /// boot — and contradicts its own `install`, which refuses that same
    /// state as [`Foreign`](Error::Foreign).
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("daemon `{id}` is not installed")]
    NotInstalled { id: String },

    /// Something exists at `id`, but it carries no Goetia marker at all —
    /// distinct from [`NotInstalled`](Error::NotInstalled), which means
    /// nothing is there. Every [`ServiceManager`] verb other than `install`
    /// refuses a foreign id with this, never `NotInstalled`: reporting a
    /// service that demonstrably exists on the machine as merely "not
    /// installed" sends the user looking for a missing service instead of
    /// to the actual remedy in `recovery` (identical wording to
    /// `decide::Outcome::RefuseForeign`'s, via
    /// [`crate::decide::foreign_recovery`]).
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("daemon `{id}` exists but is not managed by goetia: {recovery}")]
    Foreign { id: String, recovery: String },

    /// A read that would have said whether anything is installed at `id`
    /// failed, so goetia does not know. `reason` names the path and the
    /// failure; `recovery` says what would make that read succeed.
    ///
    /// The distinguishing property is what this variant does **not** claim.
    /// [`NotInstalled`](Error::NotInstalled) is proof that nothing
    /// goetia-attributable is at the id; [`Foreign`](Error::Foreign) — and
    /// `cli::report::Kind::Unusable`, whose published meaning is "goetia
    /// owns the id but cannot report on it" — is proof that something is.
    /// A failed read is the absence of both proofs, so reporting either one
    /// invents evidence: "installed but unusable" over an
    /// `/etc/systemd/system/<id>.service.d` an unelevated caller merely
    /// could not open puts goetia's name, and `uninstall`'s recovery
    /// advice, on what may be a stranger's override. Choose between the
    /// three by what was established, never by which is closest to hand.
    ///
    /// Exit `4` (indeterminate), like `Unusable`: the code is about
    /// whether the question was answered, and this one was not.
    #[error("cannot determine whether daemon `{id}` is installed: {reason}. {recovery}")]
    Undetermined {
        id: String,
        reason: String,
        recovery: String,
    },

    /// A wait for `id` to reach `awaited` ran out of budget. Produced only
    /// through [`crate::manager::budget::timed_out`], so all three backends
    /// word one condition identically.
    ///
    /// What this variant does **not** claim, and what separates it from
    /// [`Undetermined`](Error::Undetermined): nothing here is in doubt about
    /// what is *installed* at the id — that question was answered before the
    /// wait ever began. From a backend, the request was issued — the budget
    /// never decides that, and on launchd it may still be on its way in a
    /// `launchctl` goetia left running — and goetia stopped watching for its
    /// outcome; it was not cancelled, and the service may still arrive.
    /// `cli::restart` is the one producer that also reports a step it never
    /// issued: a start leg its budget ran out before, whose message says no
    /// start was issued and the daemon is left stopped. A backend must never
    /// report an expiry as `Ok(())`, and never as `Undetermined`.
    ///
    /// Exit `4` (indeterminate), like `Undetermined`: the code is about
    /// whether the question was answered, and this one was not.
    #[error("`{id}` did not report {awaited} within {waited}: {recovery}", waited = humantime::format_duration(*waited))]
    WaitTimeout {
        id: String,
        /// `"running"` or `"stopped"`, and nothing else.
        awaited: &'static str,
        waited: std::time::Duration,
        recovery: String,
    },

    /// `kill` found no process of `id`'s to end: the service is stopped, or between runs. Nothing
    /// was signalled.
    #[error("daemon `{id}` is not running: there is no process to kill")]
    NotRunning { id: String },

    /// Steps goetia issued whose outcome nobody established. Produced only
    /// by `cli::restart` under a budget that does not wait, on a manager with
    /// no request-only restart (launchd, SCM), where the stop was issued
    /// without confirmation and the start that followed was refused. The
    /// daemon was not restarted; whether it is running the instance the stop
    /// is still taking down, is going down, or never moved, goetia proved
    /// none of the three.
    ///
    /// The refusal itself may be determinate or not — a launchd `bootstrap`
    /// that fails outright, where SCM's `ERROR_SERVICE_ALREADY_RUNNING` says
    /// only that the service is not stopped. Either way the stop was never
    /// confirmed: it may still be in flight, may have completed, or may have
    /// had nothing to do, so what is unestablished is where the daemon ended
    /// up. Not a plain failure, exit `1`: a refusal, even a
    /// determinate one, answers whether the start went out, not that
    /// question. (A plain `start --timeout 0` of the same service is exit
    /// `1`: with no stop before it, the refusal is the whole answer.) Not
    /// [`WaitTimeout`](Error::WaitTimeout): nothing waited.
    /// Not [`Undetermined`](Error::Undetermined): what is installed at the
    /// id was never in doubt.
    ///
    /// Exit `4` (indeterminate), like the two above: the code is about
    /// whether the question was answered, and this one was not.
    #[error("`{id}` was not restarted, and where it ended up is not established: {detail}")]
    Unestablished { id: String, detail: String },

    /// A request goetia lost track of before its outcome was established: the
    /// tool carrying it — `systemctl`, `launchctl` — may have started when it
    /// failed, so whatever it sends may already be sent. `request` is the
    /// tool's command line. `reached` is whether goetia knows the request
    /// reached the manager: systemd says so (`Enqueued anchor job`) once it
    /// has enqueued the job, which may already be running, and a request lost
    /// after that had reached it.
    ///
    /// What it does **not** claim, which separates it from its neighbours: not
    /// that nothing was sent, as a plain failure would; not that goetia waited
    /// for the outcome and gave up, as [`WaitTimeout`](Error::WaitTimeout)
    /// does; and not that what is installed at the id is in doubt, as
    /// [`Undetermined`](Error::Undetermined) does. A failure goetia can place
    /// before the tool ran is never this.
    ///
    /// Exit `4` (indeterminate), like the three above: whether the manager
    /// acted is the question, and it was not answered.
    #[error("{}", in_doubt(request, *reached, detail))]
    RequestInDoubt {
        request: String,
        reached: bool,
        detail: String,
    },

    /// `install` rewrote the plist of a daemon whose job launchd now has loaded, though it was not
    /// loaded when `install` checked: someone started it in between. Which definition the loaded job
    /// read, the old plist or the new, is not established, so the job may run a spec other than the
    /// installed one. launchd only (it cannot update a loaded job).
    ///
    /// `install` then puts the plist back as it was (the previous one, or none after a create), so a
    /// rerun is refused while the job is loaded. `restore_failed` is the failure of that put-back,
    /// when it failed, and says what is on disk.
    ///
    /// Exit `4` (indeterminate).
    #[error("{}", loaded_in_doubt(id, None, restore_failed.is_some()))]
    LoadedJobInDoubt {
        id: String,
        #[source]
        restore_failed: Option<Box<Error>>,
    },

    /// `install` wrote the plist of a daemon and could not read whether launchd has its job loaded
    /// afterwards, so which definition a job loaded meanwhile read is not established either.
    /// `detail` is the failed read; `restore_failed` is as in
    /// [`LoadedJobInDoubt`](Error::LoadedJobInDoubt). launchd only.
    ///
    /// Exit `4` (indeterminate).
    #[error("{}", loaded_in_doubt(id, Some(detail), restore_failed.is_some()))]
    LoadedStateInDoubt {
        id: String,
        detail: String,
        #[source]
        restore_failed: Option<Box<Error>>,
    },

    /// `install` booted out a job that answered to the label, then found `found` at `path`, where it
    /// was about to create the plist: a concurrent writer took the place. The `bootout` was sent, so
    /// the job is gone and the daemon is not installed. launchd only.
    ///
    /// Exit `4` (indeterminate).
    #[error(
        "`{id}`: a job loaded under that label was booted out, but {found} appeared at {path} before the plist \
         could be created, so the daemon is not installed: re-run install, which decides against what is there",
        path = path.display()
    )]
    StaleJobBootedOut { id: String, path: PathBuf, found: String },

    /// goetia will ask no service manager here, on positive evidence: systemd's offline mode, a
    /// chroot, or a system systemd did not boot. `evidence` says which. goetia refuses there
    /// rather than act on unit files alone, or read a `systemctl` that asked nobody as an answer.
    ///
    /// Exit `1`, from every verb that reaches the manager, `status` and `list` included: a refusal,
    /// not an answer about the daemon — per id where the verb takes ids, and for the whole listing
    /// from `list`. Raised before anything is written or sent, and by any `systemctl` that says it
    /// ignored what it was asked, which did nothing.
    #[error(
        "goetia does not manage systemd here ({evidence}): it works only through the running systemd manager of the \
         system it runs on, never offline or from a chroot"
    )]
    NoManager { evidence: String },

    /// A mutating CLI subcommand was invoked without the elevation
    /// (root/Administrator) it requires. Never returned for `list`,
    /// `status`, `show`, `diff`, or `install --dry-run`, none of which
    /// mutate anything.
    #[error("`{subcommand}` requires elevation (root/Administrator): re-run as root or Administrator")]
    ElevationRequired { subcommand: String },

    /// [`crate::manager::native`] has no [`ServiceManager`] implementation
    /// for the running platform — anything but Linux, macOS or Windows. A
    /// message here, never a panic: a CLI user hitting this gets a
    /// diagnosable error instead of a crash.
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("no backend for {platform} yet")]
    UnsupportedPlatform { platform: String },

    /// Resolving a [`crate::spec::User`] to a real platform account failed:
    /// no such user/uid, or the lookup itself errored.
    #[error("account lookup failed: {detail}")]
    AccountLookup { detail: String },

    /// A backend tried to create or move an artifact to `path`, but
    /// something already occupies it — a state none of that backend's own
    /// operations should be able to produce, so this always carries a
    /// "this should never happen" flavor rather than being a normal,
    /// expected outcome.
    #[error("{path} already exists (this should never happen)", path = path.display())]
    AlreadyExists { path: PathBuf },
}

impl Error {
    /// Whether this failure left the outcome open: what is at the id, whether a request reached the
    /// manager, or where the daemon ended up. `false` means goetia established that the operation
    /// failed or was refused. The CLI exits `4` for the first and `1` for the second.
    ///
    /// Exhaustive on purpose: a new variant must be classified here before it compiles.
    pub fn is_indeterminate(&self) -> bool {
        match self {
            Error::Undetermined { .. }
            | Error::WaitTimeout { .. }
            | Error::Unestablished { .. }
            | Error::RequestInDoubt { .. }
            | Error::LoadedJobInDoubt { .. }
            | Error::LoadedStateInDoubt { .. }
            | Error::StaleJobBootedOut { .. } => true,
            Error::Step { source, .. } => source.is_indeterminate(),
            Error::Both { first, then } => first.is_indeterminate() && then.is_indeterminate(),
            Error::Os { .. }
            | Error::Command { .. }
            | Error::Spawn { .. }
            | Error::ConcurrentChange { .. }
            | Error::QuarantineStranded { .. }
            | Error::AmbiguousInstall { .. }
            | Error::NotStarted { .. }
            | Error::LoadedJob { .. }
            | Error::PasswordRequired { .. }
            | Error::UnsupportedManager { .. }
            | Error::NotEnqueued { .. }
            | Error::IncompleteAnswer { .. }
            | Error::UnitNotLoaded { .. }
            | Error::Reservation { .. }
            | Error::NotADirectory { .. }
            | Error::EnvFile { .. }
            | Error::Interpolate { .. }
            | Error::Yaml(_)
            | Error::Invalid { .. }
            | Error::Blob(_)
            | Error::NotInstalled { .. }
            | Error::Foreign { .. }
            | Error::NotRunning { .. }
            | Error::NoManager { .. }
            | Error::ElevationRequired { .. }
            | Error::UnsupportedPlatform { .. }
            | Error::AccountLookup { .. }
            | Error::AlreadyExists { .. } => false,
        }
    }

    /// The error a [`Step`](Error::Step) chain ends in, which classification reads: a step over
    /// [`NotInstalled`](Error::NotInstalled) is still "not installed". Not unwrapped through
    /// [`Both`](Error::Both), which has two causes.
    pub fn cause(&self) -> &Error {
        match self {
            Error::Step { source, .. } => source.cause(),
            e => e,
        }
    }

    /// The error and its causes, joined as `context: cause: cause`; a [`Both`](Error::Both) prints
    /// as `first; then: then`. What the CLI prints and `--json` carries: [`Display`](fmt::Display)
    /// alone is the outermost context.
    pub fn report(&self) -> impl fmt::Display + '_ {
        Report(self)
    }

    /// An OS call failed with no path to name.
    pub(crate) fn os(what: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Error::Os {
            what: what.into(),
            path: None,
            source: source.into(),
        }
    }

    /// `command` exited unsuccessfully with `status`, having written `stderr`.
    pub(crate) fn command(command: impl Into<String>, status: std::process::ExitStatus, stderr: &[u8]) -> Self {
        Error::Command {
            command: command.into(),
            status,
            stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        }
    }

    /// `command` could not be run, or its run was lost.
    pub(crate) fn spawn(command: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Error::Spawn {
            command: command.into(),
            source: source.into(),
        }
    }

    /// An OS call failed on `path`.
    pub(crate) fn os_at(what: impl Into<String>, path: impl Into<PathBuf>, source: impl Into<BoxError>) -> Self {
        Error::Os {
            what: what.into(),
            path: Some(path.into()),
            source: source.into(),
        }
    }
}

/// [`Error::report`]'s value.
struct Report<'a>(&'a Error);

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_chain(self.0, f)
    }
}

fn write_chain(e: &(dyn std::error::Error + 'static), f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // A `#[source]` of type `Box<Error>` reaches here as the box.
    let ours = e
        .downcast_ref::<Error>()
        .or_else(|| e.downcast_ref::<Box<Error>>().map(|b| &**b));
    if let Some(Error::Both { first, then }) = ours {
        write_chain(first.as_ref(), f)?;
        f.write_str("; then: ")?;
        return write_chain(then.as_ref(), f);
    }
    write!(f, "{e}")?;
    if let Some(source) = e.source() {
        f.write_str(": ")?;
        write_chain(source, f)?;
    }
    Ok(())
}

/// What a version source reported, kept as the source gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionReading {
    /// The major version it reported.
    Major(u32),
    /// The text it reported, which has no version goetia can read.
    Unreadable(String),
}

/// [`Error::UnsupportedManager`]'s message: one sentence for each of the manager and its client
/// that is below the floor or unreadable.
fn unsupported_manager(client: &VersionReading, manager: &VersionReading) -> String {
    use crate::backend::systemd::{SYSTEMD_FLOOR, TYPE_EXEC};
    const ANSWER: &str = "cannot answer the `systemctl --show-transaction` every start and stop runs";
    const TYPE_SIMPLE: &str = "runs goetia's `Type=exec` units as `Type=simple`, which cannot report a failed exec";
    let manager = match manager {
        VersionReading::Major(n) if *n >= SYSTEMD_FLOOR => None,
        VersionReading::Major(n) if *n >= TYPE_EXEC => Some(format!("The running systemd is {n}, which {ANSWER}.")),
        VersionReading::Major(n) => Some(format!(
            "The running systemd is {n}, which {ANSWER}, and {TYPE_SIMPLE}."
        )),
        VersionReading::Unreadable(text) => Some(format!(
            "goetia cannot read the version the running systemd reports ({text:?}), so cannot tell whether it is."
        )),
    };
    let client = match client {
        VersionReading::Major(n) if *n >= SYSTEMD_FLOOR => None,
        VersionReading::Major(n) => Some(format!(
            "The `systemctl` client is {n}, which does not accept the `--show-transaction` option every start and \
             stop passes it."
        )),
        VersionReading::Unreadable(text) => Some(format!(
            "goetia cannot read the version `systemctl --version` reports ({text:?}), so cannot tell whether it is."
        )),
    };
    let sentences: Vec<String> = [manager, client].into_iter().flatten().collect();
    format!(
        "goetia requires systemd {SYSTEMD_FLOOR} or newer. {}",
        sentences.join(" ")
    )
}

/// ` /path`, or nothing.
fn path_suffix(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map(Path::display)
        .map(|p| format!(" {p}"))
        .unwrap_or_default()
}

/// ` at /path`, or nothing.
fn read_at(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map(Path::display)
        .map(|p| format!(" at {p}"))
        .unwrap_or_default()
}

/// `: stderr`, or nothing when the tool said nothing.
fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// The reason a request announced no job: the client's diagnostic, or that it gave none.
fn why(diagnostic: &Option<String>) -> String {
    match diagnostic {
        Some(diagnostic) => format!(": {diagnostic}"),
        None => " and said nothing about why".to_string(),
    }
}

/// `` `id`: ``, or nothing.
fn id_prefix(id: &Option<String>) -> String {
    id.as_deref().map(|id| format!("`{id}`: ")).unwrap_or_default()
}

/// [`Error::RequestInDoubt`]'s message: what goetia knows of how far the
/// request got, and no more.
fn in_doubt(request: &str, reached: bool, detail: &str) -> String {
    if reached {
        format!(
            "`{request}` reached the service manager, but goetia lost track of it before its outcome was confirmed: \
             {detail}"
        )
    } else {
        format!(
            "`{request}` may have started before it failed, so it may or may not have reached the service manager: \
             {detail}"
        )
    }
}

/// The message of both in-doubt variants. `read_failed` is the failed read (second variant only);
/// `restore_failed` is whether the put-back of the plist failed, whose error is the variant's source.
fn loaded_in_doubt(id: &str, read_failed: Option<&str>, restore_failed: bool) -> String {
    let cause = match read_failed {
        None => format!("`{id}` was loaded in launchd while its plist was written"),
        Some(detail) => {
            format!("`{id}`'s plist was written, but whether launchd has its job loaded could not be read ({detail})")
        }
    };
    let remedy = if restore_failed {
        format!(
            "Check what is on disk, then run `goetia daemon stop {id}`, re-run install, then `goetia daemon start {id}`. The plist could not be put back as it was before this install"
        )
    } else {
        format!(
            "The plist is back as it was before this install, so the change is not installed: run `goetia daemon stop {id}`, re-run install, then `goetia daemon start {id}`"
        )
    };
    format!("{cause}, so goetia cannot tell whether a loaded job holds the old definition or the new one. {remedy}")
}

/// This crate's `Result` alias, used throughout [`crate::manager`] and
/// [`crate::cli`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
