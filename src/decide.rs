//! The shared install decision.
//!
//! One pure function shared by the test fake and all three real backends, so
//! the install policy (absent / ours / up-to-date / conflict / foreign)
//! cannot drift between them.

use crate::blob::Blob;
use crate::diff;
use crate::error::Reason;
use crate::spec::{DaemonSpec, Id};

// Ownership ===========================================================================================================

/// What discovery found at the id an install targets, before any decision
/// about what to do with it.
// `Ours` embeds a full `DaemonSpec`; boxing it would ripple into every backend's match arms, and
// `decide` runs once per daemon per install.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Nothing exists at this id.
    Absent,
    /// A Goetia-marked artifact exists and its blob decoded successfully.
    /// `regenerated` is `generate(blob.spec)` — the caller's own generator
    /// applied to the *embedded* spec, not the new one — and `on_disk` is
    /// the artifact's raw text. Both live in this variant so the three-way
    /// comparison of `on_disk`, `regenerated` and `desired` is always
    /// available and `on_disk` cannot be passed for an ownership that never
    /// reads it.
    Ours {
        blob: Blob,
        regenerated: String,
        on_disk: String,
    },
    /// A Goetia-marked artifact exists, and goetia cannot use it: compare, report or render it.
    /// Still *ours*: the marker was written by some Goetia. Treating it as `Foreign` would deny
    /// ownership of a service goetia created.
    ///
    /// The causes:
    /// 1. The embedded blob does not decode: a newer schema, or corruption.
    /// 2. The embedded spec no longer renders: its account no longer resolves, or SCM refuses
    ///    an older `restart: always` blob.
    /// 3. The read of the installed definition, for comparison, failed (`install` and `diff`
    ///    only).
    /// 4. A marked entry sits at a name that is not a valid daemon id (`list` only).
    /// 5. The live-state query failed (`list` and `status`).
    /// 6. On launchd, the plist is present in both the staging and the enabled directory and at least
    ///    one carries the marker, so the installation is ambiguous (`list` only).
    ///
    /// `reason` is the failure itself, so its chain stays typed; `recovery` is the action that
    /// unblocks the id, or `None` when discovery names none — a drop-in that cannot be read.
    OursUnusable { reason: Reason, recovery: Option<String> },
    /// A Goetia-marked service the manager found at this id, whose embedded spec names the
    /// different daemon `installed`: a manager that matches ids case-insensitively answers a
    /// request for `FRPC` with `frpc`'s service. Refused unconditionally — see `decide`.
    OtherDaemon { installed: Id },
    /// Something exists at this id and carries no Goetia marker at all.
    /// Refused unconditionally — see `decide`.
    Foreign,
}

// ExternalConfig ======================================================================================================

/// Configuration the manager applies to a service from outside the artifact goetia wrote. Systemd's
/// `<id>.service.d` drop-in directories are the only one today, but the questions are not
/// systemd-specific; answering them here keeps them from being reinvented per backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalConfig {
    /// None exists.
    Absent,
    /// Some exists, and a successful write by this backend removes all of it. Changes exactly one
    /// thing in [`decide`]: a stale artifact that also carries it is `Conflict`, not a
    /// silently-regenerated `Stale`.
    Clearable,
    /// Some exists that a successful write will *not* remove, so `--force` cannot resolve the
    /// conflict. `recovery` is the way out.
    ///
    /// Rendered by the backend, which alone knows which locations it writes. [`decide`] carries it
    /// into [`Outcome::Conflict`] without interpreting it.
    Unclearable { recovery: String },
}

impl ExternalConfig {
    /// The recovery to carry into [`Outcome::Conflict`]: `Some` only for [`Self::Unclearable`].
    fn unclearable_recovery(&self) -> Option<String> {
        match self {
            Self::Absent | Self::Clearable => None,
            Self::Unclearable { recovery } => Some(recovery.clone()),
        }
    }
}

// Outcome =============================================================================================================

/// What `decide` recommends doing. Every refusing variant names the exact
/// action that unblocks it — an install interrupted at the wrong moment must
/// never wedge an id with no in-tool remedy — except a `RefuseUnusable`
/// whose `recovery` is `None`, where no goetia action unblocks it. For
/// `Conflict` that action is `--force` only where `--force` can actually
/// take it; see `unclearable_recovery`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing exists yet: write the artifact.
    Create,
    /// The artifact on disk already matches what `new_spec` would
    /// generate. Nothing to do.
    UpToDate,
    /// The artifact matches what its own embedded spec would regenerate —
    /// no hand-edit — but `new_spec` has changed since. Safe to
    /// regenerate; `spec_diff` is the before/after at the spec level.
    Update { spec_diff: String },
    /// The artifact does not match what its own embedded spec would
    /// regenerate: something other than Goetia touched it. `force`
    /// overwrites (see `decide`); without it, this is refused with exit
    /// code 5.
    ///
    /// `unclearable_recovery` is [`ExternalConfig::Unclearable`]'s recovery
    /// carried through: `None` when `--force` genuinely resolves this, `Some`
    /// — with the text saying what to do instead — when part of what makes the
    /// artifact differ is outside the directory the backend writes, so
    /// overwriting the artifact would leave it in place and the next run
    /// would report the identical conflict. Callers must not re-derive this
    /// from the platform.
    Conflict {
        artifact_diff: String,
        unclearable_recovery: Option<String>,
    },
    /// The embedded blob was generated by a different Goetia version.
    /// Never a `Conflict` — a release that changes generator output by a
    /// single byte must not read as "you hand-edited this", which trains
    /// people to reach for the same `--force` that discards real edits.
    Stale { from_version: String },
    /// An unmarked, pre-existing artifact. No flag overrides this:
    /// adopting a stranger's service is categorically different from
    /// overwriting one Goetia itself created, and conflating the two
    /// under `--force` is how a typo'd id destroys a hand-configured
    /// production service.
    RefuseForeign { recovery: String },
    /// A Goetia-marked artifact goetia cannot compare; the causes are on
    /// [`Ownership::OursUnusable`], whose `recovery` is carried through.
    RefuseUnusable { reason: Reason, recovery: Option<String> },
    /// The service found at the requested id is another daemon's, `installed`. No flag overrides
    /// this: installing would replace that daemon's service. [`other_daemon_refusal`] says what to do.
    RefuseOtherDaemon { installed: Id },
}

// decide ==============================================================================================================

/// The recovery text for a foreign (unmarked) id. Shared so a [`ServiceManager`] gives the
/// identical wording when refusing a non-`install` verb against the same id.
///
/// [`ServiceManager`]: crate::manager::ServiceManager
pub fn foreign_recovery(id: &str) -> String {
    format!(
        "remove or rename the existing `{id}` service through your platform's native service \
         manager (systemctl / launchctl / services.msc), then re-run install — goetia never \
         takes over a service it did not create",
    )
}

/// The recovery text for an id whose blob will not decode. Shared, like [`foreign_recovery`], so
/// a backend's discovery and the CLI give the identical wording.
pub fn unusable_recovery(id: &str) -> String {
    format!(
        "run `goetia daemon uninstall {id}` to remove the unusable artifact (uninstall does \
         not need to decode it), then re-run install",
    )
}

/// Why, and what to do, for [`Outcome::RefuseOtherDaemon`]. Shared so `install` and `diff` give
/// the identical wording after their own `<id>: refused:` / `<id>: would be refused:` prefix.
pub fn other_daemon_refusal(installed: &str) -> String {
    format!(
        "the service manager finds the installed daemon `{installed}` at this id, and installing \
         would replace it. Give this daemon a different id, or uninstall `{installed}` first.",
    )
}

/// The install policy, in one place.
///
/// - `found` is what discovery saw at the target id; for `Ownership::Ours`
///   it includes the artifact's raw text.
/// - `desired` is `generate(new_spec)` — the artifact text the calling
///   backend's generator would write right now, under the identity and
///   crate version currently running.
/// - `new_spec` is the resolved spec install was asked to apply.
/// - `running_version` is the currently running Goetia's own version,
///   compared against the embedded blob's `version` to tell a stale
///   artifact (regenerate silently) from a hand-edited one (conflict).
/// - `force` allows overwriting a genuine hand-edit. It never overrides a
///   `Foreign`, `OtherDaemon` or `OursUnusable` refusal.
/// - `external` is what the manager applies to this artifact from outside
///   its text — see [`ExternalConfig`]. Any but `Absent` changes one row: a
///   stale artifact carrying it is `Conflict`, not a
///   silently-regenerated `Stale`. `Unclearable`'s recovery changes no row at
///   all, and is carried into [`Outcome::Conflict`] so the CLI can say whether
///   `--force` is the way out.
///
/// For `Ownership::Ours`, the three-way comparison this needs is: does
/// `on_disk` match `desired` (nothing to do), does it match
/// `regenerated` (a clean, un-hand-edited spec change), or does it match
/// neither (a hand-edit, present whether or not the spec also changed)?
pub fn decide(
    found: &Ownership,
    desired: &str,
    new_spec: &DaemonSpec,
    running_version: &str,
    force: bool,
    external: &ExternalConfig,
) -> Outcome {
    match found {
        Ownership::Absent => Outcome::Create,

        Ownership::Foreign => Outcome::RefuseForeign {
            recovery: foreign_recovery(new_spec.id.as_str()),
        },

        Ownership::OtherDaemon { installed } => Outcome::RefuseOtherDaemon {
            installed: installed.clone(),
        },

        Ownership::OursUnusable { reason, recovery } => Outcome::RefuseUnusable {
            reason: reason.clone(),
            recovery: recovery.clone(),
        },

        Ownership::Ours {
            blob,
            regenerated,
            on_disk,
        } => {
            // A version mismatch is checked first and means "stale, regenerate" — unless external
            // configuration is present and unforced: then regenerating would silently destroy
            // something outside goetia, so it is a conflict. Checking it first is unconditional;
            // otherwise a release that changes generator output by one byte would read as a
            // hand-edit on every installed service.
            if blob.version != running_version {
                if *external != ExternalConfig::Absent && !force {
                    return Outcome::Conflict {
                        artifact_diff: diff::artifact_diff(regenerated, on_disk),
                        unclearable_recovery: external.unclearable_recovery(),
                    };
                }
                return Outcome::Stale {
                    from_version: blob.version.clone(),
                };
            }

            if on_disk == desired {
                return Outcome::UpToDate;
            }

            if on_disk == regenerated {
                // No hand-edit: the spec alone changed.
                return Outcome::Update {
                    spec_diff: diff::spec_diff(&blob.spec, new_spec),
                };
            }

            // Matches neither `desired` nor `regenerated`: a hand-edit, whether or not the spec
            // also changed. Collapsing this into `Update` would silently discard it.
            if force {
                Outcome::Update {
                    spec_diff: diff::spec_diff(&blob.spec, new_spec),
                }
            } else {
                Outcome::Conflict {
                    // `regenerated` -> `on_disk`, not `on_disk` -> `desired`: only this direction
                    // isolates the hand-edit about to be lost from the legitimate spec change.
                    artifact_diff: diff::artifact_diff(regenerated, on_disk),
                    unclearable_recovery: external.unclearable_recovery(),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod decide_tests;
