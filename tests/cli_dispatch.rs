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
use std::time::Duration;

use clap::Parser as _;
use goetia::cli::{self, Cli, Command, DaemonCommand};
use goetia::manager::fake::Fake;
use goetia::manager::{Budget, Installed, ServiceManager, State, Status, Step};
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

// One `(Fake, manifest)` builder per `decide::Outcome` class `diff` can see,
// shared by every test that only needs that one outcome for a single
// daemon — the tests that mix several outcomes in one run build their own
// manifest instead, since what they exercise is precisely the combination.

/// `id` absent from the store: `diff` reports `Outcome::Create`.
fn absent_fixture(dir: &Path, id: &str) -> (Fake, PathBuf) {
    let fake = Fake::new();
    let manifest = write_manifest(dir, &format!("daemons:\n  {id}:\n    command: [daemon]\n"));
    (fake, manifest)
}

/// `id` installed with `restart: never`, against a manifest asking for
/// `restart: on-failure`: `diff` reports `Outcome::Update`, a clean spec
/// change with no hand-edit.
fn update_fixture(dir: &Path, id: &str) -> (Fake, PathBuf) {
    let fake = Fake::new();
    let mut old = mk(id);
    old.restart = Restart::Never;
    fake.install(&old, false).unwrap();
    let manifest = write_manifest(
        dir,
        &format!("daemons:\n  {id}:\n    command: [daemon]\n    restart: on-failure\n"),
    );
    (fake, manifest)
}

/// `id` installed then hand-edited outside Goetia: `diff` reports
/// `Outcome::Conflict`.
fn hand_edited_fixture(dir: &Path, id: &str) -> (Fake, PathBuf) {
    let fake = Fake::new();
    fake.install_then_hand_edit(&mk(id), "# hand-added directive\n");
    let manifest = write_manifest(dir, &format!("daemons:\n  {id}:\n    command: [daemon]\n"));
    (fake, manifest)
}

/// An unmarked entry at `id`: `diff` reports `Outcome::RefuseForeign`.
fn foreign_fixture(dir: &Path, id: &str) -> (Fake, PathBuf) {
    let fake = Fake::new();
    fake.seed_foreign(id, "not a goetia artifact at all\n");
    let manifest = write_manifest(dir, &format!("daemons:\n  {id}:\n    command: [daemon]\n"));
    (fake, manifest)
}

/// An undecodable Goetia-marked entry at `id`: `diff` reports
/// `Outcome::RefuseUnreadable`.
fn unreadable_fixture(dir: &Path, id: &str) -> (Fake, PathBuf) {
    let fake = Fake::new();
    fake.seed_unreadable(id);
    let manifest = write_manifest(dir, &format!("daemons:\n  {id}:\n    command: [daemon]\n"));
    (fake, manifest)
}

/// `id` installed with exactly `resolve()`'s own output for the manifest
/// below: `diff` reports `Outcome::UpToDate`. Installing a literal `mk(id)`
/// instead would never match: `resolve()` makes `command[0]` absolute
/// against the manifest's directory.
fn up_to_date_fixture(dir: &Path, id: &str) -> (Fake, PathBuf) {
    let fake = Fake::new();
    let manifest = write_manifest(
        dir,
        &format!("daemons:\n  {id}:\n    command: [{id}]\n    restart: on-failure\n"),
    );
    let (specs, _warnings) = goetia::spec::load(&manifest).expect("load fixture manifest");
    fake.install(&specs[0], false).expect("seed install");
    (fake, manifest)
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
/// - `fail_preview_for` is the "the manager could not even answer whether
///   this daemon would change" path — `Fake` itself can never fail
///   `preview_install`, so this is the only way to put a genuine `Err(_)`
///   in the same `diff` run as a `Conflict`/drift outcome.
/// - `hidden_from_list` models the opposite asymmetry from `unqueryable`:
///   an id `list()` cannot enumerate at all (as if this privilege level
///   lacked the read access needed to see it in a directory listing —
///   real backends can hit exactly this, e.g. an ACL denying an
///   unprivileged registry read), while `status(&id)` — a direct query by
///   name — still finds it. `Fake` itself has no such gap: its `list` and
///   `status` always agree, so this is the only way to open one.
#[derive(Clone, Default)]
struct FlakyManager {
    inner: Fake,
    fail_enable_for: Option<String>,
    fail_start_for: Option<String>,
    fail_list: bool,
    unqueryable: Option<String>,
    fail_preview_for: Option<String>,
    hidden_from_list: Option<String>,
    /// The same id as `hidden_from_list`, reported honestly: `list()` emits
    /// `Installed::Undetermined` for it instead of dropping it. The pair is
    /// what lets one fixture show both the old silence and the new answer.
    undetermined_in_list: Option<String>,
    /// `stop` succeeds and `start` then fails with `Error::Undetermined`.
    /// The only order in which `restart`'s closure re-wraps the start leg's
    /// failure, and so the only way to reach that re-wrap with a variant
    /// that has to survive it. Also `install --start`'s own leg, which
    /// classifies its failure itself rather than inheriting anything from
    /// the `install` that preceded it.
    undetermined_start_for: Option<String>,
    /// `enable` fails with `Error::Undetermined` — the `--enable` twin of
    /// `undetermined_start_for`, and a third independent decision:
    /// `install`'s three `failure_code` call sites each classify their own
    /// step.
    undetermined_enable_for: Option<String>,
    /// `start` fails with `Error::WaitTimeout`. A fourth independent
    /// decision, and a different *condition* from the two above: the
    /// request was issued and accepted, and goetia stopped waiting for its
    /// outcome. `Fake` reaches this only for an entry seeded stalled, which
    /// no `install`-path fixture has, so injecting it is the only way to
    /// put it in front of `run_id_verb` and `install`'s own classifier.
    wait_timeout_start_for: Option<String>,
    /// Makes `install`/`preview_install` return the `Conflict` flavour whose
    /// cause lies outside the directory the backend writes — systemd's
    /// `<id>.service.d` under `/usr/lib` or `.control`. `Fake` has no
    /// overlay of its own (see its `decide` call site), so this is the only
    /// way to reach the branch where `--force` is not the remedy.
    unclearable_conflict: Option<String>,
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

/// A wait that ran out of budget. Built through the **real**
/// `budget::timed_out` rather than a hand-written `Error::WaitTimeout`, so
/// these tests pin the constructor every backend actually reports through,
/// not a look-alike that could drift from it.
fn injected_wait_timeout(id: &Id) -> goetia::Error {
    goetia::manager::budget::timed_out(id.as_str(), "running", goetia::manager::Budget::DEFAULT)
}

fn injected_failure(id: &Id) -> goetia::Error {
    goetia::Error::NotInstalled {
        id: format!("{id} (injected test failure)"),
    }
}

/// The failure a backend produces when the read that would have said what
/// is at `id` did not complete: neither absence nor presence established.
fn injected_indeterminacy(id: &Id) -> goetia::Error {
    goetia::Error::Undetermined {
        id: id.as_str().to_string(),
        reason: "the artifact could not be read (injected test failure)".to_string(),
        recovery: "re-run with enough privilege to read the artifact".to_string(),
    }
}

/// What [`FlakyManager::undetermined_in_list`] reports for the id `list()`
/// could not enumerate.
const UNDETERMINED_IN_LIST_REASON: &str = "could not be enumerated at this privilege level (injected test failure)";

/// The `Conflict` [`FlakyManager::unclearable_conflict`] injects.
fn unclearable_conflict(recovery: &str) -> goetia::decide::Outcome {
    goetia::decide::Outcome::Conflict {
        artifact_diff: "- desired\n+ on disk\n".to_string(),
        unclearable_recovery: Some(recovery.to_string()),
    }
}

impl FlakyManager {
    /// The failure a `start`-shaped call is seeded to report — shared by `start` and
    /// `request_start_after_stop`, which are two ways of asking for the same thing.
    fn injected_start(&self, id: &Id) -> goetia::Result<()> {
        if self.fail_start_for.as_deref() == Some(id.as_str()) {
            return Err(injected_failure(id));
        }
        if self.undetermined_start_for.as_deref() == Some(id.as_str()) {
            return Err(injected_indeterminacy(id));
        }
        if self.wait_timeout_start_for.as_deref() == Some(id.as_str()) {
            return Err(injected_wait_timeout(id));
        }
        Ok(())
    }
}

impl ServiceManager for FlakyManager {
    fn install(&self, spec: &DaemonSpec, force: bool) -> goetia::Result<goetia::decide::Outcome> {
        if let Some(recovery) = &self.unclearable_conflict {
            return Ok(unclearable_conflict(recovery));
        }
        self.inner.install(spec, force)
    }
    fn preview_install(&self, spec: &DaemonSpec) -> goetia::Result<goetia::decide::Outcome> {
        if self.fail_preview_for.as_deref() == Some(spec.id.as_str()) {
            return Err(injected_failure(&spec.id));
        }
        if let Some(recovery) = &self.unclearable_conflict {
            return Ok(unclearable_conflict(recovery));
        }
        self.inner.preview_install(spec)
    }
    fn uninstall(&self, id: &Id) -> goetia::Result<()> {
        self.inner.uninstall(id)
    }
    fn enable(&self, id: &Id) -> goetia::Result<()> {
        if self.fail_enable_for.as_deref() == Some(id.as_str()) {
            return Err(injected_failure(id));
        }
        if self.undetermined_enable_for.as_deref() == Some(id.as_str()) {
            return Err(injected_indeterminacy(id));
        }
        self.inner.enable(id)
    }
    fn disable(&self, id: &Id) -> goetia::Result<()> {
        self.inner.disable(id)
    }
    fn start(&self, id: &Id, budget: Budget) -> goetia::Result<()> {
        self.injected_start(id)?;
        self.inner.start(id, budget)
    }
    fn request_start_after_stop(&self, id: &Id) -> goetia::Result<()> {
        self.injected_start(id)?;
        self.inner.request_start_after_stop(id)
    }
    fn stop(&self, id: &Id, budget: Budget) -> goetia::Result<()> {
        self.inner.stop(id, budget)
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
        if let Some(hidden) = &self.hidden_from_list {
            installed.retain(|entry| match entry {
                Installed::Ours { spec, .. } => spec.id.as_str() != hidden,
                Installed::OursUnreadable { name, .. } => name != hidden,
                // A `None` name stands for ids this entry could not
                // separate, so it says nothing about `hidden` and is kept.
                // Dropping it here would be the fixture concluding the very
                // absence the entry exists to deny.
                Installed::Undetermined { name, .. } => name.as_deref() != Some(hidden.as_str()),
            });
        }
        // After the `retain`, deliberately: this is the same id reported
        // rather than dropped, so the filter above must not take it back
        // out.
        if let Some(name) = &self.undetermined_in_list {
            installed.push(Installed::Undetermined {
                name: Some(name.clone()),
                reason: UNDETERMINED_IN_LIST_REASON.to_string(),
            });
        }
        Ok(installed)
    }
}

fn installed_ids(fake: &Fake) -> Vec<String> {
    let mut ids: Vec<String> = fake
        .list()
        .unwrap()
        .into_iter()
        .map(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str().to_string(),
            // Present, and goetia's — only its blob would not decode. Three
            // callers read `is_empty()` as "nothing is installed", and
            // dropping this made that flatly false rather than merely
            // unsound: the id demonstrably *is* installed. `Fake` keys the
            // entry by the id it was seeded with, so the name is the answer.
            Installed::OursUnreadable { name, .. } => name,
            // Three callers ask this `is_empty()` and read the answer as
            // "nothing is installed". That conclusion is unsound while an
            // undetermined entry is present — it may stand for the very id
            // being asked about — so the question is refused rather than
            // answered wrongly. No fixture produces one today; this exists so
            // that one cannot arrive silently.
            Installed::Undetermined { name, reason } => panic!(
                "installed_ids cannot answer while an entry is undetermined \
                 (name: {name:?}): {reason}"
            ),
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

/// The remedy `--force` is published as has to be one that works. When part of what makes the
/// artifact differ is outside the directory the backend writes, forcing rewrites the artifact,
/// leaves that part in place, and the next run reports the identical conflict — so the message must
/// name what to remove instead of sending the operator back around that loop. Exit `5` either way:
/// it is a conflict in both, and the distinction is the remedy, not the code.
#[skuld::test]
fn install_offers_force_only_for_a_conflict_force_can_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let args = ["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()];

    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");
    let (code, out, err) = dispatch_with(&args, &fake, &|| true);
    assert_eq!(code, 5, "stdout:\n{out}");
    assert!(out.contains("conflict (re-run with --force to overwrite)"), "{out}");
    assert!(err.contains("conflict (re-run with --force to overwrite)"), "{err}");

    let recovery = "remove /usr/lib/systemd/system/frpc.service.d by hand and re-run";
    let mgr = FlakyManager {
        unclearable_conflict: Some(recovery.to_string()),
        ..Default::default()
    };
    let (code, out, err) = dispatch_with(&args, &mgr, &|| true);
    assert_eq!(
        code, 5,
        "still a conflict, and still force-resolvable-looking to a script:\n{out}"
    );
    assert!(out.contains(recovery), "{out}");
    assert!(err.contains(recovery), "{err}");
    assert!(
        !out.contains("re-run with --force to overwrite"),
        "`--force` rewrites the artifact and leaves the cause, so offering it wedges the operator:\n{out}"
    );
}

/// `diff` renders the same two flavours in the subjunctive, and must not offer what `install` will
/// not honour.
#[skuld::test]
fn diff_offers_force_only_for_a_conflict_force_can_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let args = ["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()];

    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");
    let (code, out, _err) = dispatch_read_only(&args, &fake);
    assert_eq!(code, 5, "stdout:\n{out}");
    assert!(out.contains("`install --force` would overwrite it"), "{out}");

    let recovery = "remove /etc/systemd/system.control/frpc.service.d by hand and re-run";
    let mgr = FlakyManager {
        unclearable_conflict: Some(recovery.to_string()),
        ..Default::default()
    };
    let (code, out, err) = dispatch_with(&args, &mgr, &never_elevated);
    assert_eq!(code, 5, "stdout:\n{out}");
    assert!(out.contains(recovery), "{out}");
    assert!(err.contains(recovery), "{err}");
    assert!(
        !out.contains("would overwrite it"),
        "diff must not promise an overwrite that resolves nothing:\n{out}"
    );
}

#[skuld::test]
fn conflict_exits_five() {
    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");

    let (code, out, _err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 5, "stdout:\n{out}");
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

/// The per-id counterpart to `show_from_file_and_show_from_installed_agree`
/// above: with two daemons installed from one manifest, naming each one
/// individually still renders byte-identical output through either path,
/// and neither path checks elevation. This is a commitment test over
/// already-correct behaviour — both paths render through the same
/// `crate::diff::render_yaml` today — so it is expected to pass on first
/// write; its job is to make that agreement a declared contract rather than
/// an accident, so a renderer added to only one path in the future fails
/// it.
#[skuld::test]
fn show_per_id_from_file_and_from_installed_agree() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc, -c, frpc.toml]\n    restart: on-failure\n  websocat:\n    command: [websocat]\n",
    );
    let fake = Fake::new();
    let (install_code, _, install_err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );
    assert_eq!(install_code, 0, "{install_err}");

    for id in ["frpc", "websocat"] {
        let (code_file, out_file, _) = dispatch_read_only(
            &["goetia", "daemon", "show", "-f", manifest.to_str().unwrap(), id],
            &fake,
        );
        let (code_installed, out_installed, _) = dispatch_read_only(&["goetia", "daemon", "show", id], &fake);

        assert_eq!(code_file, 0);
        assert_eq!(code_installed, 0);
        assert_eq!(
            out_file, out_installed,
            "show -f {id} and show {id} (from installed) must render identically"
        );
    }
}

/// `show` without `-f` only ever consults `list()` — never `status(&id)`
/// directly, unlike `status` itself — so a daemon this privilege level
/// cannot enumerate is one `show` cannot render even though a direct query
/// would find it. What it may not do is call that daemon absent.
#[skuld::test]
fn show_reports_indeterminacy_for_a_daemon_list_cannot_see() {
    let inner = Fake::new();
    inner.install(&mk("ghost"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        hidden_from_list: Some("ghost".to_string()),
        undetermined_in_list: Some("ghost".to_string()),
        ..Default::default()
    };

    // `status()` can see it directly...
    assert!(mgr.status(&Id::try_from("ghost").unwrap()).is_ok());

    // ...but `show`, which only calls `list()`, cannot.
    let (code, out, err) = dispatch_with(&["goetia", "daemon", "show", "ghost"], &mgr, &never_elevated);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, "");
    assert!(
        !err.contains("not installed"),
        "the listing establishes no absence: {err}"
    );
    assert!(
        !err.contains("not managed by goetia"),
        "nor anyone else's ownership: {err}"
    );
    assert!(err.contains("ghost"), "{err}");
}

/// Both of `show`'s "installed but unreadable" paths — naming the id
/// directly, and asking for everything with none named — must return `4`
/// (indeterminate: goetia owns this id and could not determine its state),
/// distinguished from the `1` a genuinely absent id returns rather than
/// merely being non-zero.
#[skuld::test]
fn show_exits_four_for_an_installed_but_unreadable_daemon() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.install(&mk("readable"), false).unwrap();

    // Per-id form. `err` also carries the unconditional unreadable-entry
    // warning `print_unreadable_warnings` prints for every form (see
    // `list_reports_an_unreadable_entry_and_exits_nonzero`), so this checks
    // for the per-id error line rather than asserting `err` in full.
    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "show", "corrupt"], &fake);
    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, "", "nothing to render for an id with no decodable spec: {out}");
    assert!(
        err.contains("error: daemon `corrupt` is installed but unreadable"),
        "{err}"
    );

    // No-ids form: still escalates from `index.unreadable`, and still
    // renders the one daemon it *can* read.
    let (code_all, out_all, err_all) = dispatch_read_only(&["goetia", "daemon", "show"], &fake);
    assert_eq!(code_all, 4, "stdout:\n{out_all}\nstderr:\n{err_all}");
    assert!(out_all.contains("# readable"), "{out_all}");
    assert!(
        !out_all.contains("corrupt"),
        "an unreadable entry has no spec to show: {out_all}"
    );
    assert!(err_all.contains("corrupt"), "{err_all}");
    assert!(err_all.contains("unreadable"), "{err_all}");

    // Distinguished from a genuinely absent id, which stays at `1` — a
    // determinate answer, not an indeterminate one.
    let (code_absent, _, err_absent) = dispatch_read_only(&["goetia", "daemon", "show", "nonexistent"], &fake);
    assert_eq!(code_absent, 1, "{err_absent}");
    assert_ne!(code, code_absent, "unreadable (4) must not collapse into absent (1)");
}

/// Splitting the "unreadable" and "not installed" branches onto different
/// codes (`4` vs `1`) means a `show` call naming both must pick one
/// consistently — not whichever happened to run last. `1` outranks `4`
/// here, matching `cli::dispatch`'s published precedence rule
/// (`1 > 4 > 5 > 3 > 0`), regardless of which id was named first.
#[skuld::test]
fn show_absent_id_outranks_unreadable_id_regardless_of_argument_order() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");

    let (code_a, _, _) = dispatch_read_only(&["goetia", "daemon", "show", "corrupt", "nonexistent"], &fake);
    let (code_b, _, _) = dispatch_read_only(&["goetia", "daemon", "show", "nonexistent", "corrupt"], &fake);

    assert_eq!(code_a, 1);
    assert_eq!(code_b, 1);
}

// backend-specific overrides through the CLI ==========================================================================
//
// `native()` is `Some` on every CI platform
// (`native_backend_agrees_with_the_platforms_that_have_a_service_manager`
// guarantees it), so these fixtures read the native backend at test time
// rather than special-casing per platform, and skipping is neither needed
// nor permitted.

/// A backend that is not this host's native one. `Backend::ALL` always has
/// at least two such entries (`Backend::native()` names at most one), so
/// the first match is deterministic and always exists. Mirrors
/// `non_native_backend` in `src/spec/resolve_tests.rs`.
fn non_native_backend() -> goetia::spec::Backend {
    goetia::spec::Backend::ALL
        .into_iter()
        .find(|&b| Some(b) != goetia::spec::Backend::native())
        .expect("ALL has 3 entries; native() names at most 1")
}

/// The blob install writes stores the *merged* spec, not the base one:
/// `restart` changes only under the native backend's override, and `show`
/// with no `-f` — reading only the installed metadata blob, never the
/// manifest — reports the overridden value.
#[skuld::test]
fn install_applies_the_native_backend_override() {
    let dir = tempfile::tempdir().unwrap();
    let native = goetia::spec::Backend::native().expect("native() is Some on every CI platform");
    let fake = Fake::new();
    let manifest = write_manifest(
        dir.path(),
        &format!(
            "daemons:\n  frpc:\n    command: [frpc]\n    restart: never\n    backend-specific:\n      {native}:\n        \
             restart: always\n"
        ),
    );

    let (install_code, install_out, install_err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );
    assert_eq!(install_code, 0, "stdout:\n{install_out}\nstderr:\n{install_err}");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "show"], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("restart: always"),
        "show (no -f) must read the merged spec back out of the installed blob:\n{out}"
    );
}

/// Editing only a non-native backend's override between two installs is
/// inert for drift: the second `install` reports `up to date`, because
/// `merged_for` never reads a non-native override into the spec that gets
/// installed.
#[skuld::test]
fn changing_only_a_non_native_override_leaves_the_install_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let native = goetia::spec::Backend::native().expect("native() is Some on every CI platform");
    let non_native = non_native_backend();
    let fake = Fake::new();
    let manifest_text = |non_native_delay: &str| {
        format!(
            "daemons:\n  frpc:\n    command: [frpc]\n    backend-specific:\n      {native}:\n        restart: always\n      \
             {non_native}:\n        restart-delay: {non_native_delay}\n"
        )
    };
    let manifest = write_manifest(dir.path(), &manifest_text("2s"));

    let (first_code, first_out, first_err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );
    assert_eq!(first_code, 0, "stdout:\n{first_out}\nstderr:\n{first_err}");

    write_manifest(dir.path(), &manifest_text("3s"));
    let (code, out, err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("up to date"),
        "a non-native override's edit must not be seen as drift:\n{out}"
    );
}

/// The same shape as
/// `changing_only_a_non_native_override_leaves_the_install_up_to_date`, but
/// editing the *native* override between the two installs: the second
/// `install` reports `updated`.
#[skuld::test]
fn changing_the_native_override_is_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let native = goetia::spec::Backend::native().expect("native() is Some on every CI platform");
    let fake = Fake::new();
    let manifest_text = |restart: &str| {
        format!(
            "daemons:\n  frpc:\n    command: [frpc]\n    backend-specific:\n      {native}:\n        restart: {restart}\n"
        )
    };
    let manifest = write_manifest(dir.path(), &manifest_text("always"));

    let (first_code, first_out, first_err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );
    assert_eq!(first_code, 0, "stdout:\n{first_out}\nstderr:\n{first_err}");

    write_manifest(dir.path(), &manifest_text("on-failure"));
    let (code, out, err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("updated"),
        "a native override's edit must be seen as drift:\n{out}"
    );
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

// Absence and `uninstall`'s exemption =================================================================================
//
// `Error::NotInstalled` says the artifact is absent; it says nothing about
// the process. Only `uninstall` treats that as success — see
// `IdVerbCall::absent_is_success`'s doc comment for the full table this
// section pins one row of at a time.

#[skuld::test]
fn uninstall_exits_zero_when_the_daemon_was_already_absent() {
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "ghost"], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, "ghost: not installed (nothing to do)\n");
    assert_eq!(err, "");
}

#[skuld::test]
fn stop_exits_one_when_the_daemon_is_absent() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "stop", "ghost"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("ghost"), "{err}");
}

#[skuld::test]
fn disable_exits_one_when_the_daemon_is_absent() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "disable", "ghost"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("ghost"), "{err}");
}

#[skuld::test]
fn start_exits_one_when_the_daemon_is_absent() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "start", "ghost"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("ghost"), "{err}");
}

#[skuld::test]
fn enable_exits_one_when_the_daemon_is_absent() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "enable", "ghost"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("ghost"), "{err}");
}

#[skuld::test]
fn restart_exits_one_when_the_daemon_is_absent() {
    let fake = Fake::new();

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "ghost"], &fake);

    assert_eq!(code, 1);
    assert!(err.contains("ghost"), "{err}");
}

/// A manager whose `start` panics if called: proof that `restart`'s
/// closure short-circuits on `mgr.stop(id)?` and never falls through to
/// `start` for an id `stop` could not act on. If the absence tolerance
/// were ever implemented inside `stop` itself rather than on
/// `IdVerbCall`, `restart` on an absent id would reach this `start` and
/// panic.
#[derive(Clone, Default)]
struct PanicsOnStart(Fake);

impl ServiceManager for PanicsOnStart {
    fn install(&self, spec: &DaemonSpec, force: bool) -> goetia::Result<goetia::decide::Outcome> {
        self.0.install(spec, force)
    }
    fn preview_install(&self, spec: &DaemonSpec) -> goetia::Result<goetia::decide::Outcome> {
        self.0.preview_install(spec)
    }
    fn uninstall(&self, id: &Id) -> goetia::Result<()> {
        self.0.uninstall(id)
    }
    fn enable(&self, id: &Id) -> goetia::Result<()> {
        self.0.enable(id)
    }
    fn disable(&self, id: &Id) -> goetia::Result<()> {
        self.0.disable(id)
    }
    fn start(&self, _id: &Id, _budget: Budget) -> goetia::Result<()> {
        panic!("restart on an absent id must never reach start")
    }
    fn request_start_after_stop(&self, _id: &Id) -> goetia::Result<()> {
        panic!("restart on an absent id must never reach start")
    }
    fn stop(&self, id: &Id, budget: Budget) -> goetia::Result<()> {
        self.0.stop(id, budget)
    }
    fn status(&self, id: &Id) -> goetia::Result<Status> {
        self.0.status(id)
    }
    fn list(&self) -> goetia::Result<Vec<Installed>> {
        self.0.list()
    }
}

#[skuld::test]
fn restart_on_an_absent_id_never_reaches_start() {
    let mgr = PanicsOnStart::default();

    let (code, _out, err) = dispatch_with(&["goetia", "daemon", "restart", "ghost"], &mgr, &|| true);

    assert_eq!(code, 1, "{err}");
}

#[skuld::test]
fn an_absent_id_is_reported_on_stdout_not_stderr() {
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "ghost"], &fake);

    assert_eq!(code, 0);
    assert!(out.contains("ghost"), "{out}");
    assert!(out.contains("not installed"), "{out}");
    assert_eq!(err, "", "nothing goes to stderr for an absent id: {err}");
    assert!(!out.contains("uninstalled"), "{out}");
    assert!(!err.contains("uninstalled"), "{err}");
}

#[skuld::test]
fn uninstall_exits_one_for_a_foreign_id() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "stranger"], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("stranger"), "{err}");
    assert!(!out.contains("not installed (nothing to do)"), "{out}");
}

#[skuld::test]
fn uninstall_exits_one_when_a_real_failure_accompanies_an_absent_id() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "ghost", "stranger"], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        out, "ghost: not installed (nothing to do)\n",
        "the absent id must still be reported as satisfied"
    );
    assert!(
        err.contains("stranger"),
        "the foreign id's failure must still be reported: {err}"
    );
}

#[skuld::test]
fn uninstall_still_exits_zero_when_every_id_was_removed() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "frpc", "ghost"], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("frpc: uninstalled"), "{out}");
    assert!(out.contains("ghost: not installed (nothing to do)"), "{out}");
    assert_eq!(err, "");
    assert!(installed_ids(&fake).is_empty());
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
    fake.start(&spec.id, Budget::DEFAULT).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "stop", "frpc"], &fake);

    assert_eq!(code, 0);
    assert_ne!(
        fake.status(&Id::try_from("frpc").unwrap()).unwrap().state,
        State::Running
    );
}

/// `--no-timeout`, not the default: under a bounded budget, `restart` reads the clock between its
/// legs, and a machine stalled for the whole budget there would abandon the start — a bet on time
/// this test has no reason to make.
#[skuld::test]
fn restart_reaches_the_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, _out, _err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc", "--no-timeout"], &fake);

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
    fake.start(&spec.id, Budget::DEFAULT).unwrap();

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
    fake.start(&spec.id, Budget::DEFAULT).unwrap();

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
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = update_fixture(dir.path(), "frpc");

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    // 3, not 0: an `Update` outcome is drift, matching `install`'s own
    // exit-code vocabulary (see `dispatch`'s doc comment).
    assert_eq!(code, 3);
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

/// Exit code 5 must mean "every failure here is force-resolvable". A batch
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

/// `--dry-run`'s preview must not silently drop a `Warning` (e.g. SCM
/// clamping a too-large `restart-delay`). This advisory now fires at
/// resolve time, from `Backend::Scm.warn`, through `load_and_warn` — on
/// every host, not only from an effectful Windows install preview — which
/// is the property this test now pins.
#[skuld::test]
fn install_dry_run_prints_generator_warnings() {
    let dir = tempfile::tempdir().unwrap();
    // A restart-delay past SC_ACTION.Delay's ~49.71-day DWORD-milliseconds
    // ceiling.
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
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("warning:"), "stderr:\n{err}");
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
/// (would be created)". `4`, not `1`: `RefuseUnreadable` is indeterminate —
/// `diff` was asked a question and could not determine the answer — unlike
/// `install`, which exits `1` here because it genuinely failed to install.
#[skuld::test]
fn diff_exits_four_when_the_installed_artifact_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = unreadable_fixture(dir.path(), "corrupt");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("unreadable"), "{err}");
    assert!(!out.contains("would be created"), "stdout:\n{out}");
}

#[skuld::test]
fn diff_reports_up_to_date_when_the_spec_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = up_to_date_fixture(dir.path(), "frpc");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("up to date"), "{out}");
}

#[skuld::test]
fn diff_reports_not_installed_for_an_id_absent_from_the_manager() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = absent_fixture(dir.path(), "frpc");

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    // 3, not 0: a `Create` outcome is drift.
    assert_eq!(code, 3);
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
/// artifact is a conflict, never "up to date". `5`, matching `install`'s
/// code for the identical outcome (conflict lives on `5`, never clap's `2`).
#[skuld::test]
fn diff_exits_five_for_a_hand_edited_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = hand_edited_fixture(dir.path(), "frpc");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 5, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("would conflict"), "{out}");
    assert!(!out.contains("up to date"), "{out}");
}

/// `diff` must distinguish "absent" from "occupied by a stranger's
/// service": both used to render identically as "would be created", which
/// `install` would then immediately contradict by refusing. A second,
/// drifting daemon in the same run must not lower the code to 3: `1`
/// outranks `3` in the precedence rule.
#[skuld::test]
fn diff_exits_one_for_a_foreign_id() {
    let fake = Fake::new();
    fake.seed_foreign("frpc", "not a goetia artifact at all\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n  drifting:\n    command: [daemon]\n",
    );

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    let frpc_line = out.lines().find(|l| l.starts_with("frpc:")).expect("frpc's own line");
    assert!(frpc_line.contains("would be refused"), "{frpc_line}");
    assert!(!frpc_line.contains("would be created"), "{frpc_line}");
    assert!(
        out.contains("drifting: not installed (would be created)"),
        "the drifting daemon must still be reported:\n{out}"
    );
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

// diff exit-code precedence ===========================================================================================
//
// `diff`'s exit code is not one severity ladder over a single number: `0`
// success, `1` error, `3` drift, `4` indeterminate, `5` conflict, combined
// by the `1 > 4 > 5 > 3 > 0` precedence rule (see `dispatch`'s doc comment
// and `cli::report::precedence`) — never `max()`. The tests below pin each
// class in isolation and every precedence-relevant pairing.

#[skuld::test]
fn diff_exits_three_when_a_daemon_would_be_created() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = absent_fixture(dir.path(), "frpc");

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 3, "{out}");
}

#[skuld::test]
fn diff_exits_three_when_a_daemon_would_be_updated() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = update_fixture(dir.path(), "frpc");

    let (code, out, _err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 3, "{out}");
}

/// Two up-to-date daemons in the same run: neither alone contributes a
/// code, so the combined result must still be 0 — not "0 unless the list
/// is empty" or some other accidental default.
#[skuld::test]
fn diff_exits_zero_only_when_everything_is_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [frpc]\n    restart: on-failure\n  websocat:\n    command: [websocat]\n    restart: on-failure\n",
    );
    let (specs, _warnings) = goetia::spec::load(&manifest).expect("load fixture manifest");
    let fake = Fake::new();
    for spec in &specs {
        fake.install(spec, false).expect("seed install");
    }

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    // `UpToDate` pushes nothing to the codes vec, so the assertion above
    // holds just as well when zero daemons were diffed at all. Pin that
    // both were actually reached.
    assert!(out.contains("frpc"), "frpc should appear in stdout:\n{out}");
    assert!(out.contains("websocat"), "websocat should appear in stdout:\n{out}");
}

/// An unreadable artifact alongside a hand-edited one returns 4: the
/// precedence rule's most surprising step, since `5` is the larger number.
/// A script branching on "conflict" (5) would otherwise run `--force` on an
/// incomplete picture, having missed that some other daemon in the same
/// run could not even be classified.
#[skuld::test]
fn diff_indeterminate_beats_conflict() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.install_then_hand_edit(&mk("conflicted"), "# hand-added directive\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  corrupt:\n    command: [daemon]\n  conflicted:\n    command: [daemon]\n",
    );

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    // Both outcomes must actually have been diffed — the point of this test
    // is the *mixing*, not just the arithmetic 4 beating 5 in the abstract.
    assert!(
        out.contains("corrupt: would be refused"),
        "unreadable outcome missing:\n{out}"
    );
    assert!(
        out.contains("conflicted: would conflict"),
        "conflict outcome missing:\n{out}"
    );
}

/// A `Create` plus a `Conflict` returns 5, not 3: `3` is not a "drift is
/// present" signal, since a conflicting daemon can ride along with it and
/// outrank it.
#[skuld::test]
fn diff_conflict_beats_drift() {
    let fake = Fake::new();
    fake.install_then_hand_edit(&mk("conflicted"), "# hand-added directive\n");

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  created:\n    command: [daemon]\n  conflicted:\n    command: [daemon]\n",
    );

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], &fake);

    assert_eq!(code, 5, "stdout:\n{out}\nstderr:\n{err}");
    // Both outcomes must actually have been diffed — the point of this test
    // is the *mixing*, not just the arithmetic 5 beating 3 in the abstract.
    assert!(
        out.contains("created: not installed (would be created)"),
        "drift outcome missing:\n{out}"
    );
    assert!(
        out.contains("conflicted: would conflict"),
        "conflict outcome missing:\n{out}"
    );
}

/// All three non-zero classes in one run — drift, conflict, and a hard
/// `Err(_)` from the manager — must return 1: an error always wins.
#[skuld::test]
fn diff_error_beats_conflict_beats_drift() {
    let inner = Fake::new();
    inner.install_then_hand_edit(&mk("conflicted"), "# hand-added directive\n");
    let mgr = FlakyManager {
        inner,
        fail_preview_for: Some("errored".to_string()),
        ..Default::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  created:\n    command: [daemon]\n  conflicted:\n    command: [daemon]\n  errored:\n    command: [daemon]\n",
    );

    let (code, out, err) = dispatch_with(
        &["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()],
        &mgr,
        &never_elevated,
    );

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    // All three outcomes must actually have been diffed, not just the
    // `errored` one that alone would already force 1.
    assert!(
        out.contains("created: not installed (would be created)"),
        "drift outcome missing:\n{out}"
    );
    assert!(
        out.contains("conflicted: would conflict"),
        "conflict outcome missing:\n{out}"
    );
    assert!(err.contains("errored"), "error outcome missing:\n{err}");
}

/// The only biconditional the contract makes: over one fixture per
/// `decide::Outcome` variant, `diff` exits 0 exactly when the daemon is
/// already up to date. The reverse direction of `3` is deliberately *not*
/// under test here — `diff_conflict_beats_drift` above is what pins that
/// `3` alone does not mean "drift is present".
#[skuld::test]
fn diff_exits_zero_iff_install_would_report_up_to_date() {
    // For each fixture: run `diff`, then ask `install` itself (through the
    // `ServiceManager` trait, directly against the same `Fake`) whether it
    // would report `UpToDate` too, and compare. `preview_install` never
    // mutates the store, so calling `install` afterwards on the same `Fake`
    // is safe — nothing before this point depended on it staying
    // unchanged. No hand-written `bool` per row: if `diff` and `install`
    // ever disagreed about one fixture, this observes the disagreement
    // instead of asserting past it.
    let check = |fake: &Fake, manifest: &Path, label: &str| -> i32 {
        let (code, out, err) =
            dispatch_read_only(&["goetia", "daemon", "diff", "-f", manifest.to_str().unwrap()], fake);
        let (specs, _warnings) = goetia::spec::load(manifest).expect("reload fixture manifest");
        let install_outcome = fake
            .install(&specs[0], false)
            .expect("install must not error for a diff fixture");
        let install_up_to_date = matches!(install_outcome, goetia::decide::Outcome::UpToDate);
        assert_eq!(
            code == 0,
            install_up_to_date,
            "{label}: diff exit {code}, install outcome {install_outcome:?}\nstdout:\n{out}\nstderr:\n{err}"
        );
        code
    };

    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = up_to_date_fixture(dir.path(), "frpc");
    check(&fake, &manifest, "UpToDate");

    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = absent_fixture(dir.path(), "frpc");
    check(&fake, &manifest, "Create");

    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = update_fixture(dir.path(), "frpc");
    check(&fake, &manifest, "Update");

    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new();
    fake.seed_stale(&mk("frpc"), "0.0.0-stale");
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let code = check(&fake, &manifest, "Stale");
    // Pinned exactly, not just `!= 0`: `Stale` is drift like `Create`/
    // `Update`, so it must be 3, not (say) a `Conflict`'s 5 — nothing else
    // in this file asserts the exact code for this outcome.
    assert_eq!(code, 3, "Stale must exit 3");

    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = hand_edited_fixture(dir.path(), "frpc");
    check(&fake, &manifest, "Conflict");

    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = foreign_fixture(dir.path(), "frpc");
    check(&fake, &manifest, "RefuseForeign");

    let dir = tempfile::tempdir().unwrap();
    let (fake, manifest) = unreadable_fixture(dir.path(), "frpc");
    check(&fake, &manifest, "RefuseUnreadable");
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

fn undetermined(doc: &serde_json::Value) -> &[serde_json::Value] {
    doc["undetermined"]
        .as_array()
        .unwrap_or_else(|| panic!("`undetermined` must always be present, as an array: {doc}"))
}

/// The `name` of each `undetermined` entry, `None` for an aggregate — the
/// one field of the third key whose *order* is contractual.
fn undetermined_names(doc: &serde_json::Value) -> Vec<Option<String>> {
    undetermined(doc)
        .iter()
        .map(|entry| match &entry["name"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(name) => Some(name.clone()),
            other => panic!("`name` must be a string or null: {other}"),
        })
        .collect()
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
    fake.start(&Id::try_from("frpc").unwrap(), Budget::DEFAULT).unwrap();
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
    fake.start(&spec.id, Budget::DEFAULT).unwrap();
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
/// has to know which verb produced the document it is reading. The fixture
/// populates all three keys, so the equality covers the third rather than
/// holding only where it is empty.
#[skuld::test]
fn status_json_with_no_ids_equals_list_json() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.start(&Id::try_from("frpc").unwrap(), Budget::DEFAULT).unwrap();
    fake.install(&mk("websocat"), false).unwrap();
    fake.seed_unreadable("corrupt");
    fake.seed_opaque("opaque");

    let (list_code, list_out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);
    let (status_code, status_out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "status"], &fake);

    assert_eq!(list_code, status_code);
    assert_eq!(parse_json(&list_out), parse_json(&status_out));
    assert_eq!(
        undetermined_names(&parse_json(&list_out)),
        [Some("opaque".to_string())],
        "the third key is populated, not vacuously equal"
    );
}

/// Three states with opposite remedies, which text mode renders as one
/// undifferentiated `error:` line each. The `unreadable` here comes from a
/// blob that fails to decode at all (`Error::Blob`); the
/// `Error::Invalid`-producing case is
/// `status_json_reports_invalid_blob_content_as_unreadable_not_invalid_id`.
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

/// The rule the whole wire format rests on: a `kind` comes from **which
/// operation failed**, never from the `Error` variant. `Error::Invalid` is
/// produced by `support::parse_id` *and* by blob-content validation, so a
/// classifier keyed on the variant would report an id goetia owns but cannot
/// decode as `invalid-id` with exit 1 — telling the user to fix a command
/// line that is perfectly correct — instead of `unreadable` with exit 4.
#[skuld::test]
fn status_json_reports_invalid_blob_content_as_unreadable_not_invalid_id() {
    let fake = Fake::new();
    fake.seed_invalid_content("frpc");

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "frpc"], &fake);

    let doc = parse_json(&out);
    assert_eq!(
        error_kinds(&doc),
        ["unreadable"],
        "the argument was a valid id; the *artifact* is what goetia cannot read: {doc}"
    );
    assert_eq!(errors(&doc)[0]["id"], "frpc");
    assert_eq!(code, 4, "{out}");
}

/// `list` must classify the same artifact the same way — it reaches
/// `Installed::OursUnreadable` rather than an `Error` at all.
#[skuld::test]
fn list_json_reports_invalid_blob_content_as_unreadable() {
    let fake = Fake::new();
    fake.seed_invalid_content("frpc");

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    assert_eq!(error_kinds(&parse_json(&out)), ["unreadable"], "{out}");
    assert_eq!(code, 4, "{out}");
}

/// Ids are reported in argument order, and a repeated argument yields a
/// repeated entry — `status` never silently deduplicates what was asked for.
#[skuld::test]
fn status_json_repeats_an_id_given_twice() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.install(&mk("websocat"), false).unwrap();

    let (code, out, _err) = dispatch_read_only(
        &["goetia", "--json", "daemon", "status", "websocat", "frpc", "websocat"],
        &fake,
    );

    assert_eq!(code, 0, "{out}");
    assert_eq!(daemon_ids(&parse_json(&out)), ["websocat", "frpc", "websocat"]);
}

/// The channel rule holds for `status` too, on a fixture whose text mode
/// writes both a `warning:` (the unreadable entry) and an `error:` line.
#[skuld::test]
fn status_json_is_the_only_thing_on_stdout_and_stderr_stays_empty() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_unreadable("corrupt");

    for args in [
        ["goetia", "--json", "daemon", "status"].as_slice(),
        [
            "goetia",
            "--json",
            "daemon",
            "status",
            "frpc",
            "corrupt",
            "not/a/valid/id",
        ]
        .as_slice(),
    ] {
        let (code, out, err) = dispatch_read_only(args, &fake);

        parse_json(&out);
        assert!(err.is_empty(), "{args:?} wrote to stderr:\n{err}");
        assert_ne!(code, 0, "{args:?} must still report the failures it found");
    }
}

/// `Error::Invalid`'s own `Display` prefixes ``daemon `X`: ``, and the text
/// renderer prefixes the id as well, so carrying `Display` into `message`
/// would name the same id three times on one line.
#[skuld::test]
fn status_names_a_rejected_id_once_per_channel() {
    let fake = Fake::new();

    let (_, out, _) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "not/a/valid/id"], &fake);
    let (_, _, text_err) = dispatch_read_only(&["goetia", "daemon", "status", "not/a/valid/id"], &fake);

    let message = errors(&parse_json(&out))[0]["message"].as_str().unwrap().to_string();
    assert!(
        !message.contains("not/a/valid/id:"),
        "`id` is the attribution; `message` must not repeat it as a prefix: {message}"
    );
    assert_eq!(
        text_err.matches("not/a/valid/id").count(),
        2,
        "once as the line's prefix and once inside the pattern message: {text_err}"
    );
}

// Residual artifacts ==================================================================================================

/// The one error `uninstall` renders as success is `NotInstalled`, so a manager must reserve it for
/// an id that is genuinely empty. `Fake::seed_residual_artifact` models the state that broke this
/// on systemd: `/etc/systemd/system/<id>.service` gone, but a `<id>.service.d/*.conf` drop-in or a
/// `multi-user.target.wants/<id>.service` link still there and still being applied. Reachable from
/// `uninstall`'s own partial-failure path — the retry it tells you to run must not then print
/// "nothing to do".
#[skuld::test]
fn uninstall_does_not_report_success_over_a_residual_artifact() {
    let fake = Fake::new();
    fake.seed_residual_artifact("leftover");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "leftover"], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, "", "`uninstall x && echo \"confirmed gone\"` must not print");
    assert!(err.contains("leftover"), "{err}");
}

/// `install` refuses the id as foreign; `uninstall` must not call the same id empty. Two verbs
/// disagreeing about one state is the defect, not either answer on its own.
#[skuld::test]
fn install_and_uninstall_agree_about_an_id_with_only_a_residual_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  leftover:\n    command: [leftover]\n");
    let fake = Fake::new();
    fake.seed_residual_artifact("leftover");

    let (install_code, _, install_err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );
    let (uninstall_code, _, _) = dispatch_elevated(&["goetia", "daemon", "uninstall", "leftover"], &fake);

    assert_ne!(install_code, 0, "install refuses it: {install_err}");
    assert!(
        install_err.contains("not a goetia-managed service"),
        "refused as foreign, not for some unrelated reason: {install_err}"
    );
    assert_ne!(uninstall_code, 0, "so uninstall cannot report it gone");
}

// Ids goetia could not classify =======================================================================================
//
// `list`, `status` with no ids, and `show` with no ids all answer from `list()`, which reports an id
// it could not classify at all as one `undetermined` entry — naming that id, or, for a read that
// denied many at once, naming nobody. It is not an `errors[]` entry on that path: the command
// succeeded and reported everything it could see, and the entry is a stated limit on the answer's
// completeness. `status <id>` reports the same fact about one named id as `errors[].kind:
// "undetermined"` instead; both are exit `4`.

/// An id `list()` cannot enumerate is reported rather than dropped — the silence the third key
/// exists to end. Keeps its fixture's asymmetry: a direct `status(&id)` still finds the daemon the
/// listing could not describe.
#[skuld::test]
fn list_reports_a_daemon_it_cannot_enumerate_as_undetermined() {
    let inner = Fake::new();
    inner.install(&mk("ghost"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        hidden_from_list: Some("ghost".to_string()),
        undetermined_in_list: Some("ghost".to_string()),
        ..Default::default()
    };
    assert!(
        mgr.status(&Id::try_from("ghost").unwrap()).is_ok(),
        "a direct query finds it"
    );

    let (code, out, err) = dispatch_with(&["goetia", "--json", "daemon", "list"], &mgr, &never_elevated);

    let doc = parse_json(&out);
    assert!(daemons(&doc).is_empty(), "the listing still cannot describe it: {doc}");
    assert!(errors(&doc).is_empty(), "and it is not a failure of the command: {doc}");
    assert_eq!(undetermined_names(&doc), [Some("ghost".to_string())], "{doc}");
    assert_eq!(code, 4, "no longer indistinguishable from a host with no daemons");
    assert_eq!(err, "", "--json puts everything in the document");

    let (text_code, _, text_err) = dispatch_with(&["goetia", "daemon", "list"], &mgr, &never_elevated);

    assert_eq!(text_code, 4, "{text_err}");
    assert!(
        text_err.contains("ghost") && text_err.contains("could not be determined"),
        "text mode warns about it too: {text_err}"
    );
}

#[skuld::test]
fn status_with_no_ids_reports_a_daemon_it_cannot_enumerate() {
    let inner = Fake::new();
    inner.install(&mk("ghost"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        hidden_from_list: Some("ghost".to_string()),
        undetermined_in_list: Some("ghost".to_string()),
        ..Default::default()
    };

    let (all_code, all_out, _) = dispatch_with(&["goetia", "--json", "daemon", "status"], &mgr, &never_elevated);
    let (one_code, one_out, _) = dispatch_with(
        &["goetia", "--json", "daemon", "status", "ghost"],
        &mgr,
        &never_elevated,
    );

    let all = parse_json(&all_out);
    assert!(daemons(&all).is_empty(), "{all}");
    assert!(errors(&all).is_empty(), "{all}");
    assert_eq!(undetermined_names(&all), [Some("ghost".to_string())], "{all}");
    assert_eq!(all_code, 4);

    // The asymmetry, unchanged: naming the id finds exactly the daemon the no-ids form could not
    // describe.
    assert_eq!(daemon_ids(&parse_json(&one_out)), ["ghost"], "{one_out}");
    assert_eq!(one_code, 0);
}

/// The key belongs to the envelope, not to the cases that populate it: a consumer reading
/// `doc["undetermined"]` must never meet a missing key, which is what an accidental
/// `skip_serializing_if` would produce.
#[skuld::test]
fn list_json_always_has_an_undetermined_key() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    let doc = parse_json(&out);
    assert!(undetermined(&doc).is_empty(), "{doc}");
    assert_eq!(code, 0, "{out}");
}

#[skuld::test]
fn list_json_reports_an_opaque_daemon_as_undetermined() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    let doc = parse_json(&out);
    assert_eq!(undetermined_names(&doc), [Some("opaque".to_string())], "{doc}");
    assert!(
        !undetermined(&doc)[0]["reason"]
            .as_str()
            .expect("`reason` must be a string")
            .is_empty(),
        "an undetermined entry must say why: {doc}"
    );
    assert!(daemons(&doc).is_empty(), "no ownership was established: {doc}");
    assert!(errors(&doc).is_empty(), "{doc}");
    assert_eq!(code, 4, "{out}");
}

/// The decided split, pinned so a later change cannot quietly move it: on the `list()` path
/// indeterminacy is the third key and never an `errors[]` entry, so a consumer can read it without
/// parsing `kind` strings. `status <id>` is the other half
/// (`status_json_for_a_named_undetermined_id_is_an_errors_entry`).
#[skuld::test]
fn list_json_undetermined_is_not_an_errors_entry() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_opaque("opaque");

    let (_code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    let doc = parse_json(&out);
    assert!(errors(&doc).is_empty(), "{doc}");
    assert_eq!(undetermined_names(&doc), [Some("opaque".to_string())], "{doc}");
    assert_eq!(
        daemon_ids(&doc),
        ["frpc"],
        "what it could read is still reported: {doc}"
    );
}

/// The exit code folds over both keys at once. Two partial answers of different shapes — an id
/// goetia owns and cannot report on, and an id it could not classify at all — still combine to the
/// one partial-answer code.
#[skuld::test]
fn list_exits_four_when_an_unreadable_entry_accompanies_an_undetermined_one() {
    let fake = Fake::new();
    fake.seed_unreadable("corrupt");
    fake.seed_opaque("opaque");

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    let doc = parse_json(&out);
    assert_eq!(error_kinds(&doc), ["unreadable"], "{doc}");
    assert_eq!(undetermined_names(&doc), [Some("opaque".to_string())], "{doc}");
    assert_eq!(code, 4, "{out}");
}

/// The sibling assertion for `list_exit_code_is_the_same_with_and_without_json`, extended to the
/// third key: a verb whose exit code depends on its output format is the split the envelope exists
/// to remove.
#[skuld::test]
fn list_exit_code_is_the_same_with_and_without_json_for_an_undetermined_entry() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (plain, _, plain_err) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);
    let (json, _, _) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    assert_eq!(plain, json, "the exit code must not depend on the output format");
    assert_eq!(plain, 4);
    assert!(
        plain_err.contains("opaque"),
        "text mode names what it could not determine: {plain_err}"
    );
}

/// The aggregate's *text* rendering. An entry with no name has no name to prefix it with, so its
/// reason — a complete sentence, unlike a named entry's fragment — stands alone after `warning: `.
/// The two existing aggregate tests read `--json` or discard stderr, which left this arm of
/// `cli::support::print_undetermined_warnings` wired into `list` and never once rendered.
#[skuld::test]
fn list_text_renders_an_aggregate_entry_with_no_name_to_prefix() {
    let fake = Fake::new();
    let reason = "2 services could not be enumerated at this privilege level";
    fake.seed_aggregate_undetermined(reason);

    let (code, _out, err) = dispatch_read_only(&["goetia", "daemon", "list"], &fake);

    assert_eq!(code, 4, "{err}");
    assert!(
        err.contains(&format!("warning: {reason}\n")),
        "the reason stands alone, with nothing between it and `warning: `: {err}"
    );
    assert!(
        !err.contains("installation state could not be determined"),
        "that phrasing is the *named* arm's, and prefixing a name that does not exist is what \
         this arm exists to avoid: {err}"
    );
}

/// Named entries by name, then the aggregates that name nobody — one ordering, applied where the
/// index is built so the text renderer (which reads the index, not the `Report`) cannot drift from
/// the document. The manager deliberately emits them in the opposite order.
#[skuld::test]
fn list_json_orders_named_entries_before_aggregates() {
    let fake = Fake::new();
    fake.seed_opaque("zulu");
    fake.seed_opaque("alpha");
    fake.seed_aggregate_undetermined("3 services could not be enumerated at this privilege level");

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "list"], &fake);

    let doc = parse_json(&out);
    assert_eq!(
        undetermined_names(&doc),
        [Some("alpha".to_string()), Some("zulu".to_string()), None],
        "{doc}"
    );
    assert_eq!(code, 4, "{out}");
}

/// The other half of the split: asked about one named id, `status` answers with that id's own
/// failure, in `errors`, where every other per-id answer of its lives.
#[skuld::test]
fn status_json_for_a_named_undetermined_id_is_an_errors_entry() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, out, _err) = dispatch_read_only(&["goetia", "--json", "daemon", "status", "opaque"], &fake);

    let doc = parse_json(&out);
    assert_eq!(error_kinds(&doc), ["undetermined"], "{doc}");
    assert_eq!(errors(&doc)[0]["id"], "opaque");
    assert!(
        undetermined(&doc).is_empty(),
        "the third key is the `list()` path's: {doc}"
    );
    assert_eq!(code, 4, "{out}");
}

#[skuld::test]
fn status_text_for_an_undetermined_id_does_not_say_not_installed() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, _out, err) = dispatch_read_only(&["goetia", "daemon", "status", "opaque"], &fake);

    assert_eq!(code, 4, "{err}");
    assert!(!err.contains("not installed"), "no absence was established: {err}");
    assert!(
        !err.contains("not managed by goetia"),
        "nor anyone else's ownership: {err}"
    );
}

#[skuld::test]
fn show_reports_indeterminacy_rather_than_absence_for_an_opaque_id() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "show", "opaque"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, "", "there is no spec to render");
    assert!(!err.contains("not installed"), "{err}");
    assert!(err.contains("opaque"), "{err}");
}

/// With no ids, an undetermined entry never enters the loop — it has no spec to show — so only the
/// pre-loop escalation can flag it. Without that, `show` prints a complete-looking dump and exits
/// `0` on a host where it could not read half the daemons.
#[skuld::test]
fn show_with_no_ids_exits_four_when_something_could_not_be_read() {
    let fake = Fake::new();
    fake.install(&mk("readable"), false).unwrap();
    fake.seed_opaque("opaque");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "show"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("# readable"), "what it could read is still shown: {out}");
    assert!(err.contains("opaque"), "{err}");
}

/// The null-name rule at its one call site: an aggregate entry may stand for the very id being
/// asked about, so "not installed" does not follow from that id going unnamed — not for the
/// message, and not for the exit code.
#[skuld::test]
fn show_refuses_to_call_an_id_absent_while_an_aggregate_is_present() {
    let fake = Fake::new();
    fake.seed_aggregate_undetermined("2 services could not be enumerated at this privilege level");

    let (code, out, err) = dispatch_read_only(&["goetia", "daemon", "show", "nowhere"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        !err.contains("not installed"),
        "the listing establishes no absence: {err}"
    );
}

/// `uninstall` is the one verb an absent artifact satisfies — and an undetermined id is not absent,
/// so `absent_is_success` must not swallow it. "Confirmed gone" for an id nobody could read is the
/// same false certificate the flag's own doc comment forbids.
#[skuld::test]
fn uninstall_exits_four_for_an_undetermined_id() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "uninstall", "opaque"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("nothing to do"), "{out}");
    assert!(err.contains("opaque"), "{err}");
}

#[skuld::test]
fn start_exits_four_for_an_undetermined_id() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "start", "opaque"], &fake);

    assert_eq!(code, 4, "{err}");
    assert!(err.contains("opaque"), "{err}");
}

/// `restart` re-wraps the start leg's failure to say the daemon is now down. The wrap must preserve
/// the variant, or the state where the distinction matters most — stopped, and not back up — is the
/// one that reports a plain failure. `--no-timeout`, as in `restart_reaches_the_manager`, so the
/// start leg is reached however long the stop took.
#[skuld::test]
fn restart_exits_four_for_an_undetermined_id() {
    let inner = Fake::new();
    inner.install(&mk("frpc"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        undetermined_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, _out, err) = dispatch_with(&["goetia", "daemon", "restart", "frpc", "--no-timeout"], &mgr, &|| true);

    assert_eq!(code, 4, "{err}");
    assert!(
        err.contains("stopped but failed to restart"),
        "the context the wrap exists for survives it: {err}"
    );
}

/// A wait that ran out is the same *class* as an unanswered question — the code is about whether
/// the question was answered — but a different condition, so `run_id_verb` needs its own arm.
/// Without one it falls to the catch-all and reports `1`, which says the start determinately
/// failed: the one thing an expiry did not establish, since the request was never cancelled.
#[skuld::test]
fn start_exits_four_when_the_wait_timed_out() {
    let inner = Fake::new();
    inner.install(&mk("frpc"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        wait_timeout_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, _out, err) = dispatch_with(&["goetia", "daemon", "start", "frpc"], &mgr, &|| true);

    assert_eq!(code, 4, "{err}");
    assert!(err.contains("frpc"), "{err}");
    assert!(
        err.contains("did not report running"),
        "the expiry must reach the user as one: {err}"
    );
}

/// The combination a scalar exit flag could not express: one id that failed determinately and one
/// goetia could not answer for at all.
#[skuld::test]
fn an_id_verb_reports_error_over_indeterminate() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, _out, err) = dispatch_elevated(&["goetia", "daemon", "start", "opaque", "missing"], &fake);

    assert_eq!(code, 1, "a determinate failure outranks an unanswered question: {err}");
}

#[skuld::test]
fn install_exits_four_for_an_undetermined_id() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  opaque:\n    command: [daemon]\n");
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let (code, _out, err) = dispatch_elevated(
        &["goetia", "daemon", "install", "-f", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 4, "{err}");
    assert!(err.contains("opaque"), "{err}");
}

/// The artifact was written and the id is goetia's beyond doubt, and then the *enable* could not be
/// answered — a distinct call site from the `mgr.install` failure above, classifying its own step.
/// Reporting `1` here would say the enable determinately failed, which is what nobody established.
#[skuld::test]
fn install_exits_four_when_the_enable_leg_cannot_be_determined() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let mgr = FlakyManager {
        undetermined_enable_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, out, err) = dispatch_with(
        &[
            "goetia",
            "daemon",
            "install",
            "--enable",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("enable"), "the failing step is named: {err}");
}

/// The third call site, and the same rule: `install --start` classifies its own step. Every other
/// test that reaches an undetermined `start` goes through `restart`, which is a different path
/// entirely.
#[skuld::test]
fn install_exits_four_when_the_start_leg_cannot_be_determined() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let mgr = FlakyManager {
        undetermined_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, out, err) = dispatch_with(
        &[
            "goetia",
            "daemon",
            "install",
            "--start",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("start"), "the failing step is named: {err}");
}

/// `install --start`'s own classifier, and the same rule as the undetermined case above: it
/// classifies its own step. `failure_code`'s catch-all would report `1` — "the start determinately
/// failed" — for a request that was issued, accepted and simply not waited out.
#[skuld::test]
fn install_exits_four_when_the_start_leg_timed_out() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let mgr = FlakyManager {
        wait_timeout_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, out, err) = dispatch_with(
        &[
            "goetia",
            "daemon",
            "install",
            "--start",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("start"), "the failing step is named: {err}");
}

/// `install`'s three classes through the one precedence rule: a refusal outranks an unanswered
/// question, which outranks a conflict.
#[skuld::test]
fn install_error_beats_indeterminate_beats_conflict() {
    let two_dir = tempfile::tempdir().unwrap();
    let three_dir = tempfile::tempdir().unwrap();
    let two = write_manifest(
        two_dir.path(),
        "daemons:\n  opaque:\n    command: [daemon]\n  edited:\n    command: [daemon]\n",
    );
    let three = write_manifest(
        three_dir.path(),
        "daemons:\n  opaque:\n    command: [daemon]\n  edited:\n    command: [daemon]\n  stranger:\n    command: [daemon]\n",
    );
    let fake = Fake::new();
    fake.seed_opaque("opaque");
    fake.install_then_hand_edit(&mk("edited"), "# hand-added directive\n");
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let (two_code, _, two_err) =
        dispatch_elevated(&["goetia", "daemon", "install", "-f", two.to_str().unwrap()], &fake);
    let (three_code, _, three_err) =
        dispatch_elevated(&["goetia", "daemon", "install", "-f", three.to_str().unwrap()], &fake);

    assert_eq!(two_code, 4, "indeterminate outranks conflict: {two_err}");
    assert_eq!(three_code, 1, "a refusal outranks both: {three_err}");
}

// --timeout / --no-timeout ============================================================================================

/// The `Fake` with `status` made unreachable. `restart --timeout 0` must issue its two steps with
/// no state read between them, and a read is invisible to `Fake::calls`, which records only
/// `start`/`stop`/`restart`.
#[derive(Clone, Default)]
struct PanicsOnStatus(Fake);

impl ServiceManager for PanicsOnStatus {
    fn install(&self, spec: &DaemonSpec, force: bool) -> goetia::Result<goetia::decide::Outcome> {
        self.0.install(spec, force)
    }
    fn preview_install(&self, spec: &DaemonSpec) -> goetia::Result<goetia::decide::Outcome> {
        self.0.preview_install(spec)
    }
    fn uninstall(&self, id: &Id) -> goetia::Result<()> {
        self.0.uninstall(id)
    }
    fn enable(&self, id: &Id) -> goetia::Result<()> {
        self.0.enable(id)
    }
    fn disable(&self, id: &Id) -> goetia::Result<()> {
        self.0.disable(id)
    }
    fn start(&self, id: &Id, budget: Budget) -> goetia::Result<()> {
        self.0.start(id, budget)
    }
    fn request_start_after_stop(&self, id: &Id) -> goetia::Result<()> {
        self.0.request_start_after_stop(id)
    }
    fn stop(&self, id: &Id, budget: Budget) -> goetia::Result<()> {
        self.0.stop(id, budget)
    }
    fn status(&self, id: &Id) -> goetia::Result<Status> {
        panic!("`restart` with no budget must not read state: status({id})")
    }
    fn list(&self) -> goetia::Result<Vec<Installed>> {
        self.0.list()
    }
}

#[skuld::test]
fn start_that_times_out_exits_4_and_says_what_was_established() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_start_stalls("frpc");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "start", "frpc", "--timeout", "5s"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("did not report running"), "{err}");
    assert!(
        err.contains("stopped waiting"),
        "goetia stopped waiting, it did not cancel: {err}"
    );
    assert!(err.contains("not cancelled"), "{err}");
    assert!(
        err.contains("goetia daemon status"),
        "the way to find out is named: {err}"
    );
}

#[skuld::test]
fn start_with_timeout_zero_reports_requested_not_started() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "start", "frpc", "--timeout", "0"], &fake);

    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out, "frpc: start requested\n",
        "nothing was confirmed, so nothing is claimed"
    );
}

#[skuld::test]
fn start_with_no_flag_waits_and_reports_started() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "start", "frpc"], &fake);

    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "frpc: started\n");
}

#[skuld::test]
fn no_timeout_is_accepted() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "start", "frpc", "--no-timeout"], &fake);

    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "frpc: started\n");
}

#[skuld::test]
fn stop_that_times_out_exits_4() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_stop_stalls("frpc");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "stop", "frpc", "--timeout", "5s"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("did not report stopped"), "{err}");
}

/// `cli::restart` re-wraps its start leg's error variant by variant. A flattened `WaitTimeout`
/// becomes `Error::Other` and silently exits `1` — "the start determinately failed", which is the
/// one thing an expiry did not establish.
#[skuld::test]
fn restart_whose_start_leg_times_out_exits_4_not_1() {
    let inner = Fake::new();
    inner.install(&mk("frpc"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        wait_timeout_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, out, err) = dispatch_with(
        &["goetia", "daemon", "restart", "frpc", "--timeout", "5s"],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.contains("was stopped"),
        "the context the wrap exists for survives it: {err}"
    );
    assert!(
        err.contains("did not report running within 5s:"),
        "the budget reported is the `--timeout` given, not what the leg had left: {err}"
    );
    assert!(!out.contains("restarted"), "{out}");
}

/// The neighbouring behaviour, unchanged: a stop leg that determinately failed is still `1`, and
/// naming a budget does not soften it.
#[skuld::test]
fn restart_that_never_stopped_still_exits_1() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "stranger", "--timeout", "5s"], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("restart"), "{out}");
}

/// goetia does not start into an unconfirmed stop. Asserted on the **absence** of the start call:
/// an implementation that issues it and then reports `4` passes an exit-code assertion alone.
///
/// No bet on time: whether the stop leg gets what is left of the `1s` or a budget already spent,
/// every assertion below holds — see `restart_does_not_start_after_a_stop_whose_budget_was_spent`.
#[skuld::test]
fn restart_does_not_start_after_a_stop_that_timed_out() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_stop_stalls("frpc");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc", "--timeout", "1s"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        fake.calls(),
        vec![("stop", "frpc".to_string())],
        "the start leg must never be issued"
    );
    assert!(err.contains("restart was abandoned"), "{err}");
    assert!(err.contains("no start was issued"), "{err}");
    assert!(err.contains("may be left stopped"), "{err}");
    assert!(
        err.contains("did not report stopped within 1s:"),
        "the budget reported is the `--timeout` given, as plain `stop` reports it, not what the \
         leg had left: {err}"
    );
}

/// A bounded budget spent before the stop leg is issued is still a timeout, never `--timeout 0`'s
/// "requested": the stop goes out and confirms nothing, so the restart neither reports it stopped nor
/// starts into it. `1ns` is spent before the stop leg on any machine, and a stop leg that did get
/// what was left times out against the stall just the same — so the outcome is one, not a race.
#[skuld::test]
fn restart_does_not_start_after_a_stop_whose_budget_was_spent() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_stop_stalls("frpc");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc", "--timeout", "1ns"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        fake.calls(),
        vec![("stop", "frpc".to_string())],
        "the start leg must never be issued"
    );
    assert!(err.contains("no start was issued"), "{err}");
    assert!(err.contains("may be left stopped"), "{err}");
    assert!(!err.contains("was stopped"), "nothing confirmed the stop: {err}");
}

/// "Don't wait, just do the steps": on a manager with no request-only restart, both steps issued, in
/// order, with no confirmation and no state read between them.
#[skuld::test]
fn restart_with_no_budget_issues_both_steps_without_confirming() {
    let mgr = PanicsOnStatus::default();
    mgr.0.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_with(
        &["goetia", "daemon", "restart", "frpc", "--timeout", "0"],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        mgr.0.calls(),
        vec![("stop", "frpc".to_string()), ("start", "frpc".to_string())]
    );
    assert_eq!(out, "frpc: restart requested\n");
}

/// `restart --timeout 0` over a daemon that is up, in the shape the Windows backend really meets:
/// the stop request is accepted, and the service goes on reading `RUNNING` for its whole teardown —
/// `goetia-shim` never reports `STOP_PENDING` — so the start that follows is answered "already
/// running". The `Fake` models exactly that: its request-only `stop` settles nothing, so the entry
/// is still `Running` when the start arrives, and its plain `start` is idempotent over it, as SCM's
/// is over a queried `RUNNING`.
///
/// That answer is a refused start, not a restart: `4`, never `0`. `cli::restart` must not read
/// the trait's idempotent `start` `Ok` as proof the service cycled, and must not read a refusal as
/// proof that it did not.
#[skuld::test]
fn restart_with_no_budget_reports_a_start_refused_over_a_service_still_up() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_state("frpc", State::Running);

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc", "--timeout", "0"], &fake);

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, "", "nothing may claim the daemon was restarted: {out}");
    assert!(
        err.contains("was not restarted, and where it ended up is not established"),
        "{err}"
    );
    assert_eq!(
        fake.calls(),
        vec![("stop", "frpc".to_string()), ("start", "frpc".to_string())],
        "both steps are still issued"
    );
}

/// The non-waiting path must not absorb an `Undetermined` start leg into `Error::Unestablished`,
/// which says in its own doc comment that what is installed at the id was never in doubt. Here it
/// is in doubt, and the waiting path already preserves the variant — so both paths must.
#[skuld::test]
fn restart_with_no_budget_keeps_an_undetermined_start_undetermined() {
    let inner = Fake::new();
    inner.install(&mk("frpc"), false).unwrap();
    let mgr = FlakyManager {
        inner,
        undetermined_start_for: Some("frpc".to_string()),
        ..Default::default()
    };

    let (code, out, err) = dispatch_with(
        &["goetia", "daemon", "restart", "frpc", "--timeout", "0"],
        &mgr,
        &|| true,
    );

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.starts_with("error: frpc: cannot determine whether daemon `frpc` is installed"),
        "the start leg's variant must survive the non-waiting path, not be nested inside another \
         error that denies the doubt: {err}"
    );
    assert!(
        err.contains("without waiting for it"),
        "what the stop leg did and did not establish is still disclosed: {err}"
    );
    assert_eq!(out, "", "nothing may claim the daemon was restarted: {out}");
}

/// Where the manager has a request-only restart, `restart --timeout 0` is that one request and
/// nothing else: a stop and a separate start leave the window systemd closes by replacing the stop.
/// Over a daemon still up, where the two-step path reports a refused start, the one request is
/// simply accepted.
#[skuld::test]
fn restart_with_no_budget_is_one_request_where_the_manager_has_one() {
    let fake = Fake::new();
    fake.seed_native_restart();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_state("frpc", State::Running);

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc", "--timeout", "0"], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(fake.calls(), vec![("restart", "frpc".to_string())]);
    assert_eq!(out, "frpc: restart requested\n");
}

/// A restart the manager refused is one it never started: nothing was stopped, so the refusal is
/// the whole answer — exit `1`, not the two-step path's "where it ended up is not established".
#[skuld::test]
fn a_refused_request_only_restart_is_a_failure_that_changed_nothing() {
    let fake = Fake::new();
    fake.seed_native_restart();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "stranger", "--timeout", "0"], &fake);

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!err.contains("not established"), "{err}");
    assert_eq!(fake.calls(), vec![("restart", "stranger".to_string())]);
}

/// A budget that waits confirms each leg, which the request-only restart cannot: it is never used.
#[skuld::test]
fn a_restart_that_waits_never_takes_the_request_only_restart() {
    let fake = Fake::new();
    fake.seed_native_restart();
    fake.install(&mk("frpc"), false).unwrap();

    let (code, out, err) = dispatch_elevated(&["goetia", "daemon", "restart", "frpc"], &fake);

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        fake.calls(),
        vec![("stop", "frpc".to_string()), ("start", "frpc".to_string())]
    );
}

/// A start that may or may not have reached the manager is indeterminate — exit `4`, never the `1`
/// that says it failed — through every verb that starts: `start`, `install --start`, and
/// `restart`'s start leg under a budget that waits and one that does not, where the stop before it
/// is disclosed as the leg left it. Never as a failure to restart, which is not what was
/// established, and never, under the budget that does not wait, as the refused start after an
/// unconfirmed stop it is not.
#[skuld::test]
fn a_start_in_doubt_exits_4_through_every_verb_that_starts() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let manifest = manifest.to_str().unwrap();
    for (args, disclosed) in [
        (&["goetia", "daemon", "start", "frpc"][..], ""),
        (&["goetia", "daemon", "install", "--file", manifest, "--start"], ""),
        (
            &["goetia", "daemon", "restart", "frpc"],
            "`frpc` was stopped, and this start was attempted after it",
        ),
        (
            &["goetia", "daemon", "restart", "frpc", "--timeout", "0"],
            "the stop was issued without waiting for it, and this start was attempted after it",
        ),
    ] {
        let fake = Fake::new();
        fake.install(&mk("frpc"), false).unwrap();
        fake.seed_start_in_doubt("frpc");

        let (code, out, err) = dispatch_elevated(args, &fake);

        assert_eq!(code, 4, "{args:?}\nstdout:\n{out}\nstderr:\n{err}");
        assert!(
            err.contains("may or may not have reached the service manager"),
            "{args:?}: {err}"
        );
        assert!(err.contains(disclosed), "{args:?}: {err}");
        for claim in ["failed to restart", "refused", "not established"] {
            assert!(!err.contains(claim), "{args:?}: {err}");
        }
    }
}

/// A stop in doubt may have stopped the daemon, and `restart` goes no further: it says so — no
/// start was issued, and the daemon may be left stopped — as it does for a stop that timed out.
/// Exit `4`, under every budget.
#[skuld::test]
fn a_stop_in_doubt_abandons_the_restart_and_says_so() {
    for budget in [&[][..], &["--no-timeout"], &["--timeout", "0"]] {
        let fake = Fake::new();
        fake.install(&mk("frpc"), false).unwrap();
        fake.seed_stop_in_doubt("frpc");
        let mut args = vec!["goetia", "daemon", "restart", "frpc"];
        args.extend_from_slice(budget);

        let (code, out, err) = dispatch_elevated(&args, &fake);

        assert_eq!(code, 4, "{budget:?}\nstdout:\n{out}\nstderr:\n{err}");
        for said in [
            "may or may not have reached the service manager",
            "no start was issued, so `frpc` may be left stopped",
            "goetia daemon start frpc",
        ] {
            assert!(err.contains(said), "{budget:?}: {err}");
        }
        assert_eq!(fake.calls(), vec![("stop", "frpc".to_string())], "{budget:?}");
    }
}

/// `restart` makes ready what both legs need before the stop is sent: when that fails, nothing was
/// sent — exit `1`, with neither leg issued and the daemon untouched.
#[skuld::test]
fn restart_that_cannot_prepare_sends_nothing() {
    for budget in [&[][..], &["--timeout", "0"], &["--no-timeout"]] {
        let fake = Fake::new();
        fake.install(&mk("frpc"), false).unwrap();
        fake.seed_state("frpc", State::Running);
        fake.seed_prepare_fails();
        let mut args = vec!["goetia", "daemon", "restart", "frpc"];
        args.extend_from_slice(budget);

        let (code, out, err) = dispatch_elevated(&args, &fake);

        assert_eq!(code, 1, "{budget:?}\nstdout:\n{out}\nstderr:\n{err}");
        assert!(err.contains("nothing could be prepared"), "{err}");
        assert_eq!(fake.calls(), vec![], "{budget:?}: nothing may be sent");
        assert_eq!(
            fake.status(&Id::try_from("frpc").unwrap()).unwrap().state,
            State::Running
        );
    }
}

/// `install --start` makes ready what the start needs before the install sends anything: when that
/// fails, nothing is installed and nothing started.
#[skuld::test]
fn install_start_that_cannot_prepare_installs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let fake = Fake::new();
    fake.seed_prepare_fails();

    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--file",
            manifest.to_str().unwrap(),
            "--start",
        ],
        &fake,
    );

    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("nothing could be prepared"), "{err}");
    assert_eq!(fake.calls(), vec![]);
    assert!(
        fake.status(&Id::try_from("frpc").unwrap()).is_err(),
        "nothing may be installed"
    );
}

/// Without `--start` there is no sequence, and nothing is prepared.
#[skuld::test]
fn install_without_start_prepares_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let fake = Fake::new();
    fake.seed_prepare_fails();

    let (code, out, err) = dispatch_elevated(
        &["goetia", "daemon", "install", "--file", manifest.to_str().unwrap()],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(fake.prepared(), vec![]);
    assert_eq!(fake.guarded(), vec![("install", "frpc".to_string(), vec![])]);
}

/// `(verb, id, steps)` for each of `sent`, as [`Fake::guarded`] records a request asked while a
/// preparation of `steps` was held.
fn under(steps: &[Step], sent: &[(&'static str, &str)]) -> Vec<(&'static str, String, Vec<Step>)> {
    sent.iter()
        .map(|(verb, id)| (*verb, id.to_string(), steps.to_vec()))
        .collect()
}

/// `restart` prepares both of its legs, under the budget it was given, before either sends anything,
/// and holds what it prepared until both are sent, then releases it before the next id's — under
/// every budget, the one-request restart included.
#[skuld::test]
fn restart_holds_what_it_prepared_for_both_legs_until_both_are_sent() {
    let both = [Step::Stop, Step::Start];
    let two_legs: &[(&str, &str)] = &[("stop", "frpc"), ("start", "frpc"), ("stop", "web"), ("start", "web")];
    let native: &[(&str, &str)] = &[("restart", "frpc"), ("restart", "web")];
    for (budget, given, one_request, sent) in [
        (&[][..], Budget::DEFAULT, false, two_legs),
        (&["--no-timeout"][..], Budget::Unbounded, false, two_legs),
        (
            &["--timeout", "5s"][..],
            Budget::Bounded(Duration::from_secs(5)),
            false,
            two_legs,
        ),
        (&["--timeout", "0"][..], Budget::Immediate, false, two_legs),
        (&["--timeout", "0"][..], Budget::Immediate, true, native),
    ] {
        let fake = Fake::new();
        fake.install(&mk("frpc"), false).unwrap();
        fake.install(&mk("web"), false).unwrap();
        if one_request {
            fake.seed_native_restart();
        }
        let seeded = fake.guarded().len();
        let mut args = vec!["goetia", "daemon", "restart", "frpc", "web"];
        args.extend_from_slice(budget);

        let (code, out, err) = dispatch_elevated(&args, &fake);

        assert_eq!(code, 0, "{budget:?}\nstdout:\n{out}\nstderr:\n{err}");
        assert_eq!(
            fake.prepared(),
            vec![(both.to_vec(), given), (both.to_vec(), given)],
            "{budget:?}"
        );
        assert_eq!(fake.guarded()[seeded..], under(&both, sent), "{budget:?}");
    }
}

/// `install --start` prepares the install and the start, under the start's budget, before the
/// install sends anything, and holds what it prepared until the start is sent, then releases it
/// before the next daemon's.
#[skuld::test]
fn install_start_holds_what_it_prepared_until_the_start_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(
        dir.path(),
        "daemons:\n  frpc:\n    command: [daemon]\n  web:\n    command: [daemon]\n",
    );
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--file",
            manifest.to_str().unwrap(),
            "--start",
            "--timeout",
            "5s",
        ],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let both = [Step::Install, Step::Start];
    let given = Budget::Bounded(Duration::from_secs(5));
    assert_eq!(fake.prepared(), vec![(both.to_vec(), given), (both.to_vec(), given)]);
    assert_eq!(
        fake.guarded(),
        under(
            &both,
            &[
                ("install", "frpc"),
                ("start", "frpc"),
                ("install", "web"),
                ("start", "web")
            ]
        )
    );
}

#[skuld::test]
fn install_start_that_times_out_exits_4() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let fake = Fake::new();
    fake.seed_start_stalls("frpc");

    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--start",
            "--timeout",
            "5s",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &fake,
    );

    assert_eq!(code, 4, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("did not report running"), "{err}");
}

/// A command line goetia will not act on runs nothing: `install` without `--start` starts nothing,
/// so a budget named for that start can never be spent.
#[skuld::test]
fn install_refuses_a_timeout_without_start() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--timeout",
            "30s",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &fake,
    );
    // `was_given()` reads both flags, so both reach the same refusal.
    let (no_timeout_code, _, no_timeout_err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--no-timeout",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &fake,
    );

    assert_eq!(code, 2, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("--start"), "the missing flag is named: {err}");
    assert_eq!(no_timeout_code, 2, "{no_timeout_err}");
    assert!(
        installed_ids(&fake).is_empty(),
        "a refused command line installs nothing"
    );
}

/// `--dry-run` keeps ignoring the wait flags exactly as it already ignores `--start`. Refusing this
/// combination while still accepting a silent `--dry-run --start` would be a new inconsistency.
#[skuld::test]
fn install_dry_run_ignores_the_wait_flags() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let fake = Fake::new();

    let (plain_code, plain_out, _) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--dry-run",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &fake,
    );
    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--dry-run",
            "--start",
            "--timeout",
            "30s",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &fake,
    );

    assert_eq!(plain_code, 0);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        out, plain_out,
        "the dry-run plan is printed exactly as it is without the flags"
    );
    assert!(installed_ids(&fake).is_empty(), "--dry-run installs and starts nothing");
}

/// The refusal above is scoped to the runs it can bite: under `--dry-run` a wait flag is inert
/// whether or not `--start` is there, because `--start` is inert too.
#[skuld::test]
fn install_dry_run_ignores_a_timeout_without_start() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let fake = Fake::new();

    let (code, out, err) = dispatch_elevated(
        &[
            "goetia",
            "daemon",
            "install",
            "--dry-run",
            "--timeout",
            "30s",
            "-f",
            manifest.to_str().unwrap(),
        ],
        &fake,
    );

    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("frpc"), "the dry-run plan is still printed: {out}");
    assert!(installed_ids(&fake).is_empty());
}

#[skuld::test]
fn timeout_and_no_timeout_are_mutually_exclusive() {
    let parsed = Cli::try_parse_from(["goetia", "daemon", "start", "frpc", "--timeout", "5s", "--no-timeout"]);

    assert!(parsed.is_err(), "clap must reject naming both bounds at once");
}

/// Every waiting verb takes a variadic id list, so a spaced duration is split by the shell and its
/// tail lands in that list. Pinned as known and documented — the help text's `2m30s` guidance and
/// the README's quoting note exist for exactly this.
#[skuld::test]
fn a_spaced_duration_after_a_variadic_positional_becomes_an_id() {
    let cli = Cli::try_parse_from(["goetia", "daemon", "start", "foo", "--timeout", "2m", "30s"])
        .expect("`30s` parses as a daemon id, not as the tail of the duration");

    let Command::Daemon(DaemonCommand::Start(args)) = &cli.command else {
        panic!("parsed something other than `daemon start`")
    };
    assert_eq!(args.ids, vec!["foo".to_string(), "30s".to_string()]);
}

// --json write failures ===============================================================================================

/// Refuses every write, the way a broken pipe (`goetia --json daemon list | head -1`) or a full
/// disk does.
struct FailingStdout;

impl std::io::Write for FailingStdout {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected write failure",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`dispatch_with`] against a caller-supplied `out` — the one thing a `Vec<u8>` cannot model is a
/// stdout that refuses the document.
fn dispatch_to(args: &[&str], mgr: &Fake, out: &mut dyn std::io::Write) -> (i32, String) {
    let cli = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let mgr = mgr.clone();
    let get_manager = move || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(mgr.clone())) };
    let mut err = Vec::new();
    let code = cli::dispatch(&cli, &get_manager, &never_elevated, out, &mut err);
    (code, String::from_utf8(err).expect("stderr is UTF-8"))
}

/// `--json`'s published contract is "stdout is exactly one JSON document; parse it, then read
/// `errors`". A consumer honouring that would meet `json.loads("")` if an undelivered document
/// could still exit `0` — which is the failure `--json` was built to remove.
#[skuld::test]
fn json_exits_one_when_stdout_refuses_the_document() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    // Every `--json` path, including the `unsupported` refusal, whose own code is `2`.
    for args in [
        &["goetia", "--json", "daemon", "list"][..],
        &["goetia", "--json", "daemon", "status"][..],
        &["goetia", "--json", "daemon", "status", "frpc"][..],
        &["goetia", "--json", "daemon", "show", "frpc"][..],
    ] {
        let (code, err) = dispatch_to(args, &fake, &mut FailingStdout);

        assert_eq!(code, 1, "{args:?}");
        assert!(err.contains("injected write failure"), "{args:?}: {err}");
    }
}

/// The same runs against a stdout that accepts the write: `1` above is the write failure talking,
/// not a code these invocations return anyway.
#[skuld::test]
fn json_returns_its_own_code_when_stdout_accepts_the_document() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();

    for (args, expected) in [
        (&["goetia", "--json", "daemon", "list"][..], 0),
        (&["goetia", "--json", "daemon", "status"][..], 0),
        (&["goetia", "--json", "daemon", "status", "frpc"][..], 0),
        (&["goetia", "--json", "daemon", "show", "frpc"][..], 2),
    ] {
        let mut out = Vec::new();
        let (code, err) = dispatch_to(args, &fake, &mut out);

        assert_eq!(code, expected, "{args:?}: {err}");
        assert!(!out.is_empty(), "{args:?} still emits a document");
    }
}

/// Where no manager can be asked, every verb that reaches it is refused: exit `1`, with the one
/// message. `status` and `list` included, whose refusal is `unavailable` and never a daemon's own
/// `unreadable`, exit `4` — for the whole listing, which is one question, and per id for `status`
/// by id, as every other verb given ids answers per id. The verbs that never reach the manager are
/// untouched.
#[skuld::test]
fn where_no_manager_can_be_asked_every_verb_that_reaches_it_is_refused_whole() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_manifest(dir.path(), "daemons:\n  frpc:\n    command: [daemon]\n");
    let manifest = manifest.to_str().unwrap();
    let fake = || {
        let fake = Fake::new();
        fake.install(&mk("frpc"), false).unwrap();
        fake.seed_no_manager();
        fake
    };
    let refusal = "no running systemd manager can be asked here (seeded)";

    for args in [
        &["goetia", "daemon", "install", "--file", manifest][..],
        &["goetia", "daemon", "uninstall", "frpc"],
        &["goetia", "daemon", "start", "frpc"],
        &["goetia", "daemon", "stop", "frpc"],
        &["goetia", "daemon", "restart", "frpc"],
        &["goetia", "daemon", "restart", "frpc", "--timeout", "0"],
        &["goetia", "daemon", "enable", "frpc"],
        &["goetia", "daemon", "disable", "frpc"],
        &["goetia", "daemon", "status", "frpc"],
        &["goetia", "daemon", "status"],
        &["goetia", "daemon", "list"],
        &["goetia", "daemon", "show", "frpc"],
    ] {
        let (code, out, err) = dispatch_elevated(args, &fake());
        assert_eq!(code, 1, "{args:?}\nstdout:\n{out}\nstderr:\n{err}");
        assert!(err.contains(refusal), "{args:?}: {err}");
    }

    for (args, id) in [
        (
            &["goetia", "--json", "daemon", "status", "frpc"][..],
            serde_json::json!("frpc"),
        ),
        (&["goetia", "--json", "daemon", "status"], serde_json::Value::Null),
        (&["goetia", "--json", "daemon", "list"], serde_json::Value::Null),
    ] {
        let (code, out, _) = dispatch_read_only(args, &fake());
        assert_eq!(code, 1, "{args:?}: {out}");
        let doc = parse_json(&out);
        assert_eq!(daemons(&doc).len(), 0, "{args:?}: {out}");
        let errors = errors(&doc);
        assert_eq!(errors.len(), 1, "{args:?}: {out}");
        assert_eq!(errors[0]["kind"], "unavailable", "{args:?}: {out}");
        assert_eq!(errors[0]["id"], id, "{args:?}: {out}");
        assert!(
            errors[0]["message"].as_str().unwrap().contains(refusal),
            "{args:?}: {out}"
        );
    }

    for args in [
        &["goetia", "daemon", "install", "--dry-run", "--file", manifest][..],
        &["goetia", "daemon", "diff", "--file", manifest],
        &["goetia", "daemon", "show", "--file", manifest],
    ] {
        let (code, out, err) = dispatch_elevated(args, &fake());
        assert_ne!(code, 1, "{args:?}\nstdout:\n{out}\nstderr:\n{err}");
        assert!(!err.contains(refusal), "{args:?}: {err}");
    }
}

/// `status` by id where no manager can be asked still answers every id it can from files, and
/// reports the one it cannot as `unavailable` under its own id: every entry is kept, in argument
/// order, and the exit code is the precedence over all of them. Here `1` — the refusal and the
/// determinate answers alike — over the `4` of an id whose read failed.
#[skuld::test]
fn status_by_id_where_no_manager_can_be_asked_answers_every_other_id() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_opaque("opaque");
    fake.seed_no_manager();

    let (code, out, _) = dispatch_read_only(
        &[
            "goetia", "--json", "daemon", "status", "ghost", "frpc", "opaque", "bad!id",
        ],
        &fake,
    );

    assert_eq!(code, 1, "{out}");
    let doc = parse_json(&out);
    let reported: Vec<(String, String)> = errors(&doc)
        .iter()
        .map(|e| {
            (
                e["id"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        reported,
        [
            ("ghost", "not-installed"),
            ("frpc", "unavailable"),
            ("opaque", "undetermined"),
            ("bad!id", "invalid-id"),
        ]
        .map(|(id, kind)| (id.to_string(), kind.to_string())),
        "{out}"
    );

    let (code, _, err) = dispatch_read_only(&["goetia", "daemon", "status", "opaque", "frpc"], &fake);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("error: opaque: cannot determine"), "{err}");
    assert!(
        err.contains("error: frpc: no running systemd manager can be asked here (seeded)"),
        "{err}"
    );
}
