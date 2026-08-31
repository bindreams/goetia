//! CLI behavior tests that need no `ServiceManager` at all, run against the
//! real compiled `goetia` binary (`CARGO_BIN_EXE_goetia`, never a runtime
//! `cargo` invocation, per the Global Constraints).
//!
//! Everything here is either pure (`install --dry-run`, `show -f`) or
//! deliberately exercises `main.rs`'s real wiring to
//! `goetia::manager::native()`, which errors on every platform until Tasks
//! 11-13 land (macOS and Windows excepted). Behavior that needs a
//! *working* manager lives in `tests/cli_dispatch.rs` instead, dispatched
//! in-process against the fake.

use std::path::Path;
use std::process::Command;

fn main() {
    skuld::run_all();
}

fn goetia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_goetia")
}

fn run_cli(args: &[&str], cwd: &Path) -> (i32, String, String) {
    let output = Command::new(goetia_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn goetia {args:?}: {e}"));
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("goetia {args:?} was killed by a signal: {:?}", output.status));
    (
        code,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn write_manifest(dir: &Path, yaml: &str) {
    std::fs::write(dir.join("goetia.yaml"), yaml).expect("write goetia.yaml fixture");
}

// native() wiring =====================================================================================================

/// `goetia daemon list` needs no elevation but does need a manager, so it is
/// the cleanest proof that the real binary is wired to
/// `goetia::manager::native()` and not to the fake: on every platform but
/// macOS/Windows, that means it fails with `native()`'s exact message rather
/// than panicking.
#[skuld::test]
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn unimplemented_backend_names_the_platform_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();

    let (code, out, err) = run_cli(&["daemon", "list"], dir.path());

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    let expected = format!("no backend for {} yet", std::env::consts::OS);
    assert!(err.contains(&expected), "stderr should name the missing backend: {err}");
}

/// macOS's counterpart: `goetia daemon list` is wired to a real
/// `LaunchdManager`, so it must succeed (and needs no elevation — it only
/// reads the filesystem) rather than report a missing backend.
#[skuld::test]
#[cfg(target_os = "macos")]
fn native_backend_answers_list_unelevated() {
    let dir = tempfile::tempdir().unwrap();

    let (code, out, err) = run_cli(&["daemon", "list"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!err.contains("no backend"), "stderr:\n{err}");
}

/// Windows' counterpart: `goetia daemon list` is wired to a real
/// `ScmManager`, so it must succeed (and needs no elevation — it only reads
/// the registry) rather than report a missing backend.
#[skuld::test]
#[cfg(target_os = "windows")]
fn native_backend_answers_list_unelevated_on_windows() {
    let dir = tempfile::tempdir().unwrap();

    let (code, out, err) = run_cli(&["daemon", "list"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!err.contains("no backend"), "stderr:\n{err}");
}

// Pure paths: no elevation, no manager ================================================================================

#[skuld::test]
fn dry_run_needs_no_elevation() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");

    let (code, out, err) = run_cli(&["daemon", "install", "--dry-run"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("frpc"), "{out}");
}

#[skuld::test]
fn f_flag_accepts_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");

    // `-f` names the directory itself, not `goetia.yaml` inside it.
    let (code, out, err) = run_cli(
        &["daemon", "install", "--dry-run", "-f", dir.path().to_str().unwrap()],
        dir.path(),
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("frpc"), "{out}");
}

#[skuld::test]
fn positional_is_always_an_id_never_a_path() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");
    // A directory that happens to share a name with the id below. If a
    // positional were ever mistaken for a path, `-f`'s default (".") would
    // get overridden by this empty directory instead of finding
    // `goetia.yaml` in `dir` itself, and `frpc` would fail to load.
    std::fs::create_dir(dir.path().join("frpc")).unwrap();

    let (code, out, err) = run_cli(&["daemon", "install", "--dry-run", "frpc"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("frpc"), "{out}");
}

#[skuld::test]
fn validation_failure_installs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // An empty `command` fails `resolve()`'s validation outright.
    write_manifest(dir.path(), "daemons:\n  frpc:\n    command: []\n");

    let (code, out, err) = run_cli(&["daemon", "install"], dir.path());

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!err.is_empty(), "a validation failure should explain itself on stderr");
}

#[skuld::test]
fn warnings_are_printed_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    // A sub-second restart-delay: `spec::resolve` accepts it but warns that
    // launchd's ThrottleInterval will round it up.
    write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n    restart-delay: 500ms\n",
    );

    let (code, out, err) = run_cli(&["daemon", "show", "-f", "goetia.yaml"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("warning:"), "stderr:\n{err}");
    assert!(err.contains("restart-delay"), "stderr:\n{err}");
    assert!(out.contains("frpc"), "stdout:\n{out}");
}
