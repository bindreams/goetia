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
    running: bool,
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
                running: false,
            },
        );
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

// ServiceManager ======================================================================================================

impl ServiceManager for Fake {
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let desired = generate(spec);
        let existing = state.get(spec.id.as_str()).cloned();

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
        let on_disk = existing.as_ref().map(|e| e.text.as_str());

        let outcome = decide::decide(&found, on_disk, &desired, spec, crate::version(), force);

        // `Create`/`Update`/`Stale` are the outcomes `decide` recommends
        // actually writing for; every refusing variant (`Conflict` without
        // force, `RefuseForeign`, `RefuseUnreadable`) and `UpToDate` leave
        // the store untouched.
        if matches!(
            outcome,
            Outcome::Create | Outcome::Update { .. } | Outcome::Stale { .. }
        ) {
            let enabled = existing.as_ref().map(|e| e.enabled).unwrap_or(false);
            let running = existing.as_ref().map(|e| e.running).unwrap_or(false);
            state.insert(
                spec.id.as_str().to_string(),
                Entry {
                    text: desired,
                    enabled,
                    running,
                },
            );
        }

        Ok(outcome)
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        if state.remove(id.as_str()).is_none() {
            return Err(not_installed(id));
        }
        Ok(())
    }

    fn enable(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        entry.enabled = true;
        Ok(())
    }

    fn disable(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        entry.enabled = false;
        Ok(())
    }

    fn start(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        entry.running = true;
        Ok(())
    }

    fn stop(&self, id: &Id) -> Result<()> {
        let mut state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get_mut(id.as_str()).ok_or_else(|| not_installed(id))?;
        entry.running = false;
        Ok(())
    }

    fn status(&self, id: &Id) -> Result<Status> {
        let state = self.state.lock().expect("Fake mutex poisoned");
        let entry = state.get(id.as_str()).ok_or_else(|| not_installed(id))?;
        Ok(Status {
            state: if entry.running { State::Running } else { State::Stopped },
            pid: if entry.running { Some(FAKE_PID) } else { None },
            enabled: entry.enabled,
        })
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
                    state: if entry.running { State::Running } else { State::Stopped },
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
