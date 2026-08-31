//! Unit tests for `manager.rs`'s private items — specifically the one obligation that cannot be
//! tested through the public [`crate::manager::ServiceManager`] trait: the exact TOCTOU window
//! between classification and write (obligation 1). Reaching it needs to control `raw_state` and
//! `create_unit` directly, with a foreign write injected in between — something no interleaving of
//! `install` calls alone can force deterministically.
//!
//! Elevated (writes real files under `/etc/systemd/system`), so it opts into the same `elevated`
//! precondition/label convention `tests/support/mod.rs` uses, duplicated locally rather than shared:
//! a plain library unit test cannot depend on the `tests/` integration-test support crate.

use std::path::PathBuf;

use super::*;

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
