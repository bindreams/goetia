//! [`Fake`]: an in-memory [`ServiceManager`] with no real I/O.
//!
//! Every CLI test in this crate runs against `Fake`, never a real backend —
//! see `manager.rs`'s module doc comment. `Fake` still routes every
//! `install` through [`crate::decide::decide`], exactly as a real backend
//! must: its own tiny `generate`/`extract` pair below (not any real
//! systemd/launchd/SCM format) exists only so `decide` has something to
//! compare, the same role each real backend's own generator plays.

use std::collections::BTreeMap;
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
    state: Arc<Mutex<BTreeMap<String, Entry>>>,
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
        state.insert(
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

    /// Test-only seeding: install `spec` normally, then append `extra_line`
    /// to the stored artifact so it no longer matches what regenerating its
    /// own embedded spec would produce — simulating a hand-edit made
    /// outside Goetia. See `manager::conformance`'s module doc comment.
    pub fn install_then_hand_edit(&self, spec: &DaemonSpec, extra_line: &str) {
        self.install(spec, false).expect("seed install must succeed");
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state
            .get_mut(spec.id.as_str())
            .expect("just installed by the line above");
        entry.text.push_str(extra_line);
        if !entry.text.ends_with('\n') {
            entry.text.push('\n');
        }
    }

    /// Test-only: force `id`'s reported [`State`] directly, bypassing
    /// `start`/`stop` (which can only produce `Running`/`Stopped`). `id`
    /// must already be installed.
    pub fn seed_state(&self, id: &str, new_state: State) {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state
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

fn not_installed(id: &Id) -> Error {
    Error::NotInstalled {
        id: id.as_str().to_string(),
    }
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
fn discover(state: &BTreeMap<String, Entry>, id: &str) -> (Ownership, Option<String>) {
    let existing = state.get(id).cloned();
    let found = match &existing {
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
        let desired = generate(spec);
        let (found, on_disk) = discover(&state, spec.id.as_str());

        let outcome = decide::decide(&found, on_disk.as_deref(), &desired, spec, crate::version(), force);

        // `Create`/`Update`/`Stale` are the outcomes `decide` recommends
        // actually writing for; every refusing variant (`Conflict` without
        // force, `RefuseForeign`, `RefuseUnreadable`) and `UpToDate` leave
        // the store untouched.
        if matches!(
            outcome,
            Outcome::Create | Outcome::Update { .. } | Outcome::Stale { .. }
        ) {
            let (enabled, run_state) = state
                .get(spec.id.as_str())
                .map(|e| (e.enabled, e.state))
                .unwrap_or((false, State::Stopped));
            state.insert(
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
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get(id.as_str()).ok_or_else(|| not_installed(id))?;
        require_ours(entry, id)?;
        state.remove(id.as_str());
        Ok(())
    }

    fn enable(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        require_ours(entry, id)?;
        entry.enabled = true;
        Ok(())
    }

    fn disable(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        require_ours(entry, id)?;
        entry.enabled = false;
        Ok(())
    }

    fn start(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        require_ours(entry, id)?;
        entry.state = State::Running;
        Ok(())
    }

    fn stop(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        require_ours(entry, id)?;
        // Idempotent: stopping an already-stopped (or failed, or unknown)
        // service is `Ok(())` — see `ServiceManager::stop`'s doc comment for
        // why every backend must agree on this.
        entry.state = State::Stopped;
        Ok(())
    }

    fn status(&self, id: &Id) -> Result<Status> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get(id.as_str()).ok_or_else(|| not_installed(id))?;
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
                pid: if entry.state == State::Running {
                    Some(FAKE_PID)
                } else {
                    None
                },
                enabled: entry.enabled,
            }),
        }
    }

    fn list(&self) -> Result<Vec<Installed>> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        let mut out = Vec::new();
        for (name, entry) in state.iter() {
            match extract(&entry.text) {
                // A foreign entry is not Goetia-managed at all: `list`
                // reports only what Goetia owns, per the trait doc comment.
                Ok(None) => {}
                Ok(Some(blob)) => out.push(Installed::Ours {
                    spec: blob.spec,
                    state: entry.state,
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
