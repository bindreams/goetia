//! systemd: is an `[X-Goetia]` section in a unit file inert?
//!
//! `systemd.unit(5)` says an `X-`-prefixed section "is ignored completely by
//! systemd", and that unknown *non*-`X-` keys are warned about. Both halves are
//! tested: the second is the positive control without which the first could
//! pass by reading an empty journal.

use std::fs;
use std::path::PathBuf;

use crate::support::{self, ELEVATED, ServiceGuard, cmd};

const UNIT_DIR: &str = "/etc/systemd/system";

// Probes ==============================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn systemd_x_section_is_inert() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let description = format!("goetia X-section inertness probe {id}");
    let blob = support::probe_blob();
    let text = format!(
        "[Unit]\n\
         Description={description}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/bin/echo {id}\n\
         RemainAfterExit=yes\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n\
         \n\
         [X-Goetia]\n\
         Marker=goetia\n\
         Schema=1\n\
         Spec={blob}\n"
    );

    let cursor = journal_cursor();
    write_unit(guard.id(), &text);
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();

    // `daemon-reload` alone does not necessarily parse a unit nothing refers
    // to yet. `systemctl show` issues a LoadUnit, which does — so the journal
    // window below covers a parse that has certainly happened.
    let fragment = show(guard.id(), "FragmentPath");
    assert_eq!(fragment, unit_path(guard.id()).to_str().unwrap());

    // The decisive assertion: a unit systemd refused over the 8 KiB `Spec=`
    // line, or over the unknown section, does not reach `loaded`.
    assert_eq!(show(guard.id(), "LoadState"), "loaded", "unit did not load");

    // ...and it parsed the *whole* file, not a prefix of it.
    assert_eq!(show(guard.id(), "Description"), description);
    let exec_start = show(guard.id(), "ExecStart");
    assert!(exec_start.contains(&id), "ExecStart not parsed: {exec_start}");

    let complaints = journal_mentioning(&cursor, guard.id());
    assert!(
        complaints.is_empty(),
        "systemd logged about the unit:\n{}",
        complaints.join("\n")
    );

    // The section survives on disk byte-for-byte: systemd never rewrites a
    // fragment, which is what makes the file itself the metadata store.
    let on_disk = fs::read_to_string(&fragment).expect("read back fragment");
    assert_eq!(on_disk, text, "fragment changed on disk");

    support::record_probe(
        "systemd-x-section",
        &format!(
            "site: [X-Goetia] section in the unit fragment\n\
             inert: yes\n\
             blob_len: {} base64 chars\n\
             load_state: loaded\n\
             journal_complaints: 0\n\
             systemd_version: {}\n",
            blob.len(),
            systemd_version(),
        ),
    );
}

/// Without this, `systemd_x_section_is_inert` could pass by reading an empty
/// journal — proving only that the query found nothing, never that it could
/// have found something.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn systemd_unknown_non_x_key_does_warn() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    const KEY: &str = "GoetiaNotAnXPrefixedKey";
    let text = format!(
        "[Unit]\n\
         Description=goetia positive control {id}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/bin/echo {id}\n\
         {KEY}=1\n"
    );

    let cursor = journal_cursor();
    write_unit(guard.id(), &text);
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
    show(guard.id(), "FragmentPath");

    let complaints = journal_mentioning(&cursor, guard.id());
    assert!(
        complaints.iter().any(|line| line.contains(KEY)),
        "systemd did not warn about the unknown key `{KEY}`, so the journal query in \
         `systemd_x_section_is_inert` cannot detect a complaint either. Lines mentioning the unit:\n{}",
        complaints.join("\n")
    );

    support::record_probe(
        "systemd-positive-control",
        &format!("unknown non-X key warns: yes\nlines:\n{}\n", complaints.join("\n")),
    );
}

// Helpers -------------------------------------------------------------------------------------------------------------

fn unit_path(id: &str) -> PathBuf {
    PathBuf::from(UNIT_DIR).join(format!("{id}.service"))
}

fn write_unit(id: &str, text: &str) {
    let path = unit_path(id);
    fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn show(id: &str, property: &str) -> String {
    let unit = format!("{id}.service");
    let run = cmd::run("systemctl", &["show", "--property", property, "--value", &unit]).expect_ok();
    run.stdout.trim_end().to_string()
}

fn systemd_version() -> String {
    cmd::run("systemctl", &["--version"])
        .stdout
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `journalctl --sync` is documented not to return until synchronization is
/// complete — a real rendezvous with journald, not a chosen interval. Without
/// it the reads below fail *open*: an unwritten journal looks like a silent
/// systemd, which is exactly the false green this suite exists to prevent.
fn journal_sync() {
    cmd::run("journalctl", &["--sync"]).expect_ok();
}

fn journal_cursor() -> String {
    journal_sync();
    let run = cmd::run("journalctl", &["--no-pager", "--lines=1", "--show-cursor"]).expect_ok();
    run.stdout
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("-- cursor:"))
        .map(|cursor| cursor.trim().to_string())
        .unwrap_or_else(|| panic!("journalctl printed no cursor; a journal check without one is worthless:\n{run}"))
}

/// Lines journald recorded after `cursor` that name `id`. Test ids are random,
/// so nothing but systemd's own messages about our unit can match.
fn journal_mentioning(cursor: &str, id: &str) -> Vec<String> {
    journal_sync();
    let run = cmd::run("journalctl", &["--no-pager", "--after-cursor", cursor]).expect_ok();
    run.stdout
        .lines()
        .filter(|line| line.contains(id))
        .map(str::to_string)
        .collect()
}
