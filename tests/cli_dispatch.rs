//! CLI behavior tests that need a working `ServiceManager`.
//!
//! `native()` errors on every platform until Tasks 11-13 land (see
//! `goetia::cli`'s module doc comment), so these run `goetia::cli::dispatch`
//! in-process against `goetia::manager::fake::Fake`, injected exactly the way
//! `main.rs` injects `native()` — through `dispatch`'s `get_manager`
//! parameter. Tests that need no manager at all run the real compiled binary
//! instead; see `tests/cli_binary.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use goetia::cli::{self, Cli};
use goetia::manager::fake::Fake;
use goetia::manager::{Installed, ServiceManager, State};
use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};

fn main() {
    skuld::run_all();
}

// Fixtures ============================================================================================================

fn mk(id: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).unwrap(),
        name: id.to_string(),
        command: vec!["daemon".to_owned()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

fn write_manifest(dir: &Path, yaml: &str) -> PathBuf {
    let path = dir.join("goetia.yaml");
    std::fs::write(&path, yaml).expect("write goetia.yaml fixture");
    path
}

/// Parse `args` and dispatch against `fake`, capturing stdout/stderr as
/// strings. `is_elevated` is passed straight through, so a test can hand in
/// a closure that panics if called — proof a read-only subcommand never
/// checks elevation.
fn dispatch(args: &[&str], fake: &Fake, is_elevated: &dyn Fn() -> bool) -> (i32, String, String) {
    let cli = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let fake = fake.clone();
    let get_manager = move || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(fake.clone())) };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = cli::dispatch(&cli, &get_manager, is_elevated, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("stdout is UTF-8"),
        String::from_utf8(err).expect("stderr is UTF-8"),
    )
}

/// [`dispatch`] with elevation granted — the common case for mutating
/// subcommands under test.
fn dispatch_elevated(args: &[&str], fake: &Fake) -> (i32, String, String) {
    dispatch(args, fake, &|| true)
}

/// [`dispatch`] proving elevation is never checked — for the read-only
/// subcommands.
fn dispatch_read_only(args: &[&str], fake: &Fake) -> (i32, String, String) {
    dispatch(args, fake, &|| {
        panic!("a read-only subcommand must never check elevation")
    })
}

fn installed_ids(fake: &Fake) -> Vec<String> {
    let mut ids: Vec<String> = fake
        .list()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            Installed::Ours { spec, .. } => Some(spec.id.as_str().to_string()),
            Installed::OursUnreadable { .. } => None,
        })
        .collect();
    ids.sort();
    ids
}

// install =============================================================================================================

#[skuld::test]
fn install_all_daemons_when_no_ids_given() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n  websocat:\n    command: [websocat]\n",
    );
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(installed_ids(&fake), vec!["frpc".to_string(), "websocat".to_string()]);
}

#[skuld::test]
fn install_selects_named_ids() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n  websocat:\n    command: [websocat]\n",
    );
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap(), "frpc"],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(installed_ids(&fake), vec!["frpc".to_string()]);
}

#[skuld::test]
fn install_unknown_named_id_installs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "-f",
            manifest.to_str().unwrap(),
            "nonexistent",
        ],
        &fake,
    );

    assert_eq!(code, 1);
    assert!(err.contains("nonexistent"), "{err}");
    assert!(installed_ids(&fake).is_empty(), "a bad selection must install nothing");
}

#[skuld::test]
fn conflict_exits_two() {
    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");

    let (code, out, _err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 2, "stdout:\n{out}");
    assert!(out.contains("conflict"), "{out}");
}

#[skuld::test]
fn install_start_and_enable_flags_reach_the_manager() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "-f",
            manifest.to_str().unwrap(),
            "--start",
            "--enable",
        ],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let status = fake.status(&Id::try_from("frpc").unwrap()).unwrap();
    assert_eq!(status.state, State::Running, "--start must reach the manager");
    assert!(status.enabled, "--enable must reach the manager");
}

/// Unlike `list`/`status`/`show`/`diff` (whose `run` functions do not even
/// take an `is_elevated` parameter — elevation is checked nowhere in their
/// call graph, a compile-time guarantee), `install` *does* receive the
/// closure and must skip calling it on the `--dry-run` branch specifically.
/// A manager closure that panics if called proves the same thing for the
/// one case where "never calls it" is a runtime property, not a structural
/// one.
#[skuld::test]
fn install_dry_run_does_not_check_elevation_or_touch_the_manager() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");
    let cli = Cli::try_parse_from([
        "goetia",
        "daemon",
        "install",
        "-f",
        manifest.to_str().unwrap(),
        "--dry-run",
    ])
    .expect("parse");
    let get_manager =
        || -> goetia::Result<Box<dyn ServiceManager>> { panic!("--dry-run must never touch the manager") };
    let is_elevated = || -> bool { panic!("--dry-run must never check elevation") };
    let mut out = Vec::new();
    let mut err = Vec::new();

    let code = cli::dispatch(&cli, &get_manager, &is_elevated, &mut out, &mut err);

    assert_eq!(code, 0, "stderr:\n{}", String::from_utf8_lossy(&err));
    assert!(!out.is_empty(), "dry-run should print something");
}

#[skuld::test]
fn install_without_flags_does_not_start_or_enable() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");
    let fake = Fake::new();

    let (code, _out, _err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 0);
    let status = fake.status(&Id::try_from("frpc").unwrap()).unwrap();
    assert_ne!(status.state, State::Running);
    assert!(!status.enabled);
}

// show ================================================================================================================

#[skuld::test]
fn show_from_file_and_show_from_installed_agree() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc, -c, frpc.toml]\n    restart: on-failure\n",
    );
    let fake = Fake::new();
    let (install_code, _, install_err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );
    assert_eq!(install_code, 0, "{install_err}");

    let (code_file, out_file, _) =
        dispatch_read_only(&["goetia", "daemon", "show", "-f", manifest.to_str().unwrap()], &fake);
    let (code_installed, out_installed, _) = dispatch_read_only(&["goetia", "daemon", "show"], &fake);

    assert_eq!(code_file, 0);
    assert_eq!(code_installed, 0);
    assert_eq!(
        out_file, out_installed,
        "show -f and show (from installed) must render identically"
    );
    assert!(out_file.contains("frpc"), "{out_file}");
}

// The per-verb wiring the previous plan omitted ========================================================================

#[skuld::test]
fn uninstall_reaches_the_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "frpc"], &fake);

    assert_eq!(code, 0);
    assert!(installed_ids(&fake).is_empty());
}

#[skuld::test]
fn uninstall_unknown_id_exits_nonzero() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "nonexistent"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("nonexistent"), "{err}");
}

#[skuld::test]
fn start_reaches_the_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "start", "frpc"], &fake);

    assert_eq!(code, 0);
    assert_eq!(
        fake.status(&Id::try_from("frpc").unwrap()).unwrap().state,
        State::Running
    );
}

#[skuld::test]
fn stop_reaches_the_manager() {
    let fake = Fake::new();
    let spec = mk("frpc");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "stop", "frpc"], &fake);

    assert_eq!(code, 0);
    assert_ne!(
        fake.status(&Id::try_from("frpc").unwrap()).unwrap().state,
        State::Running
    );
}

#[skuld::test]
fn restart_reaches_the_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc"], &fake);

    assert_eq!(code, 0);
    assert_eq!(
        fake.status(&Id::try_from("frpc").unwrap()).unwrap().state,
        State::Running
    );
}

#[skuld::test]
fn enable_reaches_the_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "enable", "frpc"], &fake);

    assert_eq!(code, 0);
    assert!(fake.status(&Id::try_from("frpc").unwrap()).unwrap().enabled);
}

#[skuld::test]
fn disable_reaches_the_manager() {
    let fake = Fake::new();
    let spec = mk("frpc");
    fake.install(&spec, false).unwrap();
    fake.enable(&spec.id).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "disable", "frpc"], &fake);

    assert_eq!(code, 0);
    assert!(!fake.status(&Id::try_from("frpc").unwrap()).unwrap().enabled);
}

#[skuld::test]
fn status_reaches_the_manager() {
    let fake = Fake::new();
    let spec = mk("frpc");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "status", "frpc"], &fake);

    assert_eq!(code, 0);
    assert!(out.contains("running"), "{out}");
}

#[skuld::test]
fn diff_reaches_the_manager() {
    let fake = Fake::new();
    let mut old = mk("frpc");
    old.restart = Restart::Never;
    fake.install(&old, false).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [daemon]\n    restart: on-failure\n",
    );

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 0);
    assert!(out.contains("restart"), "{out}");
}

#[skuld::test]
fn list_reaches_the_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);

    assert_eq!(code, 0);
    assert!(out.contains("frpc"), "{out}");
}
