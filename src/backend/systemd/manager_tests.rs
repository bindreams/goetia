//! Unit tests for `manager.rs`'s private items — the TOCTOU windows and the drop-in/stale
//! interaction that cannot be tested through the public [`crate::manager::ServiceManager`] trait
//! alone. Reaching some of these needs direct control over `raw_state`/`create_unit`/
//! `quarantine_if_still_ours`, with a foreign write injected between classification and write —
//! something no interleaving of `install` calls alone can force deterministically.
//!
//! Most of these are elevated (they write real files under `/etc/systemd/system`), so they join
//! the `systemd_group` fixture of `crate::testing::groups` and the `ELEVATED` label.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicBool, Ordering};

use super::dirs::ensure_dir;
use super::discover::{RawState, raw_state};
use super::systemctl::systemctl_tests::{booted, gate_then};
use super::write::{CreateOutcome, ReplaceOutcome, create_unit, quarantine_if_still_ours, replace_unit_verified};
use super::*;
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::{bounded, standby};
use crate::manager::ServiceManager as _;
use crate::manager::conformance::fresh_id;
use crate::spec::DaemonFields;
use crate::spec::Kind;
use crate::testing::plain_fields;

use crate::testing::cleanup::{DirGuard, FileGuard, remove_or_warn};
use crate::testing::elevation::ELEVATED;
use crate::testing::groups::{Group, systemd_group};

/// On drop, removes every `unique_quarantine_path(id)` file the test's scenario left behind.
/// Production deliberately leaves one on an unrecoverable double-race so a human can find it, and a
/// failed assertion can strand the backup of a test that restores it. Register it before the call
/// that can create one.
struct QuarantineSweep(String);

impl QuarantineSweep {
    fn new(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl Drop for QuarantineSweep {
    fn drop(&mut self) {
        let entries = match std::fs::read_dir(UNIT_DIR) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!(
                    "warning: left behind: list {UNIT_DIR} for the quarantine files of {}: {e}",
                    self.0
                );
                return;
            }
        };
        let prefix = format!(".{}.service.goetia-quarantine", self.0);
        for entry in entries {
            match entry {
                Ok(entry) if entry.file_name().to_string_lossy().starts_with(&prefix) => {
                    remove_or_warn(&entry.path(), false);
                }
                Ok(_) => {}
                Err(e) => eprintln!(
                    "warning: left behind: read {UNIT_DIR} for the quarantine files of {}: {e}",
                    self.0
                ),
            }
        }
    }
}

/// A manager over `fake`, on a booted host: nothing here runs a real `systemctl`.
fn over(fake: Fake) -> Systemd {
    Systemd::with(Systemctl::new(Box::new(fake), booted))
}

/// Tells a racer thread to stop when dropped, so a panic in the call it races (the fake refusing an
/// unstated call) ends the scope instead of leaving the racer looping.
struct StopRacer<'a>(&'a AtomicBool);

impl Drop for StopRacer<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn mk(id: &str) -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        kind: Kind::Simple,
        ..plain_fields(id, vec!["/bin/true".to_string()])
    })
    .unwrap()
}

// create_unit: a create never clobbers ================================================================================

#[skuld::test(labels = [ELEVATED])]
fn create_does_not_clobber_a_unit_that_appears_after_classification(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());

    // "Classification" (what `install`'s `read` does) sees nothing here yet.
    assert!(matches!(raw_state(&id).expect("raw_state"), RawState::Absent));

    // Something else — a package postinst, or a concurrent invocation — drops a foreign unit into
    // exactly the window `install`'s classify-then-write sequence leaves open.
    let foreign_text = "[Unit]\nDescription=not goetia, appeared mid-race\n";
    std::fs::write(&path, foreign_text).expect("write foreign unit");

    // `create_unit` must detect this rather than clobber it: a plain `rename(2)` would
    // unconditionally replace whatever is now at `path`, destroying a file the classification step
    // never saw.
    let outcome = create_unit(&id, "[Unit]\nDescription=goetia\n").expect("create_unit");
    assert!(
        matches!(outcome, CreateOutcome::Raced),
        "create_unit must report the race, not clobber it"
    );

    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(
        on_disk, foreign_text,
        "the foreign file must survive create_unit untouched"
    );
}

// quarantine_if_still_ours: an update never clobbers ==================================================================

#[skuld::test(labels = [ELEVATED])]
fn quarantine_detects_a_swap_that_happened_after_classification(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    let _sweep = QuarantineSweep::new(&id);

    let original_text = "[Unit]\nDescription=goetia original\n";
    std::fs::write(&path, original_text).expect("write original");

    // Something replaces the exact file that was classified, before `replace_unit_verified` would
    // have written to it.
    std::fs::remove_file(&path).expect("remove original");
    let swapped_text = "[Unit]\nDescription=swapped in after classification\n";
    std::fs::write(&path, swapped_text).expect("write swapped-in file");

    let result = quarantine_if_still_ours(&id, original_text).expect("quarantine_if_still_ours");
    assert!(
        result.is_none(),
        "a swapped-in file must be reported as a race, not quarantined"
    );

    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, swapped_text, "the swapped-in file must survive untouched");
}

#[skuld::test(labels = [ELEVATED])]
fn quarantine_succeeds_when_nothing_raced(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    let _sweep = QuarantineSweep::new(&id);

    let text = "[Unit]\nDescription=goetia unraced\n";
    std::fs::write(&path, text).expect("write");

    let backup = quarantine_if_still_ours(&id, text)
        .expect("quarantine_if_still_ours")
        .expect("nothing raced, so the file must be quarantined");
    assert!(!path.exists(), "the original name must be free while quarantined");
    let quarantined_text = std::fs::read_to_string(&backup).expect("read quarantined file");
    assert_eq!(quarantined_text, text);

    // Restore the backup: part of what this checks, and `FileGuard` only knows `path`.
    std::fs::rename(&backup, &path).expect("restore for cleanup");
}

// install(): the Raced retry loop end-to-end ==========================================================================

/// Closes the one thing the primitive-level tests above cannot: that `Systemd::install`'s own
/// `CreateOutcome::Raced => continue` loop (not `create_unit` in isolation) turns a detected race
/// into the right final `Outcome` for the caller, rather than looping forever, discarding the race,
/// or mis-classifying the post-race state. Real OS threads racing real filesystem calls, gated only
/// by a shared flag `install` itself flips when it returns — no sleep, no fixed iteration count, no
/// timeout: whichever side reaches the target path first, the assertions below hold for both
/// possible legitimate outcomes, so this cannot flake even on a run where the race is never actually
/// hit (repeated CI runs across many changes are what gives the race a chance to land at least once).
/// The racer thread is explicitly joined before any assertion reads the file: `done` only tells it to
/// stop looping, not that its in-flight attempt has finished — reading before the join is its own,
/// separate race. The racer uses `create_new` rather than a plain `write`, deliberately: a plain write
/// would truncate-then-write even after `install` has already committed its own content, clobbering a
/// legitimate `Create` result the racer arrived too late to actually contest — `create_new` only ever
/// succeeds once, against whichever of the two genuinely got to the empty path first, which is the
/// only interleaving a non-clobbering write is about.
#[skuld::test(labels = [ELEVATED])]
fn install_never_corrupts_state_when_something_races_the_first_create(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());

    let done = AtomicBool::new(false);
    let foreign_text = "[Unit]\nDescription=raced in\n";
    let spec = mk(&id);

    std::thread::scope(|scope| {
        let racer = scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                    let _ = f.write_all(foreign_text.as_bytes());
                }
            }
        });

        // Every install asks the version gate's probes; only one that wrote asks `daemon-reload`, so a
        // raced one leaves that reply unused.
        let mgr = over(gate_then([reply(&["daemon-reload"], exited(0, "", ""))]));
        let stop = StopRacer(&done);
        let mut held = Backend::reserve(&mgr, &[Step::Install], Budget::Unbounded).expect("reserve");
        let outcome = flow::install(&mgr, &spec, false, &mut held);
        drop(stop);
        racer.join().expect("racer thread panicked");

        let outcome = outcome.expect("install must not error, only refuse or succeed");
        match outcome {
            Outcome::Create => {
                let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(
                    on_disk.contains("X-Goetia"),
                    "install reported Create but on-disk content is not its own: {on_disk}"
                );
            }
            Outcome::RefuseForeign { .. } => {
                let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
                assert_eq!(
                    on_disk, foreign_text,
                    "the racer's file must survive install's refusal untouched"
                );
            }
            other => panic!("a raced install must Create or RefuseForeign, got {other:?}"),
        }
    });
}

// ensure_dir: parent directories ======================================================================================

#[skuld::test(labels = [ELEVATED])]
fn ensure_dir_leaves_a_preexisting_directory_untouched(#[fixture(systemd_group)] _group: &Group) {
    let dir = std::env::temp_dir().join(format!("{}-preexisting", fresh_id("systemd")));
    std::fs::create_dir(&dir).expect("create pre-existing dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let _cleanup = DirGuard::new(dir.clone());

    ensure_dir(&dir, &User::Root).expect("ensure_dir on a pre-existing dir");

    let meta = std::fs::metadata(&dir).expect("stat");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "a pre-existing directory's mode must not be widened"
    );
}

#[skuld::test(labels = [ELEVATED])]
fn ensure_dir_accepts_a_preexisting_directory_the_account_cannot_write(#[fixture(systemd_group)] _group: &Group) {
    use std::os::unix::fs::MetadataExt as _;

    let dir = std::env::temp_dir().join(format!("{}-unwritable", fresh_id("systemd")));
    std::fs::create_dir(&dir).expect("create pre-existing dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let _cleanup = DirGuard::new(dir.clone());

    ensure_dir(&dir, &User::Name("nobody".to_string())).expect("ensure_dir");

    let meta = std::fs::metadata(&dir).expect("stat");
    assert_eq!(meta.permissions().mode() & 0o777, 0o700, "mode must be untouched");
    assert_eq!(meta.uid(), 0, "owner must be untouched");
}

#[skuld::test(labels = [ELEVATED])]
fn ensure_dir_refuses_a_preexisting_file(#[fixture(systemd_group)] _group: &Group) {
    let path = std::env::temp_dir().join(format!("{}-a-file", fresh_id("systemd")));
    std::fs::write(&path, b"").expect("create file");
    let _cleanup = FileGuard::new(path.clone());

    let err = ensure_dir(&path, &User::Root).expect_err("a file cannot be a directory");
    assert!(err.to_string().contains("not a directory"), "{err}");
}

// replace_unit_verified / quarantine_if_still_ours: the remaining race branches =======================================

/// The one non-clobber branch none of the other tests reach: a *third* file appearing at
/// `unit_path(id)` in the moment between `quarantine_if_still_ours` emptying it and
/// `replace_unit_verified`'s own write claiming it. Real threads racing real filesystem calls, same
/// join discipline as `install_never_corrupts_state_when_something_races_the_first_create`.
#[skuld::test(labels = [ELEVATED])]
fn replace_reports_raced_twice_rather_than_guessing_which_file_wins(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    let _sweep = QuarantineSweep::new(&id);

    let original_text = "[Unit]\nDescription=goetia original\n";
    std::fs::write(&path, original_text).expect("write original");

    let done = AtomicBool::new(false);
    let intruder_text = "[Unit]\nDescription=raced in during replace\n";

    std::thread::scope(|scope| {
        let racer = scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                    let _ = f.write_all(intruder_text.as_bytes());
                }
            }
        });

        let result = replace_unit_verified(&id, "[Unit]\nDescription=goetia replacement\n", original_text);
        done.store(true, Ordering::Release);
        racer.join().expect("racer thread panicked");

        match result {
            Ok(ReplaceOutcome::Replaced) => {
                // The racer never got a chance to land between the quarantine and the write: a
                // completely ordinary `Replaced` is a legitimate outcome of this race, not a bug.
            }
            Ok(ReplaceOutcome::Raced) => panic!(
                "replace_unit_verified reported Raced, but nothing changed the fragment before its \
                 own quarantine step ran — that is `quarantine_if_still_ours`'s race, not this test's"
            ),
            Err(e) => {
                let message = e.to_string();
                assert!(matches!(e, Error::QuarantineStranded { .. }), "{e:?}");
                assert!(message.contains("quarantined"), "{message}");
                let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
                assert_eq!(
                    on_disk, intruder_text,
                    "the intruder's file must survive the reported race untouched"
                );
            }
        }
    });
}

/// `uninstall`'s own instance of the non-clobber race: the fragment can change between `read` and
/// the removal, since two full `systemctl` round-trips run in between. Unlike the `install` paths,
/// this needs no live daemon lifecycle, so it is exercised directly at the
/// `quarantine_if_still_ours` level rather than through a real `stop`/`disable` cycle.
#[skuld::test(labels = [ELEVATED])]
fn quarantine_used_by_uninstall_reports_a_race_instead_of_removing_the_wrong_file(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    let _sweep = QuarantineSweep::new(&id);

    let classified_text = "[Unit]\nDescription=goetia, as read saw it\n";
    std::fs::write(&path, classified_text).expect("write");

    // Something changes the fragment after `read` saw it, before
    // `uninstall` reaches its own `quarantine_if_still_ours` call.
    let changed_text = "[Unit]\nDescription=changed after classification\n";
    std::fs::write(&path, changed_text).expect("overwrite");

    let result = quarantine_if_still_ours(&id, classified_text).expect("quarantine_if_still_ours");
    assert!(
        result.is_none(),
        "uninstall's removal must not proceed against a fragment that changed after classification"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, changed_text, "the changed fragment must survive untouched");
}

// Enumeration: a scan that did not finish =============================================================================

/// A unit directory that will not open is reported, not propagated (see `flow::scan`): an `Err` here
/// reaches the CLI as `Kind::Unavailable` — exit `1` over an empty document, which is `list`
/// reporting a populated host as having nothing on it.
#[skuld::test]
fn a_unit_directory_that_cannot_be_opened_is_reported_not_propagated() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blocking_file = tmp.path().join("not-a-directory");
    std::fs::write(&blocking_file, "").expect("write the blocking file");
    // `ENOTDIR`: not `NotFound`, and identical for root and everyone else — which is what makes it
    // the right probe in a binary CI runs elevated.
    let dir = blocking_file.join("system");

    let scan = scan_unit_dir(&dir);

    assert!(scan.units.is_empty(), "nothing was enumerated: {scan:?}");
    match &scan.incomplete {
        Some(Installed::Undetermined { name: None, reason }) => {
            let reason = reason.to_string();
            assert!(reason.contains("failed to read directory: "), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

/// The case that is deliberately *not* this one: an absent unit directory establishes that nothing
/// is installed. A determinate answer is reported as one — an empty listing on exit `0`, never an
/// `Err` that renders as "goetia could not answer".
#[skuld::test]
fn an_absent_unit_directory_is_an_empty_listing_not_a_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let scan = scan_unit_dir(&tmp.path().join("nowhere"));

    assert!(scan.units.is_empty(), "{scan:?}");
    assert!(
        scan.incomplete.is_none(),
        "an absent directory answers the question rather than leaving it open: {scan:?}"
    );
}

#[skuld::test]
fn a_pass_that_finishes_names_every_fragment_and_drop_in_and_nothing_else() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("kept.service"), "").expect("write the fragment");
    std::fs::write(tmp.path().join("ignored.conf"), "").expect("write the non-fragment");
    std::fs::create_dir(tmp.path().join("kept.service.d")).expect("create the drop-in directory");

    let scan = scan_unit_dir(tmp.path());

    assert_eq!(
        scan.units.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["kept"],
        "{scan:?}"
    );
    assert_eq!(scan.dropins, ["kept"], "{scan:?}");
    assert!(
        scan.incomplete.is_none(),
        "a pass that finished has nothing to report: {scan:?}"
    );
}

/// The name is what the pass reports, and nothing about it is stat'd: a *regular file* named
/// `<id>.service.d` is the shape `read_dir` answers `ENOTDIR` for under every uid, root's included,
/// and it is precisely the id `list` used to drop. A pass that classified the name by type would
/// leave it out again at exactly the privilege level where the state is real.
#[skuld::test]
fn a_drop_in_name_that_is_not_a_directory_still_names_its_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("blocked.service.d"), "").expect("write the blocking file");

    let scan = scan_unit_dir(tmp.path());

    assert!(scan.units.is_empty(), "there is no fragment here: {scan:?}");
    assert_eq!(scan.dropins, ["blocked"], "{scan:?}");
}

// reserve =============================================================================================================

/// The announcement `systemctl --show-transaction` makes of the job it enqueued for `verb`.
fn job(verb: &str) -> Entry {
    reply(
        &[verb],
        exited(0, "", &format!("Enqueued anchor job 1 x.service/{verb}.\n")),
    )
}

/// Under a bounded budget `restart`'s legs each run one watched `systemctl`, and `reserve` makes
/// what every one of them needs before either sends anything: the threads each leg reads its
/// streams with come from the reservation it is handed, not from anywhere made later, and one
/// version gate serves both. A reservation holds no more than its steps watch.
#[skuld::test]
fn a_reserved_sequence_needs_nothing_made_later() {
    let fake = gate_then([job("stop"), job("start")]);
    let mgr = over(fake.clone());
    let mut held = Backend::reserve(&mgr, &[Step::Stop, Step::Start], Budget::DEFAULT).expect("reserve");
    let _no_thread = standby::test_hook::threads(0);
    let _no_file = bounded::test_hook::temp_files(0);

    mgr.systemctl
        .stop_impl("x", Budget::DEFAULT, Budget::DEFAULT.start(), &mut held)
        .expect("its threads were reserved");
    mgr.systemctl
        .start_impl("x", Budget::DEFAULT, Budget::DEFAULT.start(), &mut held)
        .expect("its threads were reserved");

    assert_eq!(
        bounded::test_hook::listeners_left(&held.draws),
        0,
        "it reserved more than it drew"
    );
    fake.assert_spent("the sequence");
    assert_eq!(
        fake.calls().len(),
        4,
        "the gate's pair of probes, then the two legs: {:?}",
        fake.calls()
    );
}

/// `install --enable --start` is one sequence over one reservation, so install, `enable` and `start`
/// share one pair of version probes: the gate is asked once for the three.
#[skuld::test(labels = [ELEVATED])]
fn install_enable_and_start_in_one_sequence_probe_the_gate_once(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _cleanup = FileGuard::new(unit_path(&id));
    let spec = mk(&id);
    let fake = gate_then([
        reply(&["daemon-reload"], exited(0, "", "")),
        reply(&["enable"], exited(0, "", "")),
        job("start"),
    ]);
    let mgr = over(fake.clone());

    let mut sequence = mgr
        .prepare(&[Step::Install, Step::Start], Budget::DEFAULT)
        .expect("prepare");
    sequence.install(&spec, false).expect("install");
    sequence.enable(&spec.id).expect("enable");
    sequence.start(&spec.id, Budget::DEFAULT).expect("start");

    fake.assert_spent("the sequence");
    let probes = fake
        .calls()
        .iter()
        .filter(|call| call[0] == "--version" || call.contains(&"--property=Version".to_string()))
        .count();
    assert_eq!(probes, 2, "the gate's pair of probes ran once: {:?}", fake.calls());
}

/// A verb whose threads cannot be made is refused before it sends anything, as `Error::Reservation`:
/// the stated refusal for a host short of tasks. The reservation fails as a whole, so no leg of a
/// sequence goes out with the next one unable to follow.
#[skuld::test]
fn a_verb_short_of_threads_is_refused_before_it_sends_anything() {
    let fake = Fake::new([]);
    let mgr = over(fake.clone());
    for (steps, threads) in [
        (&[Step::Start][..], 0),
        (&[Step::Start], 1),
        (&[Step::Stop, Step::Start], 1),
        (&[Step::Stop, Step::Start], 3),
    ] {
        let _short = standby::test_hook::threads(threads);

        let e = Backend::reserve(&mgr, steps, Budget::DEFAULT)
            .err()
            .unwrap_or_else(|| panic!("{steps:?} with {threads} threads"));

        assert!(matches!(e, Error::Reservation { .. }), "{steps:?}: {e:?}");
        assert!(e.to_string().contains("so none was sent"), "{steps:?}: {e}");
        assert!(!e.is_indeterminate(), "{steps:?}: {e:?}");
    }
    assert!(fake.calls().is_empty(), "something was sent: {:?}", fake.calls());
}

/// A budget that does not wait, or never stops waiting, watches no `systemctl`, and neither do the
/// steps that run to completion, so nothing is made for them: a verb that makes no thread must not
/// be refused for want of one.
#[skuld::test]
fn a_step_that_watches_nothing_reserves_nothing() {
    let _no_thread = standby::test_hook::threads(0);
    let _no_file = bounded::test_hook::temp_files(0);
    for budget in [Budget::Immediate, Budget::Unbounded] {
        assert!(
            Backend::reserve(&over(Fake::new([])), &[Step::Stop, Step::Start], budget).is_ok(),
            "{budget:?}"
        );
    }
    for step in [Step::Install, Step::Uninstall, Step::Kill, Step::RequestStartAfterStop] {
        assert!(
            Backend::reserve(&over(Fake::new([])), &[step], Budget::DEFAULT).is_ok(),
            "{step:?}"
        );
    }
}
