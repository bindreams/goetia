//! A unit file goetia finds installed that the running manager has not loaded: its answer is about
//! no file, so it is never the unit's state.

use std::fs;
use std::process::Command;

use goetia::backend::systemd::manager::Systemd;
use goetia::manager::ServiceManager;
use goetia::spec::Id;

use crate::linux::{mk, unit_path};
use crate::support::{self, ELEVATED, ServiceGuard};

/// `goetia <args>` where goetia finds `unit` installed and the manager does not: in a private mount
/// namespace whose `/etc/systemd/system` is a tmpfs holding only `unit`. PID 1 does not see it, and
/// answers `LoadState=not-found` — as for a unit file written and not yet loaded. The mount goes
/// with the namespace, however goetia exits.
fn goetia_seeing(unit: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("unshare")
        .args([
            "--mount",
            "--propagation",
            "private",
            "/bin/sh",
            "-c",
            "mount -t tmpfs goetia-unloaded /etc/systemd/system && cp \"$0\" /etc/systemd/system/ && exec \"$@\"",
        ])
        .arg(unit)
        .arg(env!("CARGO_BIN_EXE_goetia"))
        .args(args)
        .output()
        .expect("spawn unshare")
}

/// `status` by id, `status` and `list` report a unit systemd has not loaded as what it is — goetia's
/// own, whose live state systemd does not have: `unreadable`, exit `4`, as for any live state it
/// could not read. Never "stopped, not enabled", which is what systemd answers for a name it has no
/// file for.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_unit_systemd_has_not_loaded_is_never_reported_stopped() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false).expect("install");
    let dir = tempfile::tempdir().expect("tempdir");
    let unit = dir.path().join(format!("{}.service", guard.id()));
    fs::copy(unit_path(guard.id()), &unit).expect("copy the unit");
    mgr.uninstall(&Id::try_from(guard.id()).unwrap())
        .expect("uninstall from the host");

    for (args, says) in [
        (
            &["daemon", "status", guard.id()][..],
            format!("error: {}: ", guard.id()),
        ),
        (&["daemon", "status"], format!("warning: {}", guard.id())),
        (&["daemon", "list"], format!("warning: {}", guard.id())),
    ] {
        let output = goetia_seeing(&unit, args);
        let (stdout, stderr) = (
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let context = format!("{args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert_eq!(output.status.code(), Some(4), "{context}");
        assert!(stdout.is_empty(), "{context}");
        assert!(stderr.contains(&says), "{context}");
        assert!(stderr.contains("systemd has not loaded"), "{context}");
        assert!(stderr.contains("LoadState=not-found"), "{context}");
    }
    assert!(!unit_path(guard.id()).exists(), "the host has no such unit");
}
