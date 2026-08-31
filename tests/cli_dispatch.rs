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
use goetia::manager::{Installed, ServiceManager, State, Status};
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

/// Wraps a `Fake`, forcing `enable`/`start` to fail for one specific id.
/// Exists to exercise `install`'s post-`enable`/`start` error-reporting
/// branches: a plain `Fake`'s own errors are always `NotInstalled`, which
/// cannot happen immediately after a successful install, so those branches
/// are otherwise unreachable from any test.
#[derive(Clone)]
struct FlakyManager {
    inner: Fake,
    fail_enable_for: Option<String>,
    fail_start_for: Option<String>,
}

fn injected_failure(id: &Id) -> goetia::Error {
    goetia::Error::NotInstalled {
        id: format!("{id} (injected test failure)"),
    }
}

impl ServiceManager for FlakyManager {
    fn install(&self, spec: &DaemonSpec, force: bool) -> goetia::Result<goetia::decide::Outcome> {
        self.inner.install(spec, force)
    }
    fn preview_install(&self, spec: &DaemonSpec) -> goetia::Result<goetia::decide::Outcome> {
        self.inner.preview_install(spec)
    }
    fn uninstall(&self, id: &Id) -> goetia::Result<()> {
        self.inner.uninstall(id)
    }
    fn enable(&self, id: &Id) -> goetia::Result<()> {
        if self.fail_enable_for.as_deref() == Some(id.as_str()) {
            return Err(injected_failure(id));
        }
        self.inner.enable(id)
    }
    fn disable(&self, id: &Id) -> goetia::Result<()> {
        self.inner.disable(id)
    }
    fn start(&self, id: &Id) -> goetia::Result<()> {
        if self.fail_start_for.as_deref() == Some(id.as_str()) {
            return Err(injected_failure(id));
        }
        self.inner.start(id)
    }
    fn stop(&self, id: &Id) -> goetia::Result<()> {
        self.inner.stop(id)
    }
    fn status(&self, id: &Id) -> goetia::Result<Status> {
        self.inner.status(id)
    }
    fn list(&self) -> goetia::Result<Vec<Installed>> {
        self.inner.list()
    }
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

// Per-verb wiring: uninstall/start/stop/restart/enable/disable/status/diff/list ========================================================================

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

// Foreign-id refusal and unreadable-entry regression coverage ========================================================================

/// "Goetia never touches a service it did not create" (§5) must hold for
/// every verb reachable through the CLI, not just `install`.
#[skuld::test]
fn cli_refuses_a_foreign_id_for_every_verb() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    for args in [
        vec!["goetia", "daemon", "uninstall", "stranger"],
        vec!["goetia", "daemon", "enable", "stranger"],
        vec!["goetia", "daemon", "disable", "stranger"],
        vec!["goetia", "daemon", "start", "stranger"],
        vec!["goetia", "daemon", "stop", "stranger"],
        vec!["goetia", "daemon", "restart", "stranger"],
    ] {
        let (code, _out, err) = dispatch_elevated(&args, &fake);
        assert_eq!(code, 1, "{args:?}: {err}");
    }
    let (status_code, _out, status_err) = dispatch_read_only(&["goetia", "daemon", "status", "stranger"], &fake);
    assert_eq!(status_code, 1, "{status_err}");
}

/// Exit code 2 must mean "every failure here is force-resolvable". A batch
/// mixing a conflict (force-resolvable) with a foreign refusal (not) must
/// not let the refusal hide behind the conflict's higher numeric code.
#[skuld::test]
fn install_exit_code_does_not_mask_an_error_behind_a_conflict() {
    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("conflicted"), "# hand-added directive\n");
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  conflicted:\n    command: [daemon]\n  stranger:\n    command: [daemon]\n",
    );

    let (code, out, _err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(
        code, 1,
        "an unresolvable refusal must win over a resolvable conflict:\n{out}"
    );
}

/// `--dry-run`'s preview must not silently drop a `Warning` a generator
/// produces (e.g. SCM clamping a too-large `restart-delay`).
#[skuld::test]
fn install_dry_run_prints_generator_warnings() {
    let dir = tempfile::tempdir().unwrap();
    // A restart-delay past SC_ACTION.Delay's ~49.71-day DWORD-milliseconds
    // ceiling: only the Windows SCM preview warns about this, but the test
    // must still pass (vacuously) on the other two platforms.
    write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n    type: managed\n    restart: on-failure\n    restart-delay: 60d\n",
    );
    let cli = Cli::try_parse_from([
        "goetia",
        "daemon",
        "install",
        "-f",
        dir.path().to_str().unwrap(),
        "--dry-run",
    ])
    .expect("parse");
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { panic!("dry-run must never touch the manager") };
    let is_elevated = || -> bool { panic!("dry-run must never check elevation") };
    let mut out = Vec::new();
    let mut err = Vec::new();

    let code = cli::dispatch(&cli, &get_manager, &is_elevated, &mut out, &mut err);

    assert_eq!(code, 0);
    if cfg!(windows) {
        let err = String::from_utf8_lossy(&err);
        assert!(err.contains("warning:"), "stderr:\n{err}");
    }
}

/// The `list`/`status`/`show`/`diff` partitioning helper must warn about an
/// `OursUnreadable` entry and escalate the exit code for every one of those
/// subcommands.
#[skuld::test]
fn list_reports_an_unreadable_entry_and_exits_nonzero() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.install(&mk("readable"), false).unwrap();

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("corrupt"), "{err}");
    assert!(err.contains("unreadable"), "{err}");
    assert!(out.contains("readable"), "{out}");
}

#[skuld::test]
fn status_all_reports_an_unreadable_entry_and_exits_nonzero() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.install(&mk("readable"), false).unwrap();

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "status"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("corrupt"), "{err}");
    assert!(out.contains("readable"), "{out}");
}

#[skuld::test]
fn show_unknown_id_is_not_installed() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_read_only(&["goetia", "daemon", "show", "nonexistent"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("nonexistent"), "{err}");
    assert!(err.contains("not installed"), "{err}");
}

/// An unreadable id must be reported as unreadable, never as "not installed
/// (would be created)".
#[skuld::test]
fn diff_reports_an_unreadable_entry_instead_of_claiming_it_would_be_created() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  corrupt:\n    command: [daemon]\n");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("unreadable"), "{err}");
    assert!(!out.contains("would be created"), "stdout:\n{out}");
}

#[skuld::test]
fn diff_reports_up_to_date_when_the_spec_is_unchanged() {
    let fake = Fake::new();
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n    restart: on-failure\n",
    );

    // Install the manifest's own resolved spec directly, rather than a
    // hand-built one: `resolve()` makes `command[0]` absolute against the
    // manifest's directory, which a literal `mk("frpc")` fixture would not
    // match, making the diff always non-empty for the wrong reason.
    let (specs, _warnings) = goetia::spec::load(&manifest).expect("load fixture manifest");
    fake.install(&specs[0], false).expect("seed install");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("up to date"), "{out}");
}

#[skuld::test]
fn diff_reports_not_installed_for_an_id_absent_from_the_manager() {
    let fake = Fake::new();

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 0);
    assert!(out.contains("not installed (would be created)"), "{out}");
}

/// Exercises `install::run`'s `enable`/`start` post-install error-reporting
/// branches, otherwise unreachable from any test — see [`FlakyManager`].
#[skuld::test]
fn install_reports_enable_and_start_failures_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");
    let mgr = FlakyManager {
        inner: Fake::new(),
        fail_enable_for: Some("frpc".to_string()),
        fail_start_for: Some("frpc".to_string()),
    };

    let cli = Cli::try_parse_from([
        "goetia",
        "daemon",
        "install",
        "-f",
        manifest.to_str().unwrap(),
        "--enable",
        "--start",
    ])
    .expect("parse");
    let get_manager = {
        let mgr = mgr.clone();
        move || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(mgr.clone())) }
    };
    let is_elevated = || true;
    let mut out = Vec::new();
    let mut err = Vec::new();

    let code = cli::dispatch(&cli, &get_manager, &is_elevated, &mut out, &mut err);

    assert_eq!(
        code,
        1,
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("enable"), "{err}");
    assert!(err.contains("start"), "{err}");
}

/// `--json`/`-v`/`-q` are reserved (see `Cli`'s doc comments) rather than
/// wired to per-subcommand output yet. Pinned here so that reservation is
/// itself a tested, deliberate state: a change to `list`'s output with
/// `--json` present would need to update this test, rather than silently
/// landing unnoticed in either direction.
#[skuld::test]
fn cli_accepts_json_verbose_quiet_as_currently_inert() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (plain_code, plain_out, _) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);
    let (flagged_code, flagged_out, _) = dispatch_read_only(&["goetia", "--json", "-v", "-q", "daemon", "list"], &fake);

    assert_eq!(plain_code, flagged_code);
    assert_eq!(plain_out, flagged_out, "these flags must not be silently half-wired");
}

/// `status <id>` must not fabricate a `Stopped`/`enabled: false` answer for
/// an id whose blob will not decode — the marker alone does not make its
/// `state`/`enabled` trustworthy. `status` (no ids), `list`, and `diff` all
/// already refuse to invent an answer for the same case.
#[skuld::test]
fn status_single_id_errors_on_an_unreadable_entry_instead_of_fabricating_state() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");

    let (code, _out, err) = dispatch_read_only(&["goetia", "daemon", "status", "corrupt"], &fake);

    assert_eq!(code, 1, "{err}");
    assert!(err.contains("corrupt"), "{err}");
}

/// `diff` must predict what `install` would actually do: a hand-edited
/// artifact is a conflict, never "up to date".
#[skuld::test]
fn diff_reports_would_conflict_for_a_hand_edited_artifact() {
    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("would conflict"), "{out}");
    assert!(!out.contains("up to date"), "{out}");
}

/// `diff` must distinguish "absent" from "occupied by a stranger's
/// service": both used to render identically as "would be created", which
/// `install` would then immediately contradict by refusing.
#[skuld::test]
fn diff_reports_would_be_refused_for_a_foreign_id() {
    let fake = Fake::new();
    fake.seed_foreign("frpc", "not a goetia artifact at all\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [frpc]\n");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("would be refused"), "{out}");
    assert!(!out.contains("would be created"), "{out}");
}

/// `state_str`'s `Failed`/`Unknown` arms are otherwise unreachable from any
/// test — `Fake::start`/`stop` can only ever produce `Running`/`Stopped` —
/// so nothing would catch a wrong string or a swapped arm there.
#[skuld::test]
fn list_and_status_render_failed_and_unknown_states() {
    let fake = Fake::new();
    fake.install(&mk("flaky"), false).unwrap();
    fake.seed_state("flaky", State::Failed);
    fake.install(&mk("mystery"), false).unwrap();
    fake.seed_state("mystery", State::Unknown);

    let (list_code, list_out, _) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);
    assert_eq!(list_code, 0);
    assert!(list_out.contains("failed"), "{list_out}");
    assert!(list_out.contains("unknown"), "{list_out}");

    let (status_code, status_out, _) = dispatch_read_only(&["goetia", "daemon", "status", "flaky"], &fake);
    assert_eq!(status_code, 0);
    assert!(status_out.contains("failed"), "{status_out}");
}

/// `run_id_verb` must validate every id before mutating any of them: a
/// syntactically invalid id anywhere in the list must leave every id
/// untouched, not partially execute up to the point where parsing failed.
#[skuld::test]
fn enable_with_an_invalid_id_touches_nothing() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "enable", "frpc", "not/a/valid/id"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("not/a/valid/id"), "{err}");
    assert!(
        !fake.status(&Id::try_from("frpc").unwrap()).unwrap().enabled,
        "frpc must not have been enabled: an earlier valid id must not run ahead of a later invalid one"
    );
}

/// Multiple ids, one succeeding and one failing at the manager level (both
/// syntactically valid, so `run_id_verb`'s upfront parse validation does not
/// apply): both must still be attempted, and the failure must not be lost.
#[skuld::test]
fn enable_aggregates_across_multiple_ids() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "enable", "frpc", "never-installed"], &fake);

    assert_eq!(code, 1);
    assert!(out.contains("frpc: enabled"), "{out}");
    assert!(err.contains("never-installed"), "{err}");
    assert!(
        fake.status(&Id::try_from("frpc").unwrap()).unwrap().enabled,
        "frpc must still have been enabled despite the other id failing"
    );
}
