//! Crate-wide error type.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod escape;
mod reason;
mod report;
mod within;

#[cfg(feature = "cli")]
pub(crate) use escape::{EscapedStream, escape};
pub use reason::Reason;
pub(crate) use report::Chain;
#[cfg(feature = "cli")]
pub(crate) use report::Sink;
pub(crate) use within::within;

use report::Report;

/// What a variant's `#[source]` holds when the cause is another library's error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A foreign source shared rather than owned, so the `Blob` and `Invalid` errors that hold one stay
/// cheap to share inside the `Arc<Error>` a snapshot keeps.
pub type SharedError = Arc<dyn std::error::Error + Send + Sync>;

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
    /// unsuccessfully. `stderr` is its own diagnostic, trimmed: [`report`](Error::report) writes it,
    /// and this variant's `Display` does not, so it holds no layout.
    #[error("`{command}` failed ({status})")]
    Command {
        command: String,
        status: std::process::ExitStatus,
        stderr: String,
    },

    /// A tool goetia runs could not be run, or its run was lost.
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

    /// An error a snapshot kept (its blob's failure), handed out again as it was: its text, source,
    /// remedy and indeterminacy are the kept error's. Neither [`cause`](Error::cause) nor
    /// [`map_cause`](Error::map_cause) goes into it.
    #[error(transparent)]
    Kept(Arc<Error>),

    /// A step failed, and so did the one after it. `report()` prints both, `first` then `then`.
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
    /// deadline. Determinate, unlike [`WaitTimeout`](Error::WaitTimeout). `keep_alive` is whether
    /// the daemon is `restart: always`, for which a missing process is abnormal.
    #[error(
        "`{id}` did not start: the manager accepted the start, but no process is running{}",
        keep_alive_note(*keep_alive)
    )]
    NotStarted { id: String, keep_alive: bool },

    /// The manager accepted a start, but what it reported afterwards, before the deadline,
    /// establishes neither that a process runs nor that none does (launchd's `spawn scheduled`, or
    /// a read that failed). Indeterminate, unlike [`NotStarted`](Error::NotStarted).
    #[error("`{id}` may not have started: the manager accepted the start, but its state is not established")]
    StartUnconfirmed { id: String },

    /// launchd cannot update a loaded job, so the changed definition was not installed.
    #[error(
        "`{id}` is loaded in launchd, which cannot update a loaded job, so the changed plist was not installed and \
         the installed plist is untouched: run `goetia daemon stop {id}` (a job that has exited is still loaded), \
         re-run install, then start it"
    )]
    LoadedJob { id: String },

    /// `install`'s put-back of a plist found `found` at `path` instead of the plist `install` wrote,
    /// so it left the path alone. launchd only; it is the `restore_failed` of the in-doubt variants.
    #[error("{} {}", path.display(), put_back_found(*found))]
    PutBackBlocked { path: PathBuf, found: PutBackFound },

    /// An environment variable holds a value goetia cannot read as text, and no OS call failed. The
    /// value is never kept, because it is a password.
    #[error("`{variable}` is set but its value is not valid Unicode")]
    InvalidPassword { variable: String },

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
    /// unknown field, or a malformed `user` field. `yaml_serde` attaches a
    /// line/column to the message.
    #[error(transparent)]
    Yaml(#[from] yaml_serde::Error),

    /// A `resolve()`-time validation failure: an invalid id, a control
    /// character in a user-supplied string, an `=` in an env key, an
    /// empty command, an unrecognized `restart` or `type`, or a malformed
    /// `restart-delay`. Also produced by `blob::decode`, which re-runs
    /// these same checks against a spec deserialized from an untrusted
    /// artifact.
    ///
    /// `source` is the parser's own error when one failed (`humantime`, an
    /// absolute-path resolution), printed by [`report`](Error::report).
    #[error("daemon `{daemon}`: {message}")]
    Invalid {
        daemon: String,
        message: String,
        #[source]
        source: Option<SharedError>,
    },

    /// A metadata blob (`blob::decode`) is not usable: malformed base64,
    /// malformed JSON, or a schema this build does not understand.
    /// Distinct from `Invalid`, which a *structurally valid* blob can
    /// still trigger once its envelope decodes cleanly.
    ///
    /// `source` is the decoder's own error when one failed.
    #[error("blob: {message}")]
    Blob {
        message: String,
        #[source]
        source: Option<SharedError>,
    },

    /// No daemon named `id` is managed by Goetia, per a [`ServiceManager`]
    /// verb (`uninstall`/`start`/`stop`/`enable`/`disable`/`status`/`show`)
    /// that needs one to already exist.
    ///
    /// The strong reading: **nothing goetia-attributable is at this id**, not
    /// merely that the backend's primary artifact file is missing.
    /// `cli::uninstall` maps only this variant to exit `0` and "nothing to
    /// do", so a backend that reports it while the platform still applies a
    /// leftover (systemd's fragmentless `<id>.service.d` drop-in, or its
    /// `multi-user.target.wants` enablement link) certifies "confirmed gone"
    /// for a service still loaded, running and enrolled at boot, and
    /// contradicts its own `install`, which refuses that state as
    /// [`Foreign`](Error::Foreign).
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("daemon `{id}` is not installed")]
    NotInstalled { id: String },

    /// Something exists at `id`, but it carries no Goetia marker at all,
    /// unlike [`NotInstalled`](Error::NotInstalled), where nothing is there.
    /// Every [`ServiceManager`] verb other than `install` refuses a foreign id
    /// with this, never `NotInstalled`, which would send the user looking for
    /// a missing service instead of to the remedy in `recovery` (the wording
    /// of `decide::Outcome::RefuseForeign`'s, via
    /// [`crate::decide::foreign_recovery`]).
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("daemon `{id}` exists but is not managed by goetia: {recovery}")]
    Foreign { id: String, recovery: String },

    /// A read that would have said whether anything is installed at `id`
    /// failed, so goetia does not know. `reason` is the failed read, which
    /// the chain ends in; `recovery` says what would make that read succeed,
    /// and [`report`](Error::report) prints it last, as [`remedy`](Error::remedy).
    ///
    /// What this variant does **not** claim is its point.
    /// [`NotInstalled`](Error::NotInstalled) proves nothing goetia-attributable
    /// is at the id; [`Foreign`](Error::Foreign), and
    /// `cli::report::Kind::Unusable` ("goetia owns the id but cannot report on
    /// it"), prove something is. A failed read proves neither, so reporting
    /// either invents evidence: "installed but unusable" over an
    /// `/etc/systemd/system/<id>.service.d` an unelevated caller merely could
    /// not open puts goetia's name on what may be a stranger's override.
    /// Choose by what was established.
    ///
    /// Exit `4` (indeterminate), like `Unusable`: the question was not answered.
    #[error("cannot determine whether daemon `{id}` is installed")]
    Undetermined {
        id: String,
        #[source]
        reason: Box<Error>,
        recovery: String,
    },

    /// A wait for `id` to reach `awaited` ran out of budget. Produced only
    /// through [`crate::manager::budget::timed_out`], so all three backends
    /// word one condition identically.
    ///
    /// Unlike [`Undetermined`](Error::Undetermined), nothing here is in doubt
    /// about what is *installed* at the id: that was answered before the wait
    /// began. From a backend, the request was issued (the budget never decides
    /// that; on launchd it may still be on its way in a `launchctl` goetia left
    /// running) and goetia stopped watching for its outcome. It was not
    /// cancelled, and the service may still arrive. `cli::restart` is the one
    /// producer that also reports a step it never issued: a start leg its
    /// budget ran out before, whose message says so. A backend must never
    /// report an expiry as `Ok(())`, nor as `Undetermined`.
    ///
    /// Exit `4` (indeterminate), like `Undetermined`.
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
    /// The refusal itself may be determinate (a launchd `bootstrap` that fails
    /// outright) or not (SCM's `ERROR_SERVICE_ALREADY_RUNNING` says only that
    /// the service is not stopped). Either way the stop was never confirmed,
    /// so where the daemon ended up is unestablished; the refusal answers
    /// whether the start went out, not that. Hence not a plain failure, exit
    /// `1`. (A plain `start --timeout 0` of the same service is exit `1`: with
    /// no stop before it, the refusal is the whole answer.) Not
    /// [`WaitTimeout`](Error::WaitTimeout): nothing waited. Not
    /// [`Undetermined`](Error::Undetermined): what is installed at the id was
    /// never in doubt.
    ///
    /// Exit `4` (indeterminate), like the two above.
    ///
    /// `refused` is the start's refusal, which the chain ends in.
    #[error(
        "`{id}` was not restarted, and where it ended up is not established: the stop was issued without waiting \
         for it, and the start that followed was refused (`goetia daemon status {id}` shows the manager's current \
         view)"
    )]
    Unestablished {
        id: String,
        #[source]
        refused: Box<Error>,
    },

    /// A request goetia lost track of before its outcome was established: the
    /// tool carrying it — `systemctl`, `launchctl` — may have started when it
    /// failed, so whatever it sends may already be sent. `request` is the
    /// tool's command line; `source` is how it was lost, and `note` is what a
    /// caller that knows more says before it ([`Error::in_doubt_context`]).
    /// `reached` is whether goetia knows the request reached the manager:
    /// systemd says so (`Enqueued anchor job`) once it has enqueued the job,
    /// which may already be running, and a request lost after that had
    /// reached it.
    ///
    /// It does **not** claim that nothing was sent, as a plain failure would;
    /// that goetia waited and gave up, as [`WaitTimeout`](Error::WaitTimeout)
    /// does; or that what is installed at the id is in doubt, as
    /// [`Undetermined`](Error::Undetermined) does. A failure goetia can place
    /// before the tool ran is never this.
    ///
    /// Exit `4` (indeterminate), like the three above.
    #[error("{}", in_doubt(request, *reached, note))]
    RequestInDoubt {
        request: String,
        reached: bool,
        note: Option<String>,
        #[source]
        source: BoxError,
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
    #[error("{}", loaded_job_in_doubt(id, restore_failed.is_some()))]
    LoadedJobInDoubt {
        id: String,
        #[source]
        restore_failed: Option<Box<Error>>,
    },

    /// `install` wrote the plist of a daemon and could not read whether launchd has its job loaded
    /// afterwards, so which definition a job loaded meanwhile read is not established either.
    /// `read_failed` is the failed read, printed in the message; `restore_failed` is the
    /// [`source`](std::error::Error::source), as in [`LoadedJobInDoubt`](Error::LoadedJobInDoubt).
    /// launchd only.
    ///
    /// Exit `4` (indeterminate).
    #[error("{}", loaded_state_in_doubt(id, read_failed, restore_failed.is_some()))]
    LoadedStateInDoubt {
        id: String,
        read_failed: Box<Error>,
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
    StaleJobBootedOut { id: String, path: PathBuf, found: Occupant },

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
    /// `status`, `show`, `diff`, or `install --dry-run`.
    #[error("`{subcommand}` requires elevation (root/Administrator): re-run as root or Administrator")]
    ElevationRequired { subcommand: String },

    /// [`crate::manager::native`] has no [`ServiceManager`] implementation
    /// for the running platform — anything but Linux, macOS or Windows. An
    /// error, never a panic.
    ///
    /// [`ServiceManager`]: crate::manager::ServiceManager
    #[error("no backend for {platform} yet")]
    UnsupportedPlatform { platform: String },

    /// Resolving a [`crate::spec::User`] to a real platform account failed:
    /// no such user/uid, or the lookup itself errored. `source` is the
    /// lookup's own error, when it had one.
    #[error("account lookup failed: {detail}")]
    AccountLookup {
        detail: String,
        #[source]
        source: Option<BoxError>,
    },

    /// A `start` was judged as a `type: managed` service's, because the service's spec blob could not
    /// be read, and then failed. `blob` is the unreadable blob's error, printed in the message;
    /// `source` is the failed start.
    #[error("{}", start_as_managed_message(blob))]
    StartAsManaged {
        blob: Arc<Error>,
        #[source]
        source: Box<Error>,
    },

    /// A backend tried to create or move an artifact to `path`, but
    /// something already occupies it — a state none of that backend's own
    /// operations should produce.
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
            | Error::StaleJobBootedOut { .. }
            | Error::StartUnconfirmed { .. } => true,
            Error::Step { source, .. } | Error::StartAsManaged { source, .. } => source.is_indeterminate(),
            Error::Kept(kept) => kept.is_indeterminate(),
            Error::Both { first, then } => first.is_indeterminate() && then.is_indeterminate(),
            Error::Os { .. }
            | Error::Command { .. }
            | Error::Spawn { .. }
            | Error::ConcurrentChange { .. }
            | Error::QuarantineStranded { .. }
            | Error::AmbiguousInstall { .. }
            | Error::NotStarted { .. }
            | Error::LoadedJob { .. }
            | Error::PutBackBlocked { .. }
            | Error::InvalidPassword { .. }
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
            | Error::Blob { .. }
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

    /// The error a chain of [`Step`](Error::Step)s and [`StartAsManaged`](Error::StartAsManaged)s
    /// ends in, which classification reads: a step over [`NotInstalled`](Error::NotInstalled) is
    /// still "not installed". [`Both`](Error::Both) has two causes, so it is itself the cause.
    /// [`map_cause`](Error::map_cause) walks the same chain.
    pub fn cause(&self) -> &Error {
        match self {
            Error::Step { source, .. } | Error::StartAsManaged { source, .. } => source.cause(),
            e => e,
        }
    }

    /// This error with `f` applied to its [`cause`](Error::cause), the wrappers kept around the
    /// result: the operation that failed on the cause stays in the report.
    pub fn map_cause(self, f: impl FnOnce(Error) -> Error) -> Error {
        match self {
            Error::Step { what, source } => Error::Step {
                what,
                source: Box::new(source.map_cause(f)),
            },
            Error::StartAsManaged { blob, source } => Error::StartAsManaged {
                blob,
                source: Box::new(source.map_cause(f)),
            },
            cause => f(cause),
        }
    }

    /// The first OS error code this error carries, anywhere in it; the search order is
    /// `report::os_error_in`'s.
    pub fn raw_os_error(&self) -> Option<i32> {
        report::os_error_in(self)
    }

    /// The failed read and its recovery, when this error's [`cause`](Error::cause) is an
    /// [`Undetermined`](Error::Undetermined); any other error comes back as it is. The wrappers
    /// stay around the read. Only the launchd and SCM `list` scans take an `Undetermined` apart.
    #[cfg(any(target_os = "macos", windows, test))]
    pub(crate) fn into_undetermined(self) -> std::result::Result<(Box<Error>, String), Error> {
        let Error::Undetermined { recovery, .. } = self.cause() else {
            return Err(self);
        };
        let recovery = recovery.clone();
        let read = self.map_cause(|cause| match cause {
            Error::Undetermined { reason, .. } => *reason,
            other => other,
        });
        Ok((Box::new(read), recovery))
    }

    /// What would clear this failure, when it is written to follow the failure's cause: a remedy
    /// says "that read", so it cannot lead. [`report`](Error::report) prints it after the whole
    /// chain, and this variant's [`Display`](fmt::Display) leaves it out.
    ///
    /// A variant with no source keeps its remedy in its `Display`, where nothing follows it.
    pub fn remedy(&self) -> Option<&str> {
        match self {
            Error::Undetermined { recovery, .. } => Some(recovery),
            Error::Kept(kept) => kept.remedy(),
            _ => None,
        }
    }

    /// The sentence `Display` writes around an embedded error that is not the `source`, so `report`
    /// can write that error with its remedies last; `None` otherwise. A [`Kept`](Error::Kept) error
    /// answers as the error it holds.
    ///
    /// Exhaustive on purpose, like [`is_indeterminate`](Error::is_indeterminate): a new variant must
    /// be classified here before it compiles.
    pub(crate) fn sentence(&self) -> Option<Sentence<'_>> {
        match self {
            Error::LoadedStateInDoubt {
                id,
                read_failed,
                restore_failed,
            } => Some(loaded_state_in_doubt(id, read_failed, restore_failed.is_some())),
            Error::StartAsManaged { blob, .. } => Some(start_as_managed_message(blob)),
            Error::Os { .. }
            | Error::Command { .. }
            | Error::Spawn { .. }
            | Error::Step { .. }
            | Error::Both { .. }
            | Error::ConcurrentChange { .. }
            | Error::QuarantineStranded { .. }
            | Error::AmbiguousInstall { .. }
            | Error::NotStarted { .. }
            | Error::StartUnconfirmed { .. }
            | Error::LoadedJob { .. }
            | Error::PutBackBlocked { .. }
            | Error::InvalidPassword { .. }
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
            | Error::Blob { .. }
            | Error::NotInstalled { .. }
            | Error::Foreign { .. }
            | Error::Undetermined { .. }
            | Error::WaitTimeout { .. }
            | Error::NotRunning { .. }
            | Error::Unestablished { .. }
            | Error::RequestInDoubt { .. }
            | Error::LoadedJobInDoubt { .. }
            | Error::StaleJobBootedOut { .. }
            | Error::NoManager { .. }
            | Error::ElevationRequired { .. }
            | Error::UnsupportedPlatform { .. }
            | Error::AccountLookup { .. }
            | Error::AlreadyExists { .. } => None,
            Error::Kept(kept) => kept.sentence(),
        }
    }

    /// The error and its causes, joined as `context: cause: cause`, then each [`remedy`](Error::remedy)
    /// found along the chain, outermost first, as `. remedy`; a [`Both`](Error::Both) prints as
    /// `first; then: then`. What the CLI prints: [`Display`](fmt::Display) alone is the outermost
    /// context. The control and direction characters of what an error quotes are written as
    /// `\u{XXXX}`, as the CLI's other human sinks write them; a child's stderr keeps its line feeds
    /// and tabs.
    pub fn report(&self) -> impl fmt::Display + '_ {
        Report {
            error: self,
            escaped: true,
        }
    }

    /// [`report`](Error::report) with every character as it is: the exact text for `--json`.
    #[cfg(feature = "cli")]
    pub(crate) fn report_unescaped(&self) -> impl fmt::Display + '_ {
        Report {
            error: self,
            escaped: false,
        }
    }

    /// Writes [`report`](Error::report) into `out`, which decides how it is escaped.
    #[cfg(feature = "cli")]
    pub(crate) fn write_report(&self, out: &mut Sink) -> fmt::Result {
        report::write_report(self, out)
    }

    /// This error, or the error it hands out if it is a [`Kept`](Error::Kept) one.
    pub(crate) fn unkept(&self) -> &Error {
        match self {
            Error::Kept(kept) => kept.unkept(),
            other => other,
        }
    }

    /// The diagnostic of a failed [`Command`](Error::Command), which `Display` leaves out. A
    /// [`Kept`](Error::Kept) error answers as the error it holds.
    pub(crate) fn stderr(&self) -> Option<&str> {
        match self {
            Error::Command { stderr, .. } => Some(stderr),
            Error::Kept(kept) => kept.stderr(),
            _ => None,
        }
    }

    /// A [`RequestInDoubt`](Error::RequestInDoubt) with `context` said ahead of any note it
    /// already has: what the caller that holds it knows of the request. Any other error is returned
    /// as it is.
    #[cfg(any(feature = "cli", target_os = "linux", test))]
    pub(crate) fn in_doubt_context(self, context: String) -> Self {
        match self {
            Error::RequestInDoubt {
                request,
                reached,
                note,
                source,
            } => Error::RequestInDoubt {
                request,
                reached,
                note: Some(match note {
                    Some(note) => format!("{context}: {note}"),
                    None => context,
                }),
                source,
            },
            e => e,
        }
    }

    /// A metadata blob that is not usable, for a reason that is goetia's own to state.
    pub(crate) fn blob(message: impl Into<String>) -> Self {
        Error::Blob {
            message: message.into(),
            source: None,
        }
    }

    /// A metadata blob that is not usable because `source` failed to decode it.
    pub(crate) fn blob_from(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Error::Blob {
            message: message.into(),
            source: Some(Arc::new(source)),
        }
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

/// What `install`'s put-back found at the plist's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutBackFound {
    /// Nothing: the plist was removed or moved.
    Gone,
    /// A plist other than the one `install` wrote.
    Replaced,
}

/// What `install` found where it was about to create a plist ([`Error::StaleJobBootedOut`]).
#[derive(Debug)]
pub enum Occupant {
    File,
    Directory,
    Symlink,
    /// A device, socket or the like.
    Special,
    /// The path could not be inspected. The error is part of the message, not the error's
    /// [`source`](std::error::Error::source), which would print it a second time.
    Unreadable(std::io::Error),
}

impl fmt::Display for Occupant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Occupant::File => f.write_str("a file"),
            Occupant::Directory => f.write_str("a directory"),
            Occupant::Symlink => f.write_str("a symlink"),
            Occupant::Special => f.write_str("a special file"),
            Occupant::Unreadable(e) => write!(f, "something that could not be read ({})", Chain(e)),
        }
    }
}

/// [`Error::PutBackBlocked`]'s predicate.
fn put_back_found(found: PutBackFound) -> &'static str {
    match found {
        PutBackFound::Gone => "no longer exists",
        PutBackFound::Replaced => "no longer holds what install wrote",
    }
}

/// [`Error::NotStarted`]'s tail for a `restart: always` daemon, or nothing.
fn keep_alive_note(keep_alive: bool) -> &'static str {
    if keep_alive {
        ", and `restart: always` means it should have one"
    } else {
        ""
    }
}

/// What a version source reported.
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
    debug_assert!(
        !sentences.is_empty(),
        "UnsupportedManager is raised only for a reading below the floor"
    );
    format!(
        "goetia requires systemd {SYSTEMD_FLOOR} or newer. {}",
        sentences.join(" ")
    )
}

fn path_suffix(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map(Path::display)
        .map(|p| format!(" {p}"))
        .unwrap_or_default()
}

fn read_at(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map(Path::display)
        .map(|p| format!(" at {p}"))
        .unwrap_or_default()
}

/// The reason a request announced no job: the client's diagnostic, or that it gave none.
fn why(diagnostic: &Option<String>) -> String {
    match diagnostic {
        Some(diagnostic) => format!(": {diagnostic}"),
        None => " and said nothing about why".to_string(),
    }
}

fn id_prefix(id: &Option<String>) -> String {
    id.as_deref().map(|id| format!("`{id}`: ")).unwrap_or_default()
}

/// [`Error::RequestInDoubt`]'s message: what goetia knows of how far the
/// request got, and no more.
fn in_doubt(request: &str, reached: bool, note: &Option<String>) -> String {
    let note = note.as_deref().map(|note| format!(": {note}")).unwrap_or_default();
    if reached {
        format!(
            "`{request}` reached the service manager, but goetia lost track of it before its outcome was confirmed{note}"
        )
    } else {
        format!(
            "`{request}` may have started before it failed, so it may or may not have reached the service manager{note}"
        )
    }
}

/// A message with an error set inside it: `before`, the error, `after`. `Display` writes the
/// error as [`Chain`] does, without its stderr; [`Error::report`] and [`Reason`] write it as they
/// write any error (see [`Error::sentence`]).
pub(crate) struct Sentence<'a> {
    pub(crate) before: Cow<'a, str>,
    pub(crate) error: &'a Error,
    pub(crate) after: Cow<'a, str>,
}

impl fmt::Display for Sentence<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}{}", self.before, Chain(self.error), self.after)
    }
}

/// The message of [`Error::StartAsManaged`]: `blob` is the unreadable blob's error.
fn start_as_managed_message(blob: &Error) -> Sentence<'_> {
    Sentence {
        before: "its spec blob could not be read, so the start was judged as a `type: managed` service's (".into(),
        error: blob,
        after: ")".into(),
    }
}

/// The message of [`Error::LoadedJobInDoubt`].
fn loaded_job_in_doubt(id: &str, restore_failed: bool) -> String {
    format!(
        "`{id}` was loaded in launchd while its plist was written{}",
        loaded_in_doubt_tail(id, restore_failed)
    )
}

/// The message of [`Error::LoadedStateInDoubt`]; `read_failed` is the failed read.
fn loaded_state_in_doubt<'a>(id: &str, read_failed: &'a Error, restore_failed: bool) -> Sentence<'a> {
    Sentence {
        before: format!("`{id}`'s plist was written, but whether launchd has its job loaded could not be read (")
            .into(),
        error: read_failed,
        after: format!("){}", loaded_in_doubt_tail(id, restore_failed)).into(),
    }
}

/// What both in-doubt messages end with. `restore_failed` is whether the put-back of the plist
/// failed, whose error is the variant's source.
fn loaded_in_doubt_tail(id: &str, restore_failed: bool) -> String {
    let remedy = if restore_failed {
        format!(
            "Check what is on disk, then run `goetia daemon stop {id}`, re-run install, then `goetia daemon start {id}`. The plist could not be put back as it was before this install"
        )
    } else {
        format!(
            "The plist is back as it was before this install, so the change is not installed: run `goetia daemon stop {id}`, re-run install, then `goetia daemon start {id}`"
        )
    };
    format!(", so goetia cannot tell whether a loaded job holds the old definition or the new one. {remedy}")
}

/// This crate's `Result` alias, used throughout [`crate::manager`] and
/// [`crate::cli`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[path = "error_tests.rs"]
pub(crate) mod error_tests;
