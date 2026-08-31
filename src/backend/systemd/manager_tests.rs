//! Unit tests for `manager.rs`'s private items — the TOCTOU windows and the drop-in/stale
//! interaction that cannot be tested through the public [`crate::manager::ServiceManager`] trait
//! alone. Reaching some of these needs direct control over `raw_state`/`create_unit`/
//! `quarantine_if_still_ours`, with a foreign write injected between classification and write —
//! something no interleaving of `install` calls alone can force deterministically.
//!
//! Elevated (writes real files under `/etc/systemd/system`), so it opts into the same `elevated`
//! precondition/label convention `tests/support/mod.rs` uses, duplicated locally rather than shared:
//! a plain library unit test cannot depend on the `tests/` integration-test support crate.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use super::dirs::ensure_writable_dir;
use super::discover::{RawState, raw_state};
use super::write::{CreateOutcome, ReplaceOutcome, create_unit, quarantine_if_still_ours, replace_unit_verified};
use super::*;
use crate::spec::{Kind, Restart};

#[skuld::label]
const ELEVATED: skuld::Label;

fn elevated() -> std::result::Result<(), String> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err("writes real files under /etc/systemd/system; re-run under sudo".to_string())
    }
}

/// RAII cleanup so a failed assertion still removes the file it wrote.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Removes any `unique_quarantine_path(id)` file left behind — production deliberately leaves one on
/// an unrecoverable double-race so a human can find it, but a test that engineered that exact
/// scenario on purpose doesn't need the evidence kept around afterward.
fn sweep_quarantine_files(id: &str) {
    let Ok(entries) = std::fs::read_dir(UNIT_DIR) else {
        return;
    };
    let prefix = format!(".{id}.service.goetia-quarantine");
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn mk(id: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id.to_string()).expect("valid id"),
        name: id.to_string(),
        command: vec!["/bin/true".to_string()],
        cwd: None,
        env: Default::default(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

// create_unit: obligation 1 (create case) =============================================================================

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn create_does_not_clobber_a_unit_that_appears_after_classification() {
    let id = test_id();
    let path = unit_path(&id);
    let _cleanup = Cleanup(path.clone());

    // "Classification" (what `install`'s `discover` does) sees nothing here yet.
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

// quarantine_if_still_ours: obligation 1 (update/stale case) ==========================================================

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn quarantine_detects_a_swap_that_happened_after_classification() {
    let id = test_id();
    let path = unit_path(&id);
    let _cleanup = Cleanup(path.clone());

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

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn quarantine_succeeds_when_nothing_raced() {
    let id = test_id();
    let path = unit_path(&id);
    let _cleanup = Cleanup(path.clone());

    let text = "[Unit]\nDescription=goetia unraced\n";
    std::fs::write(&path, text).expect("write");

    let backup = quarantine_if_still_ours(&id, text)
        .expect("quarantine_if_still_ours")
        .expect("nothing raced, so the file must be quarantined");
    assert!(!path.exists(), "the original name must be free while quarantined");
    let quarantined_text = std::fs::read_to_string(&backup).expect("read quarantined file");
    assert_eq!(quarantined_text, text);

    // Restore, so `Cleanup` (which only knows `path`) still cleans up correctly.
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
/// only interleaving obligation 1 is about.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn install_never_corrupts_state_when_something_races_the_first_create() {
    let id = test_id();
    let path = unit_path(&id);
    let _cleanup = Cleanup(path.clone());

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

        let mgr = Systemd::new();
        let outcome = mgr.install(&spec, false);
        done.store(true, Ordering::Release);
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

// ensure_writable_dir: obligation 5 ===================================================================================

#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn ensure_writable_dir_leaves_a_preexisting_directory_untouched() {
    let dir = std::env::temp_dir().join(format!("{}-preexisting", test_id()));
    std::fs::create_dir(&dir).expect("create pre-existing dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    struct RmDir(PathBuf);
    impl Drop for RmDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RmDir(dir.clone());

    // `User::Root` skips the writability check entirely (see `verify_writable_by`), so this proves
    // only the mode/ownership side; a non-root account would additionally need a real account to
    // `chown` to and is covered at the integration level instead.
    ensure_writable_dir(&dir, &User::Root).expect("ensure_writable_dir on a pre-existing dir");

    let meta = std::fs::metadata(&dir).expect("stat");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "a pre-existing directory's mode must not be widened"
    );
}

// replace_unit_verified / quarantine_if_still_ours: the remaining race branches =======================================

/// The one obligation-1 branch none of the other tests reach: a *third* file appearing at
/// `unit_path(id)` in the moment between `quarantine_if_still_ours` emptying it and
/// `replace_unit_verified`'s own write claiming it. Real threads racing real filesystem calls, same
/// join discipline as `install_never_corrupts_state_when_something_races_the_first_create`.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn replace_reports_raced_twice_rather_than_guessing_which_file_wins() {
    let id = test_id();
    let path = unit_path(&id);
    let _cleanup = Cleanup(path.clone());

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
                assert!(message.contains("raced twice"), "{message}");
                assert!(message.contains("quarantined"), "{message}");
                let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
                assert_eq!(
                    on_disk, intruder_text,
                    "the intruder's file must survive the reported race untouched"
                );
                // Production deliberately leaves the quarantine file behind here for a human to
                // recover — see the error message this asserts above — but this test controlled the
                // whole scenario and doesn't need it kept around.
                sweep_quarantine_files(&id);
            }
        }
    });
}

/// `uninstall`'s own instance of obligation 1: the fragment can change between `require_installed`'s
/// classification and the removal, since two full `systemctl` round-trips run in between. Unlike the
/// `install` paths, this needs no live daemon lifecycle, so it is exercised directly at the
/// `quarantine_if_still_ours` level rather than through a real `stop`/`disable` cycle.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn quarantine_used_by_uninstall_reports_a_race_instead_of_removing_the_wrong_file() {
    let id = test_id();
    let path = unit_path(&id);
    let _cleanup = Cleanup(path.clone());

    let classified_text = "[Unit]\nDescription=goetia, as require_installed saw it\n";
    std::fs::write(&path, classified_text).expect("write");

    // Something changes the fragment after `require_installed` would have read it, before
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
