//! Unit tests for `manager.rs`'s private items — the TOCTOU windows and the drop-in/stale
//! interaction that cannot be tested through the public [`crate::manager::ServiceManager`] trait
//! alone. Reaching some of these needs direct control over `raw_state`/`create_unit`/
//! `quarantine_if_still_ours`, with a foreign write injected between classification and write —
//! something no interleaving of `install` calls alone can force deterministically.
//!
//! Elevated (writes real files under `/etc/systemd/system`), so it opts into the same `elevated`
//! precondition/label convention `tests/support/mod.rs` uses, duplicated locally rather than shared:
//! a plain library unit test cannot depend on the `tests/` integration-test support crate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// stop looping, not that its in-flight `fs::write` (itself a truncate-then-write, not atomic from an
/// external reader's perspective) has finished — reading before the join is its own, separate race.
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
                let _ = std::fs::write(&path, foreign_text);
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

// apply_dropin_override: obligation 3 (the Stale branch decide() cannot see) ==========================================

#[skuld::test]
fn apply_dropin_override_converts_stale_to_conflict_when_not_forced() {
    let discovery = Discovery {
        ownership: Ownership::Absent,
        on_disk: Some("on-disk-with-dropin".to_string()),
        fragment_text: None,
        dropin_present: true,
    };
    let stale = Outcome::Stale {
        from_version: "0.0.1".to_string(),
    };
    let result = apply_dropin_override(stale, &discovery, false);
    assert!(
        matches!(result, Outcome::Conflict { .. }),
        "expected Conflict, got {result:?}"
    );
}

#[skuld::test]
fn apply_dropin_override_lets_force_regenerate_through_stale() {
    let discovery = Discovery {
        ownership: Ownership::Absent,
        on_disk: Some("on-disk-with-dropin".to_string()),
        fragment_text: None,
        dropin_present: true,
    };
    let stale = Outcome::Stale {
        from_version: "0.0.1".to_string(),
    };
    let result = apply_dropin_override(stale, &discovery, true);
    assert!(
        matches!(result, Outcome::Stale { .. }),
        "force must let Stale through unchanged, so install still regenerates and clears the \
         drop-in, got {result:?}"
    );
}

#[skuld::test]
fn apply_dropin_override_is_a_noop_without_a_dropin() {
    let discovery = Discovery {
        ownership: Ownership::Absent,
        on_disk: None,
        fragment_text: None,
        dropin_present: false,
    };
    let result = apply_dropin_override(Outcome::Create, &discovery, false);
    assert!(matches!(result, Outcome::Create), "expected Create, got {result:?}");
}
