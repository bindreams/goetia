//! Elevated end-to-end coverage for [`LaunchdManager`] against the real
//! `/Library/Application Support/Goetia/daemons` and `/Library/LaunchDaemons`
//! directories and a real `launchctl`.
//!
//! Every test that mutates anything registers a [`Guard`], which
//! `uninstall`s its id on drop — including on a panicking assertion, since
//! `Drop` still runs during unwinding (the same reasoning
//! `tests/support/service_guard.rs` documents). A few tests seed raw,
//! non-Goetia content directly on disk instead of through the manager (a
//! foreign plist, a hand-edited artifact); those use the narrower
//! [`FileGuard`] instead, since `uninstall` would refuse to touch them.

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use goetia::backend::launchd::manager::{ENABLED_DIR, LaunchdManager, STAGING_DIR};
use goetia::decide::Outcome;
use goetia::manager::{self, Installed, ServiceManager, State};
use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};

use crate::support::{self, ConnectBack, ELEVATED, cmd};

// Fixtures ============================================================================================================

fn base_spec(id: &str, command: Vec<String>) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).expect("valid id"),
        name: id.to_string(),
        command,
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

/// A long-lived, real, harmless daemon — used everywhere the test only
/// needs *something* the manager can genuinely bootstrap/kickstart/bootout,
/// not a specific observable behavior from the process itself.
fn sleepy(id: &str) -> DaemonSpec {
    base_spec(id, vec!["/bin/sleep".to_string(), "300".to_string()])
}

fn staging_dir() -> PathBuf {
    PathBuf::from(STAGING_DIR)
}

fn enabled_dir() -> PathBuf {
    PathBuf::from(ENABLED_DIR)
}

fn staging_path(id: &str) -> PathBuf {
    staging_dir().join(format!("{id}.plist"))
}

fn enabled_path(id: &str) -> PathBuf {
    enabled_dir().join(format!("{id}.plist"))
}

fn is_loaded(id: &str) -> bool {
    cmd::run("launchctl", &["print", &format!("system/{id}")]).ok()
}

/// A plausible plist carrying no Goetia marker at all — what a stranger's
/// pre-existing service, or a hand-seeded fixture, looks like on disk.
fn foreign_plist(label: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/bin/true</string>\n\t</array>\n\
         </dict>\n</plist>\n"
    )
}

fn write_plist(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent dir");
    std::fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod plist");
}

// Guards ==============================================================================================================

/// Installs `spec` through `mgr` and `uninstall`s it on drop. Covers both
/// the staging and enabled locations — `uninstall` locates the artifact
/// wherever it currently lives — so a test that calls `enable` doesn't need
/// a second guard.
struct Guard(Id);

impl Guard {
    fn install(mgr: &LaunchdManager, spec: &DaemonSpec) -> Self {
        mgr.install(spec, false).expect("seed install");
        Self(spec.id.clone())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        match LaunchdManager::new().uninstall(&self.0) {
            Ok(()) | Err(goetia::Error::NotInstalled { .. }) => {}
            Err(e) => eprintln!("Guard[{}]: cleanup failed: {e}", self.0),
        }
    }
}

/// Removes a plist written directly to disk (bypassing the manager) on
/// drop — for content `uninstall` would refuse to touch because it carries
/// no Goetia marker at all.
struct FileGuard(PathBuf);

impl Drop for FileGuard {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("FileGuard[{}]: cleanup failed: {e}", self.0.display()),
        }
    }
}

// The deliverable: conformance ========================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn launchd_passes_conformance() {
    let mgr = LaunchdManager::new();

    // Seed `FOREIGN_ID`: non-Goetia content the conformance suite itself
    // never writes to or removes (see `manager::conformance`'s module doc
    // comment) — cleaned up here, not by `run`.
    let foreign_path = staging_path(manager::conformance::FOREIGN_ID);
    write_plist(&foreign_path, &foreign_plist(manager::conformance::FOREIGN_ID));
    let _foreign_cleanup = FileGuard(foreign_path);

    // Seed `HAND_EDITED_ID`: install normally, then hand-edit the result so
    // it no longer matches what regenerating its own embedded spec would
    // produce. `run` uninstalls this one itself.
    let spec = sleepy(manager::conformance::HAND_EDITED_ID);
    mgr.install(&spec, false).expect("seed install");
    let path = staging_path(manager::conformance::HAND_EDITED_ID);
    let mut text = std::fs::read_to_string(&path).expect("read seeded artifact");
    text.push_str("<!-- a hand-added directive -->\n");
    write_plist(&path, &text);

    manager::conformance::run(&mgr, &sleepy);
}

// Round trips and drift ===============================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_round_trips_without_a_source_file() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    // Nothing outside the installed artifact backs this spec to begin with
    // — no `goetia.yaml` was ever involved — so `list` returning it intact
    // is exactly the "the artifact is the only source of truth" property
    // this proves.
    let listed = mgr.list().expect("list");
    let found = listed
        .into_iter()
        .find(|entry| matches!(entry, Installed::Ours { spec: s, .. } if s.id == spec.id));
    match found {
        Some(Installed::Ours { spec: got, .. }) => assert_eq!(got, spec),
        other => panic!("expected the installed spec back from list(), got {other:?}"),
    }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn hand_edit_is_detected_as_conflict() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    let path = staging_path(spec.id.as_str());
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("<!-- a hand-added directive -->\n");
    write_plist(&path, &text);

    let outcome = mgr
        .install(&spec, false)
        .expect("install over a hand-edited artifact must not error");
    match outcome {
        Outcome::Conflict { artifact_diff } => assert!(!artifact_diff.is_empty(), "diff must be non-empty"),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

/// The genuine TOCTOU race this guards against — a foreign write landing
/// between `discover`'s read and the actual persist — is unit-tested
/// directly against `write_new` in
/// `src/backend/launchd/manager_tests.rs::write_new_does_not_clobber_existing_content`,
/// where the target is a plain tempdir path and the timing is therefore
/// deterministic rather than dependent on winning a race with the
/// scheduler. This is the end-to-end complement, through the real
/// `STAGING_DIR`: install must never destroy content that is already
/// sitting where it is about to write.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn create_does_not_clobber_a_unit_that_appears_after_classification() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    let foreign_content = "not a goetia artifact, seeded directly\n";
    write_plist(&path, foreign_content);
    let _cleanup = FileGuard(path.clone());

    let spec = sleepy(&id);
    let outcome = mgr
        .install(&spec, false)
        .expect("install over a foreign id must not error");
    assert!(
        matches!(outcome, Outcome::RefuseForeign { .. }),
        "expected RefuseForeign, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        foreign_content,
        "install must never overwrite foreign content"
    );
}

// Verb-by-verb enrollment semantics ===================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_does_not_start_the_daemon() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    assert!(
        !is_loaded(spec.id.as_str()),
        "install must not load the job at all — it must not appear in `launchctl print system`"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn start_does_not_enable() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    mgr.start(&spec.id).expect("start");

    assert!(
        staging_path(spec.id.as_str()).exists(),
        "plist must still be in staging after start"
    );
    assert!(
        !enabled_path(spec.id.as_str()).exists(),
        "plist must not have moved into LaunchDaemons after a plain start"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn enable_does_not_start() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    mgr.enable(&spec.id).expect("enable");

    assert!(
        enabled_path(spec.id.as_str()).exists(),
        "plist must have moved into LaunchDaemons"
    );
    assert!(
        !staging_path(spec.id.as_str()).exists(),
        "plist must not remain in staging"
    );
    assert!(!is_loaded(spec.id.as_str()), "enable must not load/start the job");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn disable_returns_the_plist_to_staging() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let spec = base_spec(
        &id,
        vec![
            support::current_exe_str(),
            support::sentinel::RUN_UNTIL_KILLED.to_string(),
            listener.port().to_string(),
        ],
    );
    let _guard = Guard::install(&mgr, &spec);

    mgr.enable(&spec.id).expect("enable");
    mgr.start(&spec.id).expect("start");
    listener.accept("the daemon to report in after start");
    assert_eq!(
        mgr.status(&spec.id).unwrap().state,
        State::Running,
        "precondition: actually running"
    );

    mgr.disable(&spec.id).expect("disable");

    assert!(staging_path(spec.id.as_str()).exists(), "plist must be back in staging");
    assert!(
        !enabled_path(spec.id.as_str()).exists(),
        "plist must no longer be in LaunchDaemons"
    );
    // `ServiceManager::disable`'s contract is "does not stop it if
    // running" — a currently-loaded job must survive the plist moving back
    // to staging untouched.
    assert_eq!(
        mgr.status(&spec.id).unwrap().state,
        State::Running,
        "disable must not stop a job that was loaded"
    );
    mgr.stop(&spec.id).expect("stop"); // tidy up before the guard's uninstall
}

// Runtime behavior ====================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn start_stop_status_reflect_reality() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    // `RUN_UNTIL_KILLED`, not `sleepy`'s plain `/bin/sleep`: connecting back
    // is what lets this assert `start` actually succeeded — the process is
    // confirmed to be genuinely executing, not merely that `launchctl`
    // returned success — before trusting `status` to report `Running`, and
    // it then keeps running so that status check has something to observe.
    let spec = base_spec(
        &id,
        vec![
            support::current_exe_str(),
            support::sentinel::RUN_UNTIL_KILLED.to_string(),
            listener.port().to_string(),
        ],
    );
    let _guard = Guard::install(&mgr, &spec);

    assert_eq!(mgr.status(&spec.id).unwrap().state, State::Stopped);

    mgr.start(&spec.id).expect("start");
    listener.accept("the daemon to report in after start");
    let running = mgr.status(&spec.id).unwrap();
    assert_eq!(running.state, State::Running);
    assert!(running.pid.is_some(), "a running job should report a pid");

    mgr.stop(&spec.id).expect("stop");
    assert_ne!(mgr.status(&spec.id).unwrap().state, State::Running);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_leaves_nothing() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let spec = sleepy(&id);
    // `Guard::drop` re-running `uninstall` after the explicit call below is
    // a harmless, already-gone no-op — see its `Drop` impl — so this still
    // doubles as the panic-safety net every other test gets.
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.id).expect("start");

    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(!staging_path(&id).exists(), "no plist should remain in staging");
    assert!(!enabled_path(&id).exists(), "no plist should remain in LaunchDaemons");
    assert!(!is_loaded(&id), "job must not still be loaded after uninstall");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn list_ignores_foreign_plists() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    write_plist(&path, &foreign_plist(&id));
    let _cleanup = FileGuard(path);

    let listed = mgr.list().expect("list");
    assert!(
        !listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.id.as_str() == id)),
        "a foreign plist must not be reported as Ours"
    );
    assert!(
        !listed
            .iter()
            .any(|e| matches!(e, Installed::OursUnreadable { name, .. } if name == &id)),
        "a foreign plist carries no marker at all, so it must not even be OursUnreadable"
    );
}

/// `list`/`status`/`show` need no elevation (design spec §4): every plist
/// and every ancestor directory `install` creates is world-readable, so a
/// completely unprivileged process must be able to run `goetia daemon
/// list` — spawned here as the `nobody` account, exactly the account an
/// everyday non-root invocation would run under. Installs a real daemon
/// first and asserts it actually appears in the unelevated output: exit
/// code `0` alone proves nothing — `list()` treats a missing/empty
/// `STAGING_DIR` and a per-entry read failure identically (silently
/// omitted), so a command that ran against nothing would "succeed" even if
/// every installed plist were `0600 root:wheel`.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn unelevated_list_works() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    let uid: u32 = cmd::run("id", &["-u", "nobody"])
        .stdout
        .trim()
        .parse()
        .expect("nobody's uid");
    let gid: u32 = cmd::run("id", &["-g", "nobody"])
        .stdout
        .trim()
        .parse()
        .expect("nobody's gid");

    // Run a copy of the binary from `/tmp`, not `CARGO_BIN_EXE_goetia`
    // directly: on CI the cargo target directory lives under the runner's
    // own home directory, which is not guaranteed traversable by an
    // arbitrary low-privilege account — a failure that would be about the
    // runner's directory layout, not about Goetia's own file permissions,
    // which are what this test needs to prove. `/tmp` (the literal path,
    // not `std::env::temp_dir()` — macOS's per-user `$TMPDIR` is `0700`)
    // is world-traversable on every supported platform.
    let copy_path = Path::new("/tmp").join(format!("goetia-unelevated-test-{}", support::random_test_id()));
    std::fs::copy(env!("CARGO_BIN_EXE_goetia"), &copy_path).expect("copy goetia binary to /tmp");
    std::fs::set_permissions(&copy_path, std::fs::Permissions::from_mode(0o755)).expect("chmod copy");
    let _cleanup = FileGuard(copy_path.clone());

    let out = Command::new(&copy_path)
        .args(["daemon", "list"])
        .uid(uid)
        .gid(gid)
        .output()
        .expect("spawn goetia as `nobody`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains(spec.id.as_str()),
        "the installed daemon must actually appear in the unelevated `list` output; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// Verifies that launchd's `UserName` honours the account this backend
/// resolves `user:` to (always a real name, never a numeric uid — see
/// `resolve_account`'s doc comment): this is the empirical proof that the
/// resolved account actually runs the job, for the most consequential case.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn root_user_runs_as_uid_zero() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let spec = base_spec(
        &id,
        vec![
            support::current_exe_str(),
            support::sentinel::REPORT_UID.to_string(),
            listener.port().to_string(),
        ],
    );
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.id).expect("start");

    let reported_uid = listener.accept_value("the daemon to report its uid");
    assert_eq!(
        reported_uid, "0",
        "a `user: root` daemon must run as uid 0, got {reported_uid}"
    );
}

// Parent directories ==================================================================================================

/// `install` `create_dir_all`s `cwd` and the parent of `logs`, then hands
/// each to the daemon's own account — otherwise a non-root daemon whose
/// `WorkingDirectory` or `StandardOutPath` names a directory only root can
/// write into fails at launch with an opaque status.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_creates_and_owns_cwd_and_logs_parent_for_a_non_root_account() {
    let nobody_uid: u32 = cmd::run("id", &["-u", "nobody"])
        .stdout
        .trim()
        .parse()
        .expect("nobody's uid");
    let nobody_gid: u32 = cmd::run("id", &["-g", "nobody"])
        .stdout
        .trim()
        .parse()
        .expect("nobody's gid");

    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let cwd = Path::new("/tmp").join(format!("{id}-cwd"));
    let logs = Path::new("/tmp").join(format!("{id}-logs")).join("out.log");
    let _cleanup_cwd = DirGuard(cwd.clone());
    let _cleanup_logs_parent = DirGuard(logs.parent().unwrap().to_path_buf());

    let mut spec = base_spec(&id, vec!["/bin/true".to_string()]);
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());
    spec.logs = Some(logs.clone());
    let _guard = Guard::install(&mgr, &spec);

    for (dir, what) in [(&cwd, "cwd"), (logs.parent().unwrap(), "logs' parent")] {
        let meta = std::fs::metadata(dir).unwrap_or_else(|e| panic!("{what} ({}) must exist: {e}", dir.display()));
        assert!(meta.is_dir(), "{what} ({}) must be a directory", dir.display());
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o755,
            "{what} ({}) must be 0755",
            dir.display()
        );
        assert_eq!(
            meta.uid(),
            nobody_uid,
            "{what} ({}) must be owned by nobody",
            dir.display()
        );
        assert_eq!(
            meta.gid(),
            nobody_gid,
            "{what} ({}) must be grouped as nobody",
            dir.display()
        );
    }
}

/// Removes a directory `install` created (bypassing the manager, which has
/// no verb for it) on drop.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("DirGuard[{}]: cleanup failed: {e}", self.0.display()),
        }
    }
}

// Non-regular occupants ===============================================================================================

/// Regression coverage for the `locate`/`write_new` classification
/// mismatch: `Path::is_file` (what `locate` used to check) is `false` for
/// a directory, while `link`(2) (what `persist_noclobber` uses) refuses to
/// create over one exactly as it would over a regular file. That mismatch
/// made a directory sitting at a fresh id's staging path classify as
/// `Absent` -> `Create` -> `write_new` -> `Raced` -> re-`install` -> the
/// identical classification, an unbounded recursion that would eventually
/// abort the process. `locate` now uses `symlink_metadata`, so a directory
/// is "occupied" like anything else, and `discover`'s subsequent read
/// fails cleanly instead.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_over_a_directory_errors_instead_of_recursing() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    std::fs::create_dir_all(&path).expect("seed a directory at the staging path");
    let _cleanup = DirGuard(path);

    let spec = sleepy(&id);
    // The mere fact this returns at all (rather than stack-overflowing)
    // is most of what this test proves; asserting `Err` pins the rest.
    mgr.install(&spec, false)
        .expect_err("a directory occupying the target path must not be silently treated as absent");
}
