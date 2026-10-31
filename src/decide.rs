//! The shared install decision.
//!
//! One pure function, called by the test fake and by all three real
//! backends, so the install policy — absent / ours / up-to-date / conflict
//! / foreign — cannot drift between them. See the design spec's §5
//! "Install semantics" for the table this implements.

use crate::blob::Blob;
use crate::diff;
use crate::spec::DaemonSpec;

// Ownership ===========================================================================================================

/// What discovery found at the id an install targets, before any decision
/// about what to do with it.
// `Blob` embeds a full `DaemonSpec` (command/env/paths), so `Ours` is much
// larger than `Foreign`/`Absent`. Boxing it would ripple into every future
// backend's match arms for no behavioral benefit, since `decide` is called
// on the order of once per daemon per install, never in a hot loop.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Nothing exists at this id.
    Absent,
    /// A Goetia-marked artifact exists and its blob decoded successfully.
    /// `regenerated` is `generate(blob.spec)` — the caller's own generator
    /// applied to the *embedded* spec, not the new one — and `on_disk` is
    /// the artifact's raw text. Bundling both into this variant, rather than
    /// passing them as separate parameters, means a caller cannot forget
    /// either, and cannot pass `on_disk` for an ownership that never reads
    /// it: the three-way comparison of `on_disk`, `regenerated` and
    /// `desired` this module needs is always available.
    Ours {
        blob: Blob,
        regenerated: String,
        on_disk: String,
    },
    /// A Goetia-marked artifact exists but its blob will not decode — a
    /// future schema, or corruption. Still *ours*: the marker was written
    /// by some Goetia. Treating this as `Foreign` would mean downgrading
    /// Goetia bricks management of a service this very tool created,
    /// behind a message that misleadingly says "this isn't mine".
    ///
    /// `recovery` is the action that unblocks the id, or `None` when
    /// discovery names none — a drop-in that cannot be read.
    OursUnreadable { reason: String, recovery: Option<String> },
    /// Something exists at this id and carries no Goetia marker at all.
    /// Refused unconditionally — see `decide`.
    Foreign,
}

// ExternalConfig ======================================================================================================

/// Configuration the manager applies to a service from outside the artifact goetia wrote. Systemd's
/// `<id>.service.d` drop-in directories are the only one that exists today, but this is not
/// architecturally systemd-specific: any backend that grows an analogous side-channel (a launchd
/// override, say) needs the same questions answered, and answering them here — once — is what keeps
/// them from being reinvented per backend.
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
    /// Rendered by the backend rather than here: only the backend knows which locations it writes,
    /// and the way out is in its own vocabulary. [`decide`] carries it into [`Outcome::Conflict`]
    /// without interpreting it.
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
/// never wedge an id with no in-tool remedy — except a `RefuseUnreadable`
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
    /// would report the identical conflict. A caller must not re-derive that
    /// by re-reading the platform: it belongs where the conflict was
    /// determined.
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
    /// A Goetia-marked artifact whose blob will not decode, or whose
    /// ownership could not be established. `recovery` is
    /// [`Ownership::OursUnreadable`]'s, carried through.
    RefuseUnreadable { reason: String, recovery: Option<String> },
}

// decide ==============================================================================================================

/// The recovery text for a foreign (unmarked) id. Its own function, not
/// inlined into `decide`, so a [`ServiceManager`] can give the identical
/// wording when refusing a *non*-`install` verb (`uninstall`/`start`/etc.)
/// against the same foreign id — one string template, not two that can
/// drift apart.
///
/// [`ServiceManager`]: crate::manager::ServiceManager
pub fn foreign_recovery(id: &str) -> String {
    format!(
        "remove or rename the existing `{id}` service through your platform's native service \
         manager (systemctl / launchctl / services.msc), then re-run install — goetia never \
         takes over a service it did not create",
    )
}

/// The recovery text for an id whose blob will not decode. Its own function, like
/// [`foreign_recovery`], so a backend's discovery and the CLI can give the identical wording.
pub fn unreadable_recovery(id: &str) -> String {
    format!(
        "run `goetia daemon uninstall {id}` to remove the unreadable artifact (uninstall does \
         not need to decode it), then re-run install",
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
///   `Foreign` or `OursUnreadable` refusal.
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

        Ownership::OursUnreadable { reason, recovery } => Outcome::RefuseUnreadable {
            reason: reason.clone(),
            recovery: recovery.clone(),
        },

        Ownership::Ours {
            blob,
            regenerated,
            on_disk,
        } => {
            // A version mismatch always means "stale, regenerate" — checked
            // before anything else, and never subject to `force` — *unless*
            // foreign external configuration is present and unforced, in which case it is
            // exactly as much a conflict as a hand-edited fragment is: the
            // artifact was not solely goetia's own doing, so silently
            // regenerating it would destroy something outside goetia without
            // ever telling anyone. Without the version-mismatch priority at
            // all, a release that changes generator output by a single byte
            // would present itself as a hand-edit on every already-installed
            // service — that half of the rule is unconditional.
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
                // The artifact is exactly what the embedded spec itself
                // would regenerate — no hand-edit — so the only reason it
                // differs from `desired` is that the spec changed.
                return Outcome::Update {
                    spec_diff: diff::spec_diff(&blob.spec, new_spec),
                };
            }

            // `on_disk` matches neither `desired` nor `regenerated`: a
            // hand-edit, regardless of whether the spec also changed. A
            // spec change alone cannot explain this difference — a clean
            // one would have matched `regenerated` above — so collapsing
            // this into `Update` would silently discard the hand-edit
            // riding along with the spec change.
            if force {
                Outcome::Update {
                    spec_diff: diff::spec_diff(&blob.spec, new_spec),
                }
            } else {
                Outcome::Conflict {
                    // `regenerated` -> `on_disk`, deliberately, not
                    // `on_disk` -> `desired`. The question a conflict must
                    // answer is "what did you change outside goetia that is
                    // about to be lost?", and only this direction isolates
                    // it. Diffing against `desired` would blend the
                    // hand-edit together with the legitimate spec change,
                    // which is exactly the conflation the two-level diff
                    // design exists to avoid.
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
