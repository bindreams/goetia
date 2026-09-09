//! [`Fake`]: an in-memory [`ServiceManager`] with no real I/O.
//!
//! Every CLI test in this crate runs against `Fake`, never a real backend —
//! see `manager.rs`'s module doc comment. `Fake` still routes every
//! `install` through [`crate::decide::decide`], exactly as a real backend
//! must: its own tiny `generate`/`extract` pair below (not any real
//! systemd/launchd/SCM format) exists only so `decide` has something to
//! compare, the same role each real backend's own generator plays.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::blob::{self, Blob};
use crate::decide::{self, Outcome, Ownership};
use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, State, Status};
use crate::spec::{DaemonSpec, Id};

/// The fake's own artifact marker. Deliberately not any of the real
/// `Marker`/`Schema`/`Version`/`Spec` field names — this is not a format any
/// real manager reads, so the Global Constraints' byte-exact metadata
/// naming does not apply to it.
const FAKE_MARKER: &str = "FAKE-GOETIA-ARTIFACT";

/// A fake "PID" reported by [`ServiceManager::status`] while an entry is
/// running. Never a real process.
const FAKE_PID: u32 = 1;

/// The `on_disk` text `decide` is handed for an id carrying only a residual
/// artifact — see [`Fake::seed_residual_artifact`]. Deliberately carries no
/// [`FAKE_MARKER`]: residue is exactly what goetia cannot attribute, and
/// `decide` must reach `RefuseForeign` from it.
const RESIDUAL_TEXT: &str = "residual artifact, no primary artifact\n";

/// Why an opaque id could not be classified — see [`Fake::seed_opaque`].
/// Shared by [`Store::undetermined`] and [`Fake::list`] so the error and the
/// `list` entry cannot describe the same condition differently.
const OPAQUE_REASON: &str = "the artifact could not be read (seeded opaque by Fake::seed_opaque)";

/// The pid `Fake` reports for a given state. `status` and `list` must never
/// disagree about the same entry, so they share this rule rather than each
/// spelling it out — the three real backends get that guarantee from calling
/// one query function, and nothing but this helper would give it to `Fake`.
fn pid_for(state: State) -> Option<u32> {
    if state == State::Running { Some(FAKE_PID) } else { None }
}

#[derive(Debug, Clone)]
struct Entry {
    /// The fake's own "artifact" text: either what `generate` produced
    /// (ours), or arbitrary seeded text (foreign).
    text: String,
    enabled: bool,
    /// Not just running/stopped: test-only [`Fake::seed_state`] can force
    /// `Failed`/`Unknown` too, so CLI rendering code that switches on all
    /// four [`State`] variants (`support::state_str`) has a way to be
    /// exercised for the two a normal install/start/stop sequence can never
    /// produce.
    state: State,
}

/// An in-memory [`ServiceManager`]. See the module doc comment.
///
/// Cheaply `Clone`: every clone shares the same underlying store through an
/// `Arc`. `dispatch`'s `get_manager` closure returns an owned
/// `Box<dyn ServiceManager>` on every call, so a CLI test that dispatches
/// more than once against the same fake (installing, then listing, then
/// uninstalling) needs a `Fake` clone to observe the same state each time,
/// not a fresh empty one.
#[derive(Debug, Default, Clone)]
pub struct Fake {
    state: Arc<Mutex<Store>>,
}

/// [`Fake`]'s whole world: the artifacts, plus the ids that carry only a
/// *residual* one. The two are separate maps because a residual artifact is
/// defined by the primary one being gone — an id cannot be in `entries` and
/// still be residue-only.
#[derive(Debug, Default)]
struct Store {
    entries: BTreeMap<String, Entry>,
    /// Ids with no artifact of their own but some goetia-attributable trace
    /// the platform still applies — see [`Fake::seed_residual_artifact`].
    residual: BTreeSet<String>,
    /// Ids whose artifact this process cannot read at all — see
    /// [`Fake::seed_opaque`]. Consulted before `entries`, because the read
    /// that would have found an entry is the one that failed.
    opaque: BTreeSet<String>,
    /// One reason per seeded aggregate — see
    /// [`Fake::seed_aggregate_undetermined`]. Not a set of ids: an
    /// aggregate is precisely the case where the ids are not known.
    aggregates: Vec<String>,
}

impl Store {
    /// The artifact at `id`, or the error its absence means *there* — never
    /// a bare [`Error::NotInstalled`] read off the map lookup alone. Every
    /// verb goes through this or [`Store::get_mut`], so none of them can
    /// disagree with [`discover`] about one id.
    fn get(&self, id: &Id) -> Result<&Entry> {
        if self.opaque.contains(id.as_str()) {
            return Err(Self::undetermined(id));
        }
        match self.entries.get(id.as_str()) {
            Some(entry) => Ok(entry),
            None => Err(self.absent_error(id)),
        }
    }

    fn get_mut(&mut self, id: &Id) -> Result<&mut Entry> {
        if self.opaque.contains(id.as_str()) {
            return Err(Self::undetermined(id));
        }
        if !self.entries.contains_key(id.as_str()) {
            return Err(self.absent_error(id));
        }
        Ok(self.entries.get_mut(id.as_str()).expect("present, checked just above"))
    }

    /// The fake's single [`Error::Undetermined`] constructor: one wording
    /// for one condition, however many verbs reach it.
    fn undetermined(id: &Id) -> Error {
        Error::Undetermined {
            id: id.as_str().to_string(),
            reason: OPAQUE_REASON.to_string(),
            recovery: "re-run with enough privilege to read the artifact; if it is unreadable at \
                       any privilege level, repair or remove it out of band"
                .to_string(),
        }
    }

    /// [`Error::NotInstalled`] only when nothing at all is at `id`. A
    /// residual artifact makes it [`Error::Foreign`] instead: `cli::
    /// uninstall` maps `NotInstalled` — and only that variant — to exit `0`
    /// and "nothing to do", which must not be the answer for an id the
    /// platform is still acting on.
    fn absent_error(&self, id: &Id) -> Error {
        if self.residual.contains(id.as_str()) {
            return Error::Foreign {
                id: id.as_str().to_string(),
                recovery: decide::foreign_recovery(id.as_str()),
            };
        }
        Error::NotInstalled {
            id: id.as_str().to_string(),
        }
    }
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only seeding: place `text`, carrying no Goetia marker, at `id`
    /// — simulating a pre-existing service Goetia did not create. Discovery
    /// then classifies it as [`Ownership::Foreign`]. See
    /// `manager::conformance`'s module doc comment.
    pub fn seed_foreign(&self, id: &str, text: impl Into<String>) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.entries.insert(
            id.to_string(),
            Entry {
                text: text.into(),
                enabled: false,
                state: State::Stopped,
            },
        );
    }

    /// Test-only seeding: place a Goetia-marked entry at `id` whose blob will
    /// not decode — simulating a newer-schema artifact or one that bit-rotted.
    /// `install_then_hand_edit` cannot produce this state: `extract` only
    /// ever reads the marker and `Spec` line, so appending trailing text
    /// never breaks decoding. This is the only way to reach
    /// `Ownership::OursUnreadable`/[`Installed::OursUnreadable`] from outside
    /// the crate — every CLI test that exercises that path needs it.
    ///
    /// [`Installed::OursUnreadable`]: crate::manager::Installed::OursUnreadable
    pub fn seed_unreadable(&self, id: &str) {
        self.seed_foreign(id, format!("{FAKE_MARKER}\nSpec: not-valid-base64!!!\n"));
    }

    /// Test-only seeding: place a Goetia-marked entry at `id` whose blob's
    /// *envelope* decodes cleanly but whose spec content is out of range, so
    /// [`blob::decode`] rejects it with [`Error::Invalid`] rather than
    /// [`Error::Blob`].
    ///
    /// [`Fake::seed_unreadable`] cannot reach this: malformed base64 fails
    /// at the envelope and yields `Error::Blob`. `Error::Invalid` is the
    /// variant `spec::Id::try_from` *also* produces, so it is the one a CLI
    /// classifying failures by error variant instead of by which operation
    /// failed would misroute to `invalid-id`. Nothing else in the crate can
    /// produce it from a manager.
    ///
    /// The out-of-range value is `restart_delay.nanos == 1_000_000_000`,
    /// which `blob::duration_from_wire` checks precisely because
    /// `Duration::new` would otherwise panic on it.
    pub fn seed_invalid_content(&self, id: &str) {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = DaemonSpec {
            id: Id::try_from(id).expect("seed_invalid_content needs a valid id"),
            name: id.to_string(),
            command: vec!["daemon".to_string()],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            user: crate::spec::User::Root,
            restart: crate::spec::Restart::OnFailure,
            // Replaced with the out-of-range value below; only its presence
            // matters here, so that the field is on the wire at all.
            restart_delay: Some(std::time::Duration::from_secs(1)),
            logs: None,
            kind: crate::spec::Kind::Simple,
        };
        let encoded = blob::encode(&spec);
        let bytes = BASE64.decode(&encoded).expect("blob::encode emits valid base64");
        let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).expect("blob::encode emits valid JSON");
        envelope["spec"]["restart_delay"]["nanos"] = serde_json::json!(1_000_000_000u32);
        let tampered = BASE64.encode(serde_json::to_vec(&envelope).expect("a Value re-serializes"));

        self.seed_foreign(id, format!("{FAKE_MARKER}\nSpec: {tampered}\n"));
    }

    /// Test-only seeding: install `spec` normally, then append `extra_line`
    /// to the stored artifact so it no longer matches what regenerating its
    /// own embedded spec would produce — simulating a hand-edit made
    /// outside Goetia. See `manager::conformance`'s module doc comment.
    pub fn install_then_hand_edit(&self, spec: &DaemonSpec, extra_line: &str) {
        self.install(spec, false).expect("seed install must succeed");
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state
            .entries
            .get_mut(spec.id.as_str())
            .expect("just installed by the line above");
        entry.text.push_str(extra_line);
        if !entry.text.ends_with('\n') {
            entry.text.push('\n');
        }
    }

    /// Test-only seeding: place a Goetia-marked entry at `id` whose blob
    /// decodes cleanly but carries `version` instead of the currently
    /// running crate's own — simulating an artifact written by a different
    /// Goetia release, so `decide` reports [`Outcome::Stale`] rather than
    /// [`Outcome::Conflict`]. Tampers the same way [`Fake::seed_invalid_content`]
    /// does: encode a real blob, then patch just the field under test,
    /// since [`blob::encode`] always embeds [`crate::version()`] and has no
    /// parameter to override it.
    pub fn seed_stale(&self, spec: &DaemonSpec, version: &str) {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let encoded = blob::encode(spec);
        let bytes = BASE64.decode(&encoded).expect("blob::encode emits valid base64");
        let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).expect("blob::encode emits valid JSON");
        envelope["version"] = serde_json::json!(version);
        let tampered = BASE64.encode(serde_json::to_vec(&envelope).expect("a Value re-serializes"));

        self.seed_foreign(spec.id.as_str(), format!("{FAKE_MARKER}\nSpec: {tampered}\n"));
    }

    /// Test-only seeding: leave `id` with no artifact of its own, but with a
    /// goetia-attributable trace the platform still applies — the class
    /// systemd's fragmentless `<id>.service.d/*.conf` drop-in and its
    /// leftover `multi-user.target.wants/<id>.service` enablement link both
    /// belong to. Both outlive the unit fragment, and the second keeps the
    /// id enrolled at boot.
    ///
    /// No other seeder reaches it, because it is defined by what is *not*
    /// there: an id whose primary artifact is gone looks identical to an
    /// empty one to any verb that only ever looks the id up. That matters
    /// because `cli::uninstall` maps [`Error::NotInstalled`] — and only that
    /// variant — to exit `0` and "nothing to do", so a manager answering
    /// `NotInstalled` here certifies "confirmed gone" for an id the platform
    /// is still acting on, while `install` on that same id refuses it as
    /// foreign. `Fake` answers [`Error::Foreign`] instead, and `list` omits
    /// the id entirely — residue is not a daemon to report on.
    pub fn seed_residual_artifact(&self, id: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.entries.remove(id);
        state.residual.insert(id.to_string());
    }

    /// Test-only seeding: make `id`'s artifact unreadable to this process —
    /// the class a root-only unit file under an unelevated `list` belongs
    /// to. Every verb then answers [`Error::Undetermined`] and `list`
    /// reports [`Installed::Undetermined`], because a read that did not
    /// complete establishes neither the id's absence nor its ownership.
    ///
    /// The only way to reach that state in-crate: the fake's artifacts are
    /// in-memory strings and are always readable, so the failure has to be
    /// modelled rather than provoked. It is a dimension of its own and not
    /// a field on `Entry` because an opaque id must also refuse `install`,
    /// which never looks an `Entry` up.
    ///
    /// Independent of whether anything is installed at `id`. Over an
    /// existing entry it is a permission change on a live artifact; over a
    /// bare id it is the state `manager::conformance` seeds at
    /// `UNDETERMINED_ID` — a read that failed before it could establish even
    /// that much. Both are the same claim, which is the point.
    ///
    /// [`Installed::Undetermined`]: crate::manager::Installed::Undetermined
    pub fn seed_opaque(&self, id: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.opaque.insert(id.to_string());
    }

    /// Test-only seeding: make `list` report one [`Installed::Undetermined`]
    /// entry that names nobody — the shape a backend produces when it cannot
    /// usefully name what the entry stands for. Windows SCM produces it on an
    /// unelevated `list` not because enumeration failed (it succeeds, and the
    /// names are known) but because hundreds of services deny a `Parameters`
    /// read at once and one entry cannot carry them all.
    ///
    /// `reason` is a complete sentence, because that is how it is rendered:
    /// there is no name to prefix it with.
    ///
    /// [`Installed::Undetermined`]: crate::manager::Installed::Undetermined
    pub fn seed_aggregate_undetermined(&self, reason: impl Into<String>) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.aggregates.push(reason.into());
    }

    /// Test-only: force `id`'s reported [`State`] directly, bypassing
    /// `start`/`stop` (which can only produce `Running`/`Stopped`). `id`
    /// must already be installed.
    pub fn seed_state(&self, id: &str, new_state: State) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state
            .entries
            .get_mut(id)
            .unwrap_or_else(|| panic!("seed_state({id}, ..): not installed"));
        entry.state = new_state;
    }
}

// generate/extract ====================================================================================================

fn generate(spec: &DaemonSpec) -> String {
    format!("{FAKE_MARKER}\nSpec: {}\n", blob::encode(spec))
}

fn extract(text: &str) -> Result<Option<Blob>> {
    let mut lines = text.lines();
    if lines.next() != Some(FAKE_MARKER) {
        return Ok(None);
    }
    let spec_line = lines
        .next()
        .ok_or_else(|| Error::Blob("fake artifact has no Spec line".to_string()))?;
    let encoded = spec_line
        .strip_prefix("Spec: ")
        .ok_or_else(|| Error::Blob(format!("fake artifact's second line is not `Spec: ...`: {spec_line}")))?;
    let blob = blob::decode(encoded)?;
    Ok(Some(blob))
}

/// Refuse an operation against a *foreign* (unmarked) entry — the same
/// "goetia never touches a service it did not create" rule `install`
/// enforces via `decide`. A marked-but-undecodable entry still passes:
/// the marker alone is proof of ownership, and `uninstall` in particular
/// documents this as the way out of `decide::Outcome::RefuseUnreadable`
/// (its `recovery` text names exactly this verb). Only `install` itself
/// needs the full three-way `Ownership` distinction (via `decide`); every
/// other verb only needs this narrower "is this even ours" gate.
fn require_ours(entry: &Entry, id: &Id) -> Result<()> {
    match extract(&entry.text) {
        Ok(None) => Err(Error::Foreign {
            id: id.as_str().to_string(),
            recovery: decide::foreign_recovery(id.as_str()),
        }),
        Ok(Some(_)) | Err(_) => Ok(()),
    }
}

/// Classify what's at `id` in `state`, exactly as `install` would discover
/// it, plus the raw on-disk text `decide` needs alongside the
/// classification. Shared by `install` and `preview_install` so the two can
/// never disagree about what `decide` sees — `preview_install` exists
/// precisely so `diff` can ask "what would `install` do" without either
/// duplicating this logic or being able to drift from it.
fn discover(state: &Store, id: &str) -> (Ownership, Option<String>) {
    let existing = state.entries.get(id).cloned();
    let found = match &existing {
        // Never silently adopted as `Create`: whatever the platform still
        // applies here is configuration goetia did not write and cannot
        // show — the same refusal systemd's own `discover` gives a
        // fragmentless `<id>.service.d` drop-in.
        None if state.residual.contains(id) => {
            return (Ownership::Foreign, Some(RESIDUAL_TEXT.to_string()));
        }
        None => Ownership::Absent,
        Some(entry) => match extract(&entry.text) {
            Ok(Some(blob)) => Ownership::Ours {
                regenerated: generate(&blob.spec),
                blob,
            },
            Ok(None) => Ownership::Foreign,
            Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
        },
    };
    (found, existing.map(|e| e.text))
}

// ServiceManager ======================================================================================================

impl ServiceManager for Fake {
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        // Before `discover`, which has no error channel: an artifact whose
        // bytes were never read supplies none of `decide`'s inputs, so this
        // is an input failure ahead of policy, not a fifth `Ownership`.
        if state.opaque.contains(spec.id.as_str()) {
            return Err(Store::undetermined(&spec.id));
        }
        let desired = generate(spec);
        let (found, on_disk) = discover(&state, spec.id.as_str());

        // The fake has no concept of an overlay (systemd's drop-in
        // directory, or an analogous future backend's equivalent), so it
        // always passes `decide` the empty one.
        let outcome = decide::decide(
            &found,
            on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            force,
            &decide::Overlay::default(),
        );

        // `Create`/`Update`/`Stale` are the outcomes `decide` recommends
        // actually writing for; every refusing variant (`Conflict` without
        // force, `RefuseForeign`, `RefuseUnreadable`) and `UpToDate` leave
        // the store untouched.
        if matches!(
            outcome,
            Outcome::Create | Outcome::Update { .. } | Outcome::Stale { .. }
        ) {
            let (enabled, run_state) = state
                .entries
                .get(spec.id.as_str())
                .map(|e| (e.enabled, e.state))
                .unwrap_or((false, State::Stopped));
            state.entries.insert(
                spec.id.as_str().to_string(),
                Entry {
                    text: desired,
                    enabled,
                    state: run_state,
                },
            );
        }

        Ok(outcome)
    }

    fn preview_install(&self, spec: &DaemonSpec) -> Result<Outcome> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        // Same pre-check as `install`, for the same reason: `diff` must not
        // predict an outcome from an artifact nobody read.
        if state.opaque.contains(spec.id.as_str()) {
            return Err(Store::undetermined(&spec.id));
        }
        let desired = generate(spec);
        let (found, on_disk) = discover(&state, spec.id.as_str());
        // Always previewed without `force`: showing the forced outcome would
        // hide the very conflict `--force` exists to let a user decide
        // about, and `diff` has no `--force` flag of its own to justify it.
        Ok(decide::decide(
            &found,
            on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            false,
            &decide::Overlay::default(),
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get(id)?;
        require_ours(entry, id)?;
        state.entries.remove(id.as_str());
        Ok(())
    }

    fn enable(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id)?;
        require_ours(entry, id)?;
        entry.enabled = true;
        Ok(())
    }

    fn disable(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id)?;
        require_ours(entry, id)?;
        entry.enabled = false;
        Ok(())
    }

    fn start(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id)?;
        require_ours(entry, id)?;
        entry.state = State::Running;
        Ok(())
    }

    fn stop(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id)?;
        require_ours(entry, id)?;
        // Idempotent: stopping an already-stopped (or failed, or unknown)
        // service is `Ok(())` — see `ServiceManager::stop`'s doc comment for
        // why every backend must agree on this.
        entry.state = State::Stopped;
        Ok(())
    }

    fn status(&self, id: &Id) -> Result<Status> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get(id)?;
        // Unlike the mutating verbs, `status`'s only job is to report the
        // truth: an unreadable entry has no trustworthy `enabled`/`state` to
        // report, so this deliberately does not use `require_ours` (which
        // would let it through) — it surfaces the decode failure instead of
        // fabricating a `Stopped`/`enabled: false` answer for it.
        match extract(&entry.text) {
            Ok(None) => Err(Error::Foreign {
                id: id.as_str().to_string(),
                recovery: decide::foreign_recovery(id.as_str()),
            }),
            Err(e) => Err(e),
            Ok(Some(_)) => Ok(Status {
                state: entry.state,
                pid: pid_for(entry.state),
                enabled: entry.enabled,
            }),
        }
    }

    fn list(&self) -> Result<Vec<Installed>> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        let mut out = Vec::new();
        // Aggregates first, and the named entries last-to-first:
        // `ServiceManager::list` promises no ordering at all, so a caller
        // that renders this order instead of imposing its own must fail a
        // test rather than pass by luck.
        for reason in state.aggregates.iter() {
            out.push(Installed::Undetermined {
                name: None,
                reason: reason.clone(),
            });
        }
        for name in state.opaque.iter().rev() {
            out.push(Installed::Undetermined {
                name: Some(name.clone()),
                reason: OPAQUE_REASON.to_string(),
            });
        }
        for (name, entry) in state.entries.iter() {
            // An id whose read failed is not classifiable from the entry
            // that read would have found — `Store::get` refuses it too.
            if state.opaque.contains(name) {
                continue;
            }
            match extract(&entry.text) {
                // A foreign entry is not Goetia-managed at all: `list`
                // reports only what Goetia owns, per the trait doc comment.
                Ok(None) => {}
                Ok(Some(blob)) => out.push(Installed::Ours {
                    spec: blob.spec,
                    state: entry.state,
                    pid: pid_for(entry.state),
                    enabled: entry.enabled,
                }),
                Err(e) => out.push(Installed::OursUnreadable {
                    name: name.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod fake_tests;
