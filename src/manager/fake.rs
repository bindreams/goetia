//! [`Fake`]: an in-memory [`Backend`](crate::backend::Backend) with no real I/O, and so a [`ServiceManager`].
//!
//! Every CLI test in this crate runs against `Fake`, never a real backend —
//! see `manager.rs`'s module doc comment. Its `install`, `status` and `list`
//! are the shared flows in [`crate::manager::flow`], as for every backend:
//! its own tiny `generate`/`extract` pair below (not any real
//! systemd/launchd/SCM format) exists only so `decide` has something to
//! compare, the same role each real backend's own generator plays.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::{InstalledDefinition, ReadOutcome, Scan, WriteOutcome};
use crate::blob::{self, Blob};
use crate::decide::{ExternalConfig, Outcome};
use crate::error::{Error, Result};
use crate::manager::budget;
use crate::manager::flow;
use crate::manager::{Budget, Installed, Prepared, ServiceManager, State, Status, Step};
use crate::spec::{DaemonSpec, Id, User};

/// The fake's own artifact marker. Deliberately not any of the real
/// `Marker`/`Schema`/`Version`/`Spec` field names — this is not a format any
/// real manager reads, so the Global Constraints' byte-exact metadata
/// naming does not apply to it.
const FAKE_MARKER: &str = "FAKE-GOETIA-ARTIFACT";

/// A fake "PID" reported by [`ServiceManager::status`] while an entry is
/// running. Never a real process.
const FAKE_PID: u32 = 1;

/// Why an opaque id could not be classified — see [`Fake::seed_opaque`].
/// Shared by [`Store::undetermined`] and [`Fake::scan`] so the error and the
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
pub(crate) struct Entry {
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
    /// the platform still applies, each with the backend's own remedy if it
    /// has one — see [`Fake::seed_residual_artifact`].
    residual: BTreeMap<String, Option<String>>,
    /// Ids whose artifact this process cannot read at all — see
    /// [`Fake::seed_opaque`]. Consulted before `entries`, because the read
    /// that would have found an entry is the one that failed.
    opaque: BTreeSet<String>,
    /// One reason per seeded aggregate — see
    /// [`Fake::seed_aggregate_undetermined`]. Not a set of ids: an
    /// aggregate is precisely the case where the ids are not known.
    aggregates: Vec<String>,
    /// Ids whose manager never confirms a start — see
    /// [`Fake::seed_start_stalls`].
    start_stalls: BTreeSet<String>,
    /// [`Fake::seed_stop_stalls`]'s mirror of `start_stalls`.
    stop_stalls: BTreeSet<String>,
    /// Ids whose start request ends in doubt — see [`Fake::seed_start_in_doubt`].
    start_in_doubt: BTreeSet<String>,
    /// [`Fake::seed_stop_in_doubt`]'s mirror of `start_in_doubt`.
    stop_in_doubt: BTreeSet<String>,
    /// Whether this Fake has a request-only restart — see
    /// [`Fake::seed_native_restart`].
    native_restart: bool,
    /// Whether [`ServiceManager::prepare`] fails — see
    /// [`Fake::seed_prepare_fails`].
    prepare_fails: bool,
    /// Whether no manager can be asked — see [`Fake::seed_no_manager`].
    no_manager: bool,
    /// Accounts `render` cannot resolve — see [`Fake::seed_unresolvable_account`].
    unresolvable: BTreeSet<String>,
    /// Ids whose installed definition cannot be read back, with why — see
    /// [`Fake::seed_installed_fails`].
    installed_fails: BTreeMap<String, String>,
    /// Ids whose live state cannot be queried, with why — see [`Fake::seed_live_status_fails`].
    live_status_fails: BTreeMap<String, String>,
    /// Writes left to lose a race, by id — see [`Fake::seed_write_races`].
    write_races: BTreeMap<String, usize>,
    /// Every `write` attempted, raced or not — see [`Fake::writes`].
    writes: usize,
    /// Every `start`/`stop`/`restart`/`kill` this Fake was asked to perform, in
    /// order — see [`Fake::calls`].
    calls: Vec<(&'static str, String)>,
    /// The steps and budget each [`ServiceManager::prepare`] was given, in
    /// order — see [`Fake::prepared`].
    prepared: Vec<(Vec<Step>, Budget)>,
    /// The steps of every guard `prepare` returned that has not been
    /// dropped, by guard number.
    live: Vec<(u64, Vec<Step>)>,
    /// The number the next guard `prepare` returns is known by.
    next_guard: u64,
    /// Every request, with the steps of the guards live when it was asked —
    /// see [`Fake::guarded`].
    guarded: Vec<(&'static str, String, Vec<Step>)>,
}

impl Store {
    /// [`Fake::seed_no_manager`]'s refusal, if seeded.
    fn manager(&self) -> Result<()> {
        if self.no_manager {
            return Err(Error::NoManager {
                evidence: "seeded".to_string(),
            });
        }
        Ok(())
    }

    /// Record a request for [`Fake::guarded`], under the guards live now.
    fn asked(&mut self, verb: &'static str, id: &Id) {
        let steps = self.live.iter().flat_map(|(_, steps)| steps.iter().copied()).collect();
        self.guarded.push((verb, id.as_str().to_string(), steps));
    }

    /// What is at `id`, as [`Backend::read`] reports it. An opaque id fails
    /// first: the read that would have found an entry is the one that failed.
    fn read(&self, id: &Id) -> Result<ReadOutcome<Entry>> {
        if self.opaque.contains(id.as_str()) {
            return Err(Self::undetermined(id));
        }
        Ok(match self.entries.get(id.as_str()) {
            Some(entry) => Self::classify(entry),
            // Never absent: whatever the platform still applies here is
            // configuration goetia did not write and cannot show, and
            // `NotInstalled` is the one error `cli::uninstall` renders as
            // success.
            None => match self.residual.get(id.as_str()) {
                Some(recovery) => ReadOutcome::Foreign {
                    recovery: recovery.clone(),
                },
                None => ReadOutcome::Absent,
            },
        })
    }

    fn classify(entry: &Entry) -> ReadOutcome<Entry> {
        match extract(&entry.text) {
            Ok(None) => ReadOutcome::Foreign { recovery: None },
            Ok(Some(blob)) => ReadOutcome::Marked {
                blob: Ok(blob),
                snapshot: entry.clone(),
            },
            Err(e) => ReadOutcome::Marked {
                blob: Err(e),
                snapshot: entry.clone(),
            },
        }
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
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only seeding: place `text`, carrying no Goetia marker, at `id`
    /// — simulating a pre-existing service Goetia did not create. The shared
    /// flow then classifies it as [`ReadOutcome::Foreign`]. See
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
        state.residual.insert(id.to_string(), None);
    }

    /// [`Fake::seed_residual_artifact`] for a backend that has its own remedy for the residue
    /// (systemd's names the leftover paths): `status` and the ownership gate carry `recovery`
    /// instead of [`crate::decide::foreign_recovery`].
    pub fn seed_residual_recovery(&self, id: &str, recovery: &str) {
        self.seed_residual_artifact(id);
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.residual.insert(id.to_string(), Some(recovery.to_string()));
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

    /// Test-only seeding: mark `id` as a service whose manager never
    /// confirms a start — the state every timeout test needs and no
    /// sequence of real verbs can produce.
    ///
    /// The fake **never sleeps** for it: [`stalled`] reads the budget and
    /// reports the expiry directly. That is what makes a `--timeout 1h` test
    /// finish instantly and deterministically instead of being a bet on a
    /// scheduler.
    pub fn seed_start_stalls(&self, id: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.start_stalls.insert(id.to_string());
    }

    /// Test-only seeding: every start of `id` — `start` and
    /// `request_start_after_stop` alike — fails as a real backend's does when
    /// the tool carrying the request may have started, and was then lost:
    /// [`Error::RequestInDoubt`], changing nothing here.
    pub fn seed_start_in_doubt(&self, id: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.start_in_doubt.insert(id.to_string());
    }

    /// [`Fake::seed_start_in_doubt`]'s mirror for `stop`.
    pub fn seed_stop_in_doubt(&self, id: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.stop_in_doubt.insert(id.to_string());
    }

    /// [`Fake::seed_start_stalls`]'s mirror for `stop`.
    pub fn seed_stop_stalls(&self, id: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.stop_stalls.insert(id.to_string());
    }

    /// Test-only seeding: give this Fake a [`ServiceManager::request_restart`],
    /// as systemd's `systemctl restart --no-block` is. Without it the Fake has
    /// none, as launchd and SCM have none.
    pub fn seed_native_restart(&self) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.native_restart = true;
    }

    /// Test-only seeding: no manager can be asked, as systemd's offline mode
    /// shows. Every verb that reaches the manager refuses with
    /// [`Error::NoManager`] once it has found an artifact of goetia's to act
    /// on, changing nothing; `preview_install` never reaches it.
    pub fn seed_no_manager(&self) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.no_manager = true;
    }

    /// Test-only seeding: `render` fails for a spec whose `user` is `User::Name(user)`, as a
    /// backend's does for an account the platform cannot resolve.
    pub fn seed_unresolvable_account(&self, user: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.unresolvable.insert(user.to_string());
    }

    /// Test-only seeding: reading back what is installed at `id` fails with `detail`, as a
    /// backend's does for a drop-in directory it cannot list.
    pub fn seed_installed_fails(&self, id: &str, detail: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.installed_fails.insert(id.to_string(), detail.to_string());
    }

    /// Test-only seeding: the live state of `id` cannot be queried, failing with `detail` as a
    /// backend's does when the tool it asks is down. Not [`Error::NoManager`], which refuses
    /// every id at once: this is one entry's failure.
    pub fn seed_live_status_fails(&self, id: &str, detail: &str) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.live_status_fails.insert(id.to_string(), detail.to_string());
    }

    /// Test-only seeding: the next `times` writes at `id` lose a race and write nothing, as when
    /// something appears at the id between a backend's read and its write.
    pub fn seed_write_races(&self, id: &str, times: usize) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.write_races.insert(id.to_string(), times);
    }

    /// Test-only seeding: make [`ServiceManager::prepare`] fail, as launchd's
    /// does when no thread can be made to reap a request.
    pub fn seed_prepare_fails(&self) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.prepare_fails = true;
    }

    /// Every `start`/`stop`/`restart`/`kill` this Fake was asked to perform, in order,
    /// recorded when it was *asked* rather than when it succeeded.
    ///
    /// Exists because some assertions are about the **absence** of a call,
    /// which no state read can express: the fake's `start` on an
    /// already-`Running` entry is idempotent and leaves the state
    /// byte-identical, so "never called" and "called, changed nothing" are
    /// indistinguishable from the outside.
    /// `restart_does_not_start_after_a_stop_that_timed_out` is exactly that
    /// assertion.
    pub fn calls(&self) -> Vec<(&'static str, String)> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.calls.clone()
    }

    /// How many writes were attempted, the raced ones included.
    pub fn writes(&self) -> usize {
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.writes
    }

    /// The steps and budget every [`ServiceManager::prepare`] was given, in
    /// order — failed ones included.
    pub fn prepared(&self) -> Vec<(Vec<Step>, Budget)> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.prepared.clone()
    }

    /// Every `install`/`start`/`stop`/`restart`/`kill` this Fake was asked for, in
    /// order, each with the steps of every [`Prepared`] still held when it
    /// was asked: whether the preparation that should cover a request did —
    /// the right steps, not yet released.
    pub fn guarded(&self) -> Vec<(&'static str, String, Vec<Step>)> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.guarded.clone()
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

/// What a **stalled** entry's `start`/`stop` reports for `budget`, without
/// ever sleeping: the fake derives the expiry rather than living it.
///
/// The rule branches on the budget's *kind*, not on [`Budget::waits`], and
/// that distinction is the whole of it:
///
/// | budget | result |
/// |---|---|
/// | `Immediate`, and `Bounded(ZERO)` with it | `Ok(())`, state untouched — the honest model of "requested, not confirmed" |
/// | `Bounded(d)`, `d > 0` | [`budget::timed_out`] |
/// | `Unbounded` | **panic** |
///
/// `Unbounded` has to panic. `waits()` is true for it, so a rule phrased as
/// "a stalled entry under a waiting budget times out" routes it into
/// `budget::timed_out`, whose own `debug_assert!` requires a `Bounded`
/// budget — leaving the fake to either trip that assert or report an expiry
/// that provably cannot have happened. The honest model is "hang forever",
/// which is unusable in a test, and a fake that cannot honestly model a case
/// must refuse it rather than approximate it. Deterministic, loud, and
/// impossible to mistake for a backend behaviour.
fn stalled(id: &Id, awaited: &'static str, budget: Budget) -> Result<()> {
    match budget {
        Budget::Immediate | Budget::Bounded(Duration::ZERO) => Ok(()),
        Budget::Bounded(_) => Err(budget::timed_out(id.as_str(), awaited, budget)),
        Budget::Unbounded => panic!(
            "Fake: a stalled entry cannot be awaited under Budget::Unbounded — that wait has no \
             end. Use Budget::Bounded to exercise the expiry path."
        ),
    }
}

/// [`Fake::seed_start_in_doubt`]'s failure, for a start of `id`.
fn in_doubt(state: &Store, id: &Id) -> Result<()> {
    lost(&state.start_in_doubt, "start", id)
}

/// The failure a `seed_*_in_doubt` seeded in `ids`, for a `verb` of `id`.
fn lost(ids: &BTreeSet<String>, verb: &str, id: &Id) -> Result<()> {
    if ids.contains(id.as_str()) {
        return Err(Error::RequestInDoubt {
            request: format!("fake {verb} {id}"),
            reached: false,
            detail: "lost (seeded)".to_string(),
        });
    }
    Ok(())
}

/// What [`Fake`]'s `prepare` holds: its steps stay live until this drops.
struct Held {
    store: Arc<Mutex<Store>>,
    guard: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut state = self.store.lock().expect("Fake mutex poisoned");
        state.live.retain(|(guard, _)| *guard != self.guard);
    }
}

// Backend =============================================================================================================

impl Store {
    /// The entry at `id`. Absent only if it vanished since the ownership gate passed.
    fn entry_mut(&mut self, id: &Id) -> Result<&mut Entry> {
        self.entries.get_mut(id.as_str()).ok_or_else(|| Error::NotInstalled {
            id: id.as_str().to_string(),
        })
    }
}

impl Fake {
    /// The store, locked, for a verb that has passed the ownership gate ([`flow::owned`]) and has a
    /// manager to ask. Ownership errors beat [`Error::NoManager`]. Not reentrant: a verb records
    /// what it must, releases its guard, and only then calls this.
    fn gated(&self, id: &Id) -> Result<MutexGuard<'_, Store>> {
        flow::owned(self, id)?;
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.manager()?;
        Ok(state)
    }
}

impl crate::backend::Backend for Fake {
    type Definition = String;
    type Snapshot = Entry;
    type InstallScope = ();

    /// Records the install once, however many times the flow retries; then an opaque id, then no
    /// manager. Before `read`, which has no way to say a manager is missing.
    fn begin_install(&self, spec: &DaemonSpec) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.asked("install", &spec.id);
        if state.opaque.contains(spec.id.as_str()) {
            return Err(Store::undetermined(&spec.id));
        }
        state.manager()
    }

    fn render(&self, spec: &DaemonSpec) -> Result<String> {
        if let User::Name(user) = &spec.user {
            let state = self.state.lock().expect("Fake mutex poisoned");
            if state.unresolvable.contains(user) {
                return Err(Error::Other(format!("account `{user}` cannot be resolved (seeded)")));
            }
        }
        Ok(generate(spec))
    }

    fn read(&self, id: &Id) -> Result<ReadOutcome<Entry>> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.read(id)
    }

    /// The fake has no concept of external configuration (systemd's drop-in directory, or an
    /// analogous future backend's equivalent), so it is always absent.
    fn installed(&self, id: &Id, snapshot: &Entry) -> Result<InstalledDefinition<String>> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        if let Some(detail) = state.installed_fails.get(id.as_str()) {
            return Err(Error::Other(detail.clone()));
        }
        Ok(InstalledDefinition {
            definition: snapshot.text.clone(),
            external: ExternalConfig::Absent,
        })
    }

    /// Keeps the entry's enablement and run state across an update.
    fn write(
        &self,
        spec: &DaemonSpec,
        definition: &String,
        _outcome: &Outcome,
        snapshot: Option<&Entry>,
        _scope: &mut (),
    ) -> Result<WriteOutcome> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.writes += 1;
        if let Some(left) = state.write_races.get_mut(spec.id.as_str()) {
            if *left > 0 {
                *left -= 1;
                return Ok(WriteOutcome::Raced);
            }
        }
        let (enabled, run_state) = snapshot
            .map(|e| (e.enabled, e.state))
            .unwrap_or((false, State::Stopped));
        state.entries.insert(
            spec.id.as_str().to_string(),
            Entry {
                text: definition.clone(),
                enabled,
                state: run_state,
            },
        );
        Ok(WriteOutcome::Done)
    }

    fn live_status(&self, id: &Id, snapshot: &Entry) -> Result<Status> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        state.manager()?;
        if let Some(detail) = state.live_status_fails.get(id.as_str()) {
            return Err(Error::Other(detail.clone()));
        }
        Ok(Status {
            state: snapshot.state,
            pid: pid_for(snapshot.state),
            enabled: snapshot.enabled,
        })
    }

    /// Aggregates first, then the opaque ids last-to-first: `ServiceManager::list` promises no
    /// ordering at all, so a caller that renders this order instead of imposing its own must fail
    /// a test rather than pass by luck. An opaque id is never classified from the entry its failed
    /// read would have found.
    fn scan(&self) -> Scan<Entry> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        let aggregates = state.aggregates.iter().map(|reason| Installed::Undetermined {
            name: None,
            reason: reason.clone(),
        });
        let opaque = state.opaque.iter().rev().map(|name| Installed::Undetermined {
            name: Some(name.clone()),
            reason: OPAQUE_REASON.to_string(),
        });
        Scan {
            entries: state
                .entries
                .iter()
                .filter(|(name, _)| !state.opaque.contains(*name))
                .map(|(name, entry)| (name.clone(), Store::classify(entry)))
                .collect(),
            reported: aggregates.chain(opaque).collect(),
        }
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        self.gated(id)?.entries.remove(id.as_str());
        Ok(())
    }

    fn enable(&self, id: &Id) -> Result<()> {
        self.gated(id)?.entry_mut(id)?.enabled = true;
        Ok(())
    }

    fn disable(&self, id: &Id) -> Result<()> {
        self.gated(id)?.entry_mut(id)?.enabled = false;
        Ok(())
    }

    fn start(&self, id: &Id, budget: Budget) -> Result<()> {
        let stalls = {
            let mut state = self.state.lock().expect("Fake mutex poisoned");
            // Recorded on being *asked*, ahead of every check below: `calls`
            // answers "was this attempted", which is a different question from
            // whether it succeeded or changed anything.
            state.calls.push(("start", id.as_str().to_string()));
            state.asked("start", id);
            in_doubt(&state, id)?;
            state.start_stalls.contains(id.as_str())
        };
        let mut state = self.gated(id)?;
        let entry = state.entry_mut(id)?;
        if stalls {
            // No confirmation is coming, so none is invented: the state is
            // left exactly as it was on every branch `stalled` can take.
            return stalled(id, "running", budget);
        }
        // Request-only: no real backend establishes anything here, so
        // neither does the Fake — see `ServiceManager::start`.
        if budget.waits() {
            entry.state = State::Running;
        }
        Ok(())
    }

    /// Fails only once [`Fake::seed_prepare_fails`]ed. Makes nothing ready,
    /// but records `steps`, and holds them live for [`Fake::guarded`] until
    /// the guard drops.
    fn prepare(&self, steps: &[Step], budget: Budget) -> Result<Prepared> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        state.prepared.push((steps.to_vec(), budget));
        if state.prepare_fails {
            return Err(Error::Other(
                "nothing could be prepared (injected test failure)".to_string(),
            ));
        }
        let guard = state.next_guard;
        state.next_guard += 1;
        state.live.push((guard, steps.to_vec()));
        Ok(Prepared::holding(Held {
            store: Arc::clone(&self.state),
            guard,
        }))
    }

    /// Only once [`Fake::seed_native_restart`]ed. Request-only, so it settles
    /// nothing, as for `start`.
    fn request_restart(&self, id: &Id) -> Option<Result<()>> {
        {
            let mut state = self.state.lock().expect("Fake mutex poisoned");
            if !state.native_restart {
                return None;
            }
            state.calls.push(("restart", id.as_str().to_string()));
            state.asked("restart", id);
        }
        Some(self.gated(id).map(drop))
    }

    /// Models the strictest manager the contract allows, SCM's: a service
    /// still `Running` refuses the start, since the request-only `stop` before
    /// it settled nothing. Recorded as `start`, which is what it asks for.
    fn request_start_after_stop(&self, id: &Id) -> Result<()> {
        {
            let mut state = self.state.lock().expect("Fake mutex poisoned");
            state.calls.push(("start", id.as_str().to_string()));
            state.asked("start", id);
            in_doubt(&state, id)?;
        }
        let mut state = self.gated(id)?;
        if state.entry_mut(id)?.state == State::Running {
            return Err(Error::Other(format!(
                "start `{id}`: refused as already running — the stop issued before it has not taken effect"
            )));
        }
        Ok(())
    }

    fn stop(&self, id: &Id, budget: Budget) -> Result<()> {
        let stalls = {
            let mut state = self.state.lock().expect("Fake mutex poisoned");
            state.calls.push(("stop", id.as_str().to_string()));
            state.asked("stop", id);
            lost(&state.stop_in_doubt, "stop", id)?;
            state.stop_stalls.contains(id.as_str())
        };
        let mut state = self.gated(id)?;
        let entry = state.entry_mut(id)?;
        if stalls {
            return stalled(id, "stopped", budget);
        }
        // Idempotent: stopping an already-stopped (or failed, or unknown)
        // service is `Ok(())` — see `ServiceManager::stop`'s doc comment for
        // why every backend must agree on this. Request-only settles nothing,
        // as for `start`: the Fake models the contract's minimum, not
        // launchd's blocking `bootout`.
        if budget.waits() {
            entry.state = State::Stopped;
        }
        Ok(())
    }

    /// A running entry becomes [`State::Failed`], with no pid: the Fake models the kill and nothing
    /// of the restart policy that follows it. Any other state has no process to end.
    fn kill(&self, id: &Id) -> Result<()> {
        {
            let mut state = self.state.lock().expect("Fake mutex poisoned");
            state.calls.push(("kill", id.as_str().to_string()));
            state.asked("kill", id);
        }
        let mut state = self.gated(id)?;
        let entry = state.entry_mut(id)?;
        if entry.state != State::Running {
            return Err(Error::NotRunning {
                id: id.as_str().to_string(),
            });
        }
        entry.state = State::Failed;
        Ok(())
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod fake_tests;
