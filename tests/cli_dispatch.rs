//! CLI behavior tests that need a working `ServiceManager`.
//!
//! `native()` errors on every platform without a backend yet (see
//! `goetia::cli`'s module doc comment — Linux has one), so these run
//! `goetia::cli::dispatch` in-process against
//! `goetia::manager::fake::Fake`, injected exactly the way `main.rs` injects
//! `native()` — through `dispatch`'s `get_manager` parameter, so these tests
//! stay platform-independent regardless of which real backends exist. Tests
//! that need no manager at all run the real compiled binary instead; see
//! `tests/cli_binary.rs`.

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

/// Parse `args` and dispatch against an arbitrary `get_manager` closure,
/// capturing stdout/stderr as strings. The lowest of the three layers here,
/// for the paths where obtaining a manager is itself what fails, and for
/// proving a subcommand never asks for one. `is_elevated` is passed
/// straight through, so a test can hand in a closure that panics if called
/// — proof a read-only subcommand never checks elevation.
fn dispatch_get_manager(
    args: &[&str],
    get_manager: &dyn Fn() -> goetia::Result<Box<dyn ServiceManager>>,
    is_elevated: &dyn Fn() -> bool,
) -> (i32, String, String) {
    let cli = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = cli::dispatch(&cli, get_manager, is_elevated, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("stdout is UTF-8"),
        String::from_utf8(err).expect("stderr is UTF-8"),
    )
}

/// [`dispatch_get_manager`] against any cloneable manager — [`FlakyManager`]
/// as well as [`Fake`].
fn dispatch_with<M: ServiceManager + Clone + 'static>(
    args: &[&str],
    mgr: &M,
    is_elevated: &dyn Fn() -> bool,
) -> (i32, String, String) {
    let mgr = mgr.clone();
    let get_manager = move || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(mgr.clone())) };
    dispatch_get_manager(args, &get_manager, is_elevated)
}

/// The `is_elevated` a read-only subcommand must never call.
fn never_elevated() -> bool {
    panic!("a read-only subcommand must never check elevation")
}

/// [`dispatch_with`] with elevation granted — the common case for mutating
/// subcommands under test.
fn dispatch_elevated(args: &[&str], fake: &Fake) -> (i32, String, String) {
    dispatch_with(args, fake, &|| true)
}

/// [`dispatch_with`] proving elevation is never checked — for the read-only
/// subcommands.
fn dispatch_read_only(args: &[&str], fake: &Fake) -> (i32, String, String) {
    dispatch_with(args, fake, &never_elevated)
}

/// Wraps a `Fake` with failures no `Fake` state can produce on its own.
///
/// - `fail_enable_for`/`fail_start_for` exercise `install`'s
///   post-`enable`/`start` error-reporting branches: a plain `Fake`'s own
///   errors are always `NotInstalled`, which cannot happen immediately after
///   a successful install, so those branches are otherwise unreachable.
/// - `fail_list` is the "the manager answered, but the listing did not"
///   path.
/// - `unqueryable` models a unit that decodes but whose *live state* cannot
///   be read: `list` reports it as `OursUnreadable` while `status` fails
///   with `CommandFailed`, exactly as a real backend does. That asymmetry is
///   the one the JSON envelope's `unreadable` kind exists to close.
#[derive(Clone, Default)]
struct FlakyManager {
    inner: Fake,
    fail_enable_for: Option<String>,
    fail_start_for: Option<String>,
    fail_list: bool,
    unqueryable: Option<String>,
}

/// The failure [`FlakyManager`] injects for `fail_list`/`unqueryable`: the
/// shape a real backend produces when the tool it shells out to cannot
/// answer.
fn unqueryable_failure() -> goetia::Error {
    goetia::Error::CommandFailed {
        command: "query-live-state".to_string(),
        stderr: "live state unavailable (injected test failure)".to_string(),
    }
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
        if self.unqueryable.as_deref() == Some(id.as_str()) {
            return Err(unqueryable_failure());
        }
        self.inner.status(id)
    }
    fn list(&self) -> goetia::Result<Vec<Installed>> {
        if self.fail_list {
            return Err(unqueryable_failure());
        }
        let mut installed = self.inner.list()?;
        for entry in &mut installed {
            if matches!(entry, Installed::Ours { spec, .. } if Some(spec.id.as_str()) == self.unqueryable.as_deref()) {
                *entry = Installed::OursUnreadable {
                    name: self.unqueryable.clone().expect("matched against Some just above"),
                    reason: unqueryable_failure().to_string(),
                };
            }
        }
        Ok(installed)
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

// Per-verb wiring: uninstall/start/stop/restart/enable/disable/status/diff/list =======================================

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

/// `status` with no ids must render each entry through the exact same line
/// shape as `status <id>` — `pid` included — not a second, `pid`-less shape
/// of its own.
#[skuld::test]
fn status_with_no_ids_prints_the_pid_like_the_per_id_form() {
    let fake = Fake::new();
    let spec = mk("frpc");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id).unwrap();

    let (_, per_id, _) = dispatch_read_only(&["goetia", "daemon", "status", "frpc"], &fake);
    let (_, no_ids, _) = dispatch_read_only(&["goetia", "daemon", "status"], &fake);

    assert!(per_id.contains("pid="), "{per_id}");
    assert_eq!(
        per_id.trim(),
        no_ids.trim(),
        "status with no ids must print the same line as status <id>"
    );
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

// Foreign-id refusal and unreadable-entry regression coverage =========================================================

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

    // `4`, not `1`: an id goetia owns but cannot report on is the
    // partial-answer case, and the code is the same with or without
    // `--json` — see `cli::report::exit_code`.
    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
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

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
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
        fail_enable_for: Some("frpc".to_string()),
        fail_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, out, err) = dispatch_with(
        &[
            "goetia",
            "daemon",
            "install",
            "-f",
            manifest.to_str().unwrap(),
            "--enable",
            "--start",
        ],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("enable"), "{err}");
    assert!(err.contains("start"), "{err}");
}

/// `-v`/`-q` are still reserved (see `Cli`'s doc comments) rather than wired
/// to per-subcommand output — unlike `--json`, which the tests below cover.
/// Pinned here so that reservation is itself a tested, deliberate state: a
/// change to `list`'s output with either flag present would need to update
/// this test, rather than silently landing unnoticed in either direction.
#[skuld::test]
fn cli_accepts_verbose_and_quiet_as_currently_inert() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (plain_code, plain_out, _) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);
    let (flagged_code, flagged_out, _) = dispatch_read_only(&["goetia", "-v", "-q", "daemon", "list"], &fake);

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

    assert_eq!(code, 4, "{err}");
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

// --json ==============================================================================================================

/// Parse the one document `--json` promises: exactly one line on stdout,
/// newline-terminated, and nothing else. Every assertion below is made
/// against the parsed value, never against the raw text.
fn parse_json(stdout: &str) -> serde_json::Value {
    assert!(
        stdout.ends_with('\n'),
        "the document must be newline-terminated: {stdout:?}"
    );
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout must carry exactly one document: {stdout:?}"
    );
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout:?}"))
}

fn daemons(doc: &serde_json::Value) -> &[serde_json::Value] {
    doc["daemons"]
        .as_array()
        .unwrap_or_else(|| panic!("`daemons` must always be present, as an array: {doc}"))
}

fn errors(doc: &serde_json::Value) -> &[serde_json::Value] {
    doc["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("`errors` must always be present, as an array: {doc}"))
}

fn daemon_ids(doc: &serde_json::Value) -> Vec<String> {
    field_strings(daemons(doc), "id")
}

fn error_kinds(doc: &serde_json::Value) -> Vec<String> {
    field_strings(errors(doc), "kind")
}

fn field_strings(entries: &[serde_json::Value], field: &str) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            entry[field]
                .as_str()
                .unwrap_or_else(|| panic!("`{field}` must be a string: {entry}"))
                .to_string()
        })
        .collect()
}

/// `--json` is `global = true`, so it is accepted on either side of the
/// subcommand and means the same thing in both places.
#[skuld::test]
fn json_is_accepted_before_or_after_the_subcommand() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (before_code, before_out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);
    let (after_code, after_out, _) = dispatch_read_only(&["goetia", "daemon", "list", "--json"], &fake);

    assert_eq!(before_code, after_code);
    assert_eq!(parse_json(&before_out), parse_json(&after_out));
}

#[skuld::test]
fn list_json_emits_every_managed_daemon() {
    let fake = Fake::new();
    fake.install(&mk("websocat"), false).unwrap();
    fake.install(&mk("frpc"), false).unwrap();
    fake.start(&Id::try_from("frpc").unwrap()).unwrap();
    fake.enable(&Id::try_from("frpc").unwrap()).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    assert_eq!(code, 0, "{out}");
    let doc = parse_json(&out);
    assert_eq!(daemon_ids(&doc), ["frpc", "websocat"], "entries must be sorted by id");
    assert_eq!(daemons(&doc)[0]["state"], "running");
    assert_eq!(daemons(&doc)[0]["enabled"], true);
    assert_eq!(daemons(&doc)[0]["pid"], 1);
    assert_eq!(daemons(&doc)[1]["state"], "stopped");
    assert_eq!(daemons(&doc)[1]["enabled"], false);
    assert_eq!(daemons(&doc)[1]["pid"], serde_json::Value::Null);
    assert!(errors(&doc).is_empty(), "{doc}");
}

#[skuld::test]
fn list_json_with_no_daemons_emits_empty_arrays() {
    let fake = Fake::new();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    assert_eq!(code, 0, "{out}");
    let doc = parse_json(&out);
    assert!(daemons(&doc).is_empty(), "{doc}");
    assert!(errors(&doc).is_empty(), "{doc}");
}

#[skuld::test]
fn list_json_reports_an_unreadable_entry_as_kind_unreadable() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.install(&mk("readable"), false).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    assert_eq!(
        code, 4,
        "an id goetia owns but cannot report on is the partial-answer case: {out}"
    );
    let doc = parse_json(&out);
    assert_eq!(
        daemon_ids(&doc),
        ["readable"],
        "the readable entry must still be reported"
    );
    assert_eq!(error_kinds(&doc), ["unreadable"]);
    assert_eq!(errors(&doc)[0]["id"], "corrupt");
}

/// A verb whose exit code depends on its output format is exactly the split
/// the envelope exists to remove.
#[skuld::test]
fn list_exit_code_is_the_same_with_and_without_json() {
    let clean = Fake::new();
    clean.install(&mk("frpc"), false).unwrap();
    let unreadable = Fake::new();
    unreadable.seed_unreadable("corrupt");

    for (fake, expected) in [(&clean, 0), (&unreadable, 4)] {
        let (plain, _, _) = dispatch_read_only(&["goetia", "daemon", "list"], fake);
        let (json, _, _) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], fake);

        assert_eq!(plain, json, "the exit code must not depend on the output format");
        assert_eq!(plain, expected);
    }
}

/// Without `--json`, a manager that cannot be obtained leaves stdout empty,
/// which `json.loads` would choke on. `--json` emits a well-formed document
/// carrying one `unavailable` error instead.
#[skuld::test]
fn list_json_emits_a_document_when_the_manager_is_unavailable() {
    let (code, out, err) = dispatch_get_manager(
        &["goetia", "--json", "daemon", "list"],
        &|| {
            Err(goetia::Error::UnsupportedPlatform {
                platform: "hal9000".to_string(),
            })
        },
        &never_elevated,
    );

    assert_eq!(
        code, 1,
        "no answer was obtained for any daemon: nothing partial about it: {out}"
    );
    let doc = parse_json(&out);
    assert!(daemons(&doc).is_empty(), "{doc}");
    assert_eq!(error_kinds(&doc), ["unavailable"]);
    assert_eq!(errors(&doc)[0]["id"], serde_json::Value::Null);
    assert!(
        errors(&doc)[0]["message"].as_str().unwrap().contains("hal9000"),
        "the message must carry the underlying failure: {doc}"
    );
    assert!(err.is_empty(), "stderr:\n{err}");
}

#[skuld::test]
fn list_json_emits_a_document_when_list_fails() {
    let mgr = FlakyManager {
        fail_list: true,
        ..Default::default()
    };

    let (code, out, _err) = dispatch_with(&["goetia", "--json", "daemon", "list"], &mgr, &never_elevated);

    assert_eq!(code, 1, "{out}");
    let doc = parse_json(&out);
    assert!(daemons(&doc).is_empty(), "{doc}");
    assert_eq!(error_kinds(&doc), ["unavailable"]);
    assert_eq!(errors(&doc)[0]["id"], serde_json::Value::Null);
}

/// The channel rule: with `--json`, the document is all of stdout and the
/// subcommand writes no diagnostics of its own to stderr — not even the
/// unreadable-entry warning its text mode prints.
#[skuld::test]
fn list_json_is_the_only_thing_on_stdout_and_stderr_stays_empty() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.install(&mk("readable"), false).unwrap();

    let (code, out, err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    parse_json(&out);
    assert!(err.is_empty(), "stderr:\n{err}");
}

#[skuld::test]
fn status_json_reports_state_enabled_and_pid_for_a_named_id() {
    let fake = Fake::new();
    let spec = mk("frpc");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id).unwrap();
    fake.enable(&spec.id).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "frpc"], &fake);

    assert_eq!(code, 0, "{out}");
    let doc = parse_json(&out);
    assert_eq!(daemon_ids(&doc), ["frpc"]);
    assert_eq!(daemons(&doc)[0]["state"], "running");
    assert_eq!(daemons(&doc)[0]["enabled"], true);
    assert_eq!(daemons(&doc)[0]["pid"], 1);
    assert!(errors(&doc).is_empty(), "{doc}");
}

/// A null `pid` means the manager reports no main process — never "this
/// command could not find out", which is an `errors` entry instead.
#[skuld::test]
fn status_json_reports_a_null_pid_for_a_stopped_daemon() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "frpc"], &fake);

    assert_eq!(code, 0, "{out}");
    let doc = parse_json(&out);
    assert_eq!(daemons(&doc)[0]["state"], "stopped");
    assert_eq!(daemons(&doc)[0]["pid"], serde_json::Value::Null);
}

/// The two subcommands must describe one machine identically, or a consumer
/// has to know which verb produced the document it is reading.
#[skuld::test]
fn status_json_with_no_ids_equals_list_json() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.start(&Id::try_from("frpc").unwrap()).unwrap();
    fake.install(&mk("websocat"), false).unwrap();
    fake.seed_unreadable("corrupt");

    let (list_code, list_out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);
    let (status_code, status_out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "status"], &fake);

    assert_eq!(list_code, status_code);
    assert_eq!(parse_json(&list_out), parse_json(&status_out));
}

/// Three states with opposite remedies, which text mode renders as one
/// undifferentiated `error:` line each.
#[skuld::test]
fn status_json_distinguishes_not_installed_foreign_and_unreadable() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");
    fake.seed_unreadable("corrupt");

    let (code, out, _err) = dispatch_read_only(
        &["goetia", "--json", "daemon", "status", "missing", "stranger", "corrupt"],
        &fake,
    );

    let doc = parse_json(&out);
    assert_eq!(
        error_kinds(&doc),
        ["not-installed", "foreign", "unreadable"],
        "in argument order"
    );
    assert_eq!(field_strings(errors(&doc), "id"), ["missing", "stranger", "corrupt"]);
    assert!(daemons(&doc).is_empty(), "{doc}");
    assert_eq!(code, 1, "`not-installed`'s 1 outranks `unreadable`'s 4: {out}");
}

/// The disagreement this task exists to close: for a unit that decodes but
/// whose live state cannot be queried, `list` sees `Installed::OursUnreadable`
/// and `status` sees `Error::CommandFailed`. Both must report `unreadable`.
#[skuld::test]
fn list_and_status_report_the_same_kind_for_an_unqueryable_daemon() {
    let inner = Fake::new();
    inner.install(&mk("frpc"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        unqueryable: Some("frpc".to_string()),
        ..Default::default()
    };

    let (list_code, list_out, _) = dispatch_with(&["goetia", "--json", "daemon", "list"], &mgr, &never_elevated);
    let (status_code, status_out, _) =
        dispatch_with(&["goetia", "--json", "daemon", "status", "frpc"], &mgr, &never_elevated);

    assert_eq!(error_kinds(&parse_json(&list_out)), ["unreadable"], "{list_out}");
    assert_eq!(error_kinds(&parse_json(&status_out)), ["unreadable"], "{status_out}");
    assert_eq!(list_code, 4);
    assert_eq!(status_code, 4);
}

/// `parse_id` rejecting an argument and blob-content validation both produce
/// `Error::Invalid`, so the kind comes from which operation failed, not from
/// the variant: this one is `invalid-id`, and the one in
/// `status_json_distinguishes_not_installed_foreign_and_unreadable` is not.
#[skuld::test]
fn status_json_reports_an_invalid_id_as_kind_invalid_id() {
    let fake = Fake::new();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "not/a/valid/id"], &fake);

    assert_eq!(code, 1, "something ran, so this is not the parser's `2`: {out}");
    let doc = parse_json(&out);
    assert_eq!(error_kinds(&doc), ["invalid-id"]);
    assert_eq!(errors(&doc)[0]["id"], "not/a/valid/id");
}

#[skuld::test]
fn status_json_mixes_good_and_bad_ids() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, _err) = dispatch_read_only(
        &["goetia", "--json", "daemon", "status", "frpc", "not/a/valid/id"],
        &fake,
    );

    assert_eq!(code, 1, "{out}");
    let doc = parse_json(&out);
    assert_eq!(daemon_ids(&doc), ["frpc"], "the good id must still be answered");
    assert_eq!(error_kinds(&doc), ["invalid-id"]);
}

/// The exit code is the precedence-max over `errors[].kind`, not "1 if
/// non-empty".
#[skuld::test]
fn json_exit_code_follows_the_precedence_rule() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");

    let (alone, alone_out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "corrupt"], &fake);
    assert_eq!(error_kinds(&parse_json(&alone_out)), ["unreadable"]);
    assert_eq!(alone, 4, "an unreadable entry alone is 4, neither 1 nor 0: {alone_out}");

    let (combined, combined_out, _) =
        dispatch_read_only(&["goetia", "--json", "daemon", "status", "corrupt", "missing"], &fake);
    assert_eq!(error_kinds(&parse_json(&combined_out)), ["unreadable", "not-installed"]);
    assert_eq!(combined, 1, "1 outranks 4: {combined_out}");
}

#[skuld::test]
fn json_exit_code_is_zero_exactly_when_errors_is_empty() {
    let clean = Fake::new();
    clean.install(&mk("frpc"), false).unwrap();
    let dirty = Fake::new();
    dirty.seed_unreadable("corrupt");

    for fake in [&clean, &dirty] {
        for args in [
            ["goetia", "--json", "daemon", "list"].as_slice(),
            ["goetia", "--json", "daemon", "status"].as_slice(),
        ] {
            let (code, out, _) = dispatch_read_only(args, fake);
            let doc = parse_json(&out);

            assert_eq!(code == 0, errors(&doc).is_empty(), "{args:?} exited {code}: {doc}");
        }
    }
}

/// `--json` on a subcommand that does not implement it is a refusal, not
/// silence: the invariant is that `--json` plus a subcommand always yields
/// one JSON document on stdout.
#[skuld::test]
fn json_on_an_unsupported_subcommand_is_refused_as_json() {
    let fake = Fake::new();

    let (code, out, err) = dispatch_read_only(&["goetia", "--json", "daemon", "install"], &fake);

    assert_eq!(code, 2, "a usage error, and nothing ran: {out}");
    let doc = parse_json(&out);
    assert!(daemons(&doc).is_empty(), "{doc}");
    assert_eq!(error_kinds(&doc), ["unsupported"]);
    assert_eq!(errors(&doc)[0]["id"], serde_json::Value::Null);
    assert!(
        errors(&doc)[0]["message"].as_str().unwrap().contains("install"),
        "the refusal must name the subcommand: {doc}"
    );
    assert!(err.is_empty(), "stderr:\n{err}");
}

#[skuld::test]
fn json_on_an_unsupported_subcommand_runs_nothing() {
    let (code, out, _err) = dispatch_get_manager(
        &["goetia", "--json", "daemon", "uninstall", "frpc"],
        &|| panic!("a refused --json subcommand must never obtain a manager"),
        &|| panic!("a refused --json subcommand must never check elevation"),
    );

    assert_eq!(code, 2, "{out}");
    assert_eq!(error_kinds(&parse_json(&out)), ["unsupported"]);
}
