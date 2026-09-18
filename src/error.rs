//! Crate-wide error type.

use std::path::PathBuf;

/// Errors produced while reading, parsing, or resolving a `goetia.yaml`
/// manifest.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading `path` from disk failed.
    #[error("failed to read {path}: {source}", path = path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A `.env` file beside the manifest could not be parsed. Distinct from
    /// `Io`, which means the file could not be read at all.
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
    /// checked during deserialization: a duplicate or case-insensitively
    /// colliding daemon id, or a malformed `user` field. `yaml_serde`
    /// attaches a line/column to the message.
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
    /// `cli::report::Kind::Unreadable`, whose published meaning is "goetia
    /// owns the id but cannot report on it" — is proof that something is.
    /// A failed read is the absence of both proofs, so reporting either one
    /// invents evidence: "installed but unreadable" over an
    /// `/etc/systemd/system/<id>.service.d` an unelevated caller merely
    /// could not open puts goetia's name, and `uninstall`'s recovery
    /// advice, on what may be a stranger's override. Choose between the
    /// three by what was established, never by which is closest to hand.
    ///
    /// Exit `4` (indeterminate), like `Unreadable`: the code is about
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
    /// wait ever began. The request was issued — the budget never decides
    /// that, and on launchd it may still be on its way in a `launchctl`
    /// goetia left running — and goetia stopped watching for its outcome;
    /// it was not cancelled, and the service may still arrive. A backend
    /// must never report an expiry as `Ok(())`, and never as `Undetermined`.
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

    /// Steps goetia issued whose outcome nobody established. Produced only
    /// by `cli::restart` under a budget that does not wait, where the stop
    /// was issued without confirmation and the start that followed was
    /// refused. The daemon was not restarted; whether it is running the
    /// instance the stop is still taking down, is going down, or never moved,
    /// goetia proved none of the three.
    ///
    /// The refusal itself may be determinate or not — systemd's `start
    /// --no-block` rejects a unit it cannot load outright, where SCM's
    /// `ERROR_SERVICE_ALREADY_RUNNING` says only that the service is not
    /// stopped. Either way the stop was never confirmed: it may still be in
    /// flight, may have completed, or may have had nothing to do — an
    /// inactive unit systemd cannot load has no stop job at all — so what is
    /// unestablished is where the daemon ended up. Not [`Other`](Error::Other),
    /// exit `1`: a refusal, even a determinate one, answers whether the start
    /// went out, not that question. (A plain `start --timeout 0` of the same
    /// unit is exit `1`: with no stop before it, the refusal is the whole
    /// answer.) Not [`WaitTimeout`](Error::WaitTimeout): nothing waited.
    /// Not [`Undetermined`](Error::Undetermined): what is installed at the
    /// id was never in doubt.
    ///
    /// Exit `4` (indeterminate), like the two above: the code is about
    /// whether the question was answered, and this one was not.
    #[error("`{id}` was not restarted, and where it ended up is not established: {detail}")]
    Unestablished { id: String, detail: String },

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

    /// An external tool a backend shells out to (`launchctl`, `plutil`,
    /// `systemctl`, `sc.exe`, ...) either could not be spawned at all or
    /// exited non-zero. `stderr` carries whatever diagnostic text it
    /// produced (or the spawn error itself, for the "could not run it at
    /// all" case) — one variant for both, since neither backend nor caller
    /// needs to tell them apart.
    #[error("`{command}` failed: {stderr}")]
    CommandFailed { command: String, stderr: String },

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

    /// A lower-level failure, annotated with context a call site adds
    /// rather than a dedicated variant of its own — e.g. `daemon restart`
    /// disclosing that a daemon is now stopped, not merely that starting it
    /// back up failed. Not a catch-all for new call sites to reach for by
    /// default: prefer a real variant when the failure recurs anywhere else.
    #[error("{0}")]
    Other(String),
}

/// This crate's `Result` alias, used throughout [`crate::manager`] and
/// [`crate::cli`].
pub type Result<T> = std::result::Result<T, Error>;
