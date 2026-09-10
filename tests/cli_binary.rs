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
/// `goetia::manager::native()` and not to the fake: on a platform with no
/// backend, that means failing with `native()`'s exact message rather than
/// panicking.
#[skuld::test]
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn unimplemented_backend_names_the_platform_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();

    let (code, out, err) = run_cli(&["daemon", "list"], dir.path());

    assert_eq!(
        code, 1,
        "stdout:
{out}
stderr:
{err}"
    );
    let expected = format!("no backend for {} yet", std::env::consts::OS);
    assert!(err.contains(&expected), "stderr should name the missing backend: {err}");
}

/// On every supported platform, `goetia daemon list` reaches a real manager
/// and needs no elevation. The proof of correct wiring is an empty `errors`:
/// a genuinely unwired manager reports `native()`'s "no backend" there, as
/// `unavailable` at exit `1` — the fake would also answer cleanly here, so
/// this rules out exactly the one wrong wiring this module exists to catch.
/// Labelled `UNIT_DIR_EXCLUSIVE`: this runs a real `daemon list` over the
/// host's own artifact directories, and `list`'s exit code is host-wide now,
/// so a concurrently-running test that seeds an unreadable artifact would
/// turn this `0` into a `4`. The label is the same cross-process lock the
/// systemd and launchd integration tests take for that reason.
///
/// `--json` rather than the text form, and the code tied to the document
/// rather than merely allowed to be `0` or `4`: the exit code is host-wide,
/// so one vendor artifact this caller cannot read legitimately makes it `4`
/// — but only *with an entry that accounts for it*. A `4` over an empty
/// `undetermined` is an exit code nothing in the document explains, which
/// `matches!(code, 0 | 4)` alone accepted.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn native_backend_answers_list_unelevated() {
    let dir = tempfile::tempdir().unwrap();

    let (code, out, err) = run_cli(&["daemon", "list", "--json"], dir.path());

    let context = format!("stdout:\n{out}\nstderr:\n{err}");
    let doc: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {context}"));
    let array = |key: &str| {
        doc[key]
            .as_array()
            .unwrap_or_else(|| panic!("`{key}` must always be present, as an array: {context}"))
    };

    assert!(
        array("errors").is_empty(),
        "a real manager answers `list` on this platform: {context}"
    );
    assert_eq!(
        code,
        if array("undetermined").is_empty() { 0 } else { 4 },
        "exit `4` is exactly what an `undetermined` entry reports, and nothing else here reports it: {context}"
    );
}

/// Shared with the systemd and launchd integration binaries: any test that
/// reads or writes the host's real artifact directories takes this, because
/// `list`'s exit code is host-wide and one unreadable artifact changes it
/// for every concurrent reader.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

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

// backend-specific overrides ==========================================================================================

/// An unknown key under `backend-specific:` is a manifest error, not a
/// silently-ignored typo — `daemon show` fails outright and names the
/// offending key.
#[skuld::test]
fn an_unknown_backend_key_is_a_manifest_error() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n    backend-specific:\n      windwos:\n        user: svc\n",
    );

    let (code, out, err) = run_cli(&["daemon", "show", "-f", "goetia.yaml"], dir.path());

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("windwos"), "stderr should name the unknown key: {err}");
}

/// The same shape as `warnings_are_printed_to_stderr`, with an unrelated
/// backend override present: a `backend-specific:` block that touches a
/// field the sub-second-restart-delay advisory does not read must not
/// duplicate that warning across the all-backends sweep (see
/// `spec::resolve`'s step 4) — stderr still carries exactly one
/// `warning:` line.
#[skuld::test]
fn warnings_still_fire_once_with_backend_overrides_present() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n    restart-delay: 500ms\n    backend-specific:\n      systemd:\n        \
         cwd: /var/lib/frpc\n",
    );

    let (code, out, err) = run_cli(&["daemon", "show", "-f", "goetia.yaml"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let warning_lines = err.lines().filter(|line| line.contains("warning:")).count();
    assert_eq!(warning_lines, 1, "stderr:\n{err}");
    assert!(err.contains("restart-delay"), "stderr:\n{err}");
    assert!(out.contains("frpc"), "stdout:\n{out}");
}

/// The `.env` gate (`spec::resolve`'s step 2) end to end: a `${VAR}` written only
/// inside `backend-specific.<native>` still resolves from a `.env` beside
/// the manifest. Before the fix, `spec::resolve`'s `.env` gate was computed
/// from the wrong spec and this exited `1` with "no value for" even though
/// the file defining the variable sat in the very directory `-f` named.
#[skuld::test]
fn a_variable_used_only_in_the_native_override_resolves_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let native = goetia::spec::Backend::native().expect("native() is Some on every CI platform");
    write_manifest(
        dir.path(),
        &format!(
            "daemons:\n  frpc:\n    command: [frpc]\n    backend-specific:\n      {native}:\n        restart: ${{R}}\n"
        ),
    );
    std::fs::write(dir.path().join(".env"), "R=always\n").expect("write .env fixture");

    let (code, out, err) = run_cli(&["daemon", "show", "-f", "goetia.yaml"], dir.path());

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("restart: always"), "stdout:\n{out}");
}

// --json's clap-level carve-out =======================================================================================

/// The published `--json` invariant is over *subcommands*: whenever `--json`
/// is given together with one, stdout is exactly one JSON document.
/// `--help`/`--version` are clap-level — clap short-circuits before
/// `dispatch` is ever called — so they keep printing their own text.
/// Rendering them as JSON would mean pre-scanning `std::env::args()` before
/// parsing; the carve-out is pinned here so it stays deliberate.
#[skuld::test]
fn json_with_version_and_help_is_carved_out() {
    let dir = tempfile::tempdir().unwrap();

    let (version_code, version_out, version_err) = run_cli(&["--json", "--version"], dir.path());
    assert_eq!(version_code, 0, "stdout:\n{version_out}\nstderr:\n{version_err}");
    assert!(version_out.starts_with("goetia "), "stdout:\n{version_out}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&version_out).is_err(),
        "--version must stay plain text:\n{version_out}"
    );

    let (help_code, help_out, help_err) = run_cli(&["--json", "--help"], dir.path());
    assert_eq!(help_code, 0, "stdout:\n{help_out}\nstderr:\n{help_err}");
    assert!(help_out.contains("Usage:"), "stdout:\n{help_out}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&help_out).is_err(),
        "--help must stay plain text:\n{help_out}"
    );
}

/// The third clap-level carve-out, and the one that would otherwise be
/// discovered by a user: `daemon uninstall` requires an id, so clap rejects
/// the command line before `dispatch` exists and stdout stays empty —
/// exactly the empty-string-to-`json.loads` case `--json` exists to remove.
/// Intercepting it would mean re-rendering clap's own diagnostics, so the
/// published invariant is narrowed to subcommands clap accepted (see
/// `cli::dispatch`) and pinned here instead.
#[skuld::test]
fn json_with_a_clap_rejected_command_line_is_carved_out() {
    let dir = tempfile::tempdir().unwrap();

    let (code, out, err) = run_cli(&["--json", "daemon", "uninstall"], dir.path());

    assert_eq!(code, 2, "clap's own usage-error code: stdout:\n{out}\nstderr:\n{err}");
    assert!(out.is_empty(), "stdout:\n{out}");
    assert!(err.contains("Usage:"), "clap must still explain itself: stderr:\n{err}");
}

// Exit-code vocabulary: usage vs. conflict ============================================================================
//
// goetia's own conflict code sits on `5`, never on `2`, precisely so a
// wrapper script that typos a flag or a subcommand cannot mistake clap's
// usage-error code for goetia's conflict code and re-run `install --force`.
// These tests pin both halves of that: `2` stays clap's, unclaimed by
// anything else, and `5` never shows up on a malformed command line.

/// `goetia daemon bogus` (an unknown subcommand) and an unknown global flag
/// both go through clap's own rejection path and exit `2` — clap's default,
/// deliberately left un-overridden by `main.rs`. Pinned so this stays a
/// choice, not an accident nobody checked.
#[skuld::test]
fn a_usage_error_exits_two() {
    let dir = tempfile::tempdir().unwrap();

    let (bogus_code, bogus_out, bogus_err) = run_cli(&["daemon", "bogus"], dir.path());
    assert_eq!(bogus_code, 2, "stdout:\n{bogus_out}\nstderr:\n{bogus_err}");

    let (flag_code, flag_out, flag_err) = run_cli(&["--nosuchflag"], dir.path());
    assert_eq!(flag_code, 2, "stdout:\n{flag_out}\nstderr:\n{flag_err}");
}

/// The collision this task exists to remove: neither malformed invocation
/// above may ever produce `5`, goetia's own conflict code, even though a
/// wrapper script could otherwise mistake one for the other.
#[skuld::test]
fn no_subcommand_returns_the_conflict_code_for_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();

    let (bogus_code, bogus_out, bogus_err) = run_cli(&["daemon", "bogus"], dir.path());
    assert_ne!(bogus_code, 5, "stdout:\n{bogus_out}\nstderr:\n{bogus_err}");

    let (flag_code, flag_out, flag_err) = run_cli(&["--nosuchflag"], dir.path());
    assert_ne!(flag_code, 5, "stdout:\n{flag_out}\nstderr:\n{flag_err}");
}

/// `--help`/`--version` (without `--json`, unlike
/// `json_with_version_and_help_is_carved_out`) still exit `0` with their
/// own text on stdout, unaffected by the conflict code's move.
#[skuld::test]
fn help_and_version_still_exit_zero_on_stdout() {
    let dir = tempfile::tempdir().unwrap();

    let (version_code, version_out, version_err) = run_cli(&["--version"], dir.path());
    assert_eq!(version_code, 0, "stdout:\n{version_out}\nstderr:\n{version_err}");
    assert!(version_out.starts_with("goetia "), "stdout:\n{version_out}");

    let (help_code, help_out, help_err) = run_cli(&["--help"], dir.path());
    assert_eq!(help_code, 0, "stdout:\n{help_out}\nstderr:\n{help_err}");
    assert!(help_out.contains("Usage:"), "stdout:\n{help_out}");
}
