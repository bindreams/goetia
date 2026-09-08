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

/// Held by every test whose assertion is about `list`'s answer for the
/// *whole host* — its exit code, or the absence of any `undetermined` entry
/// — and by every test that seeds something unreadable into the shared
/// [`ENABLED_DIR`]/[`STAGING_DIR`], which changes that answer. The two
/// groups are the same group precisely because either invalidates the other,
/// so they take turns rather than race.
///
/// Named for systemd's unit directory rather than for launchd's plist ones
/// deliberately: skuld coordinates serialization across processes by label
/// *name*, and `tests/cli_binary.rs::native_backend_answers_list_unelevated`
/// runs a real `daemon list` — on macOS, through this backend — holding that
/// same name.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

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

/// A real binary plist, produced the way a macOS host produces them rather
/// than hand-assembled: `plutil -convert binary1` is what `defaults write`
/// and countless vendor build scripts leave behind in `/Library/
/// LaunchDaemons`, and goetia must read that directory without tripping
/// over them.
fn binary_plist(label: &str) -> Vec<u8> {
    let source = std::env::temp_dir().join(format!("{label}-binary.plist"));
    write_bytes(&source, foreign_plist(label).as_bytes());
    cmd::run(
        "plutil",
        &["-convert", "binary1", source.to_str().expect("a UTF-8 temp path")],
    )
    .expect_ok();
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    let _ = std::fs::remove_file(&source);
    assert!(
        bytes.starts_with(b"bplist00"),
        "`plutil -convert binary1` must produce a binary plist, got {:?}",
        &bytes[..bytes.len().min(16)]
    );
    bytes
}

/// Bytes that are neither UTF-8 nor a binary plist: nothing about them is
/// identified, and — unlike a permission denial — root meets exactly the
/// same wall an unprivileged caller does, which is what lets a test seed
/// this and read it back in one elevated process.
const UNDECODABLE_PLIST: [u8; 4] = [b'<', 0xff, 0xfe, b'>'];

fn write_plist(path: &Path, content: &str) {
    write_bytes(path, content.as_bytes());
}

fn write_bytes(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent dir");
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod plist");
}

/// Whether `listed` leaves `id` possible — the basis for every negative
/// assertion below. An aggregate `Undetermined` entry counts even though it
/// names nothing: it may stand for `id` itself, so an assertion that treated
/// it as silence would certify what the listing cannot establish (see
/// [`Installed::Undetermined`]'s null-name rule).
fn may_account_for(listed: &[Installed], id: &str) -> bool {
    listed.iter().any(|entry| match entry {
        Installed::Ours { spec, .. } => spec.id.as_str() == id,
        Installed::OursUnreadable { name, .. } => name == id,
        Installed::Undetermined { name, .. } => name.is_none() || name.as_deref() == Some(id),
    })
}

fn undetermined_named(listed: &[Installed], id: &str) -> Option<String> {
    listed.iter().find_map(|entry| match entry {
        Installed::Undetermined { name, reason } if name.as_deref() == Some(id) => Some(reason.clone()),
        _ => None,
    })
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

/// `UNIT_DIR_EXCLUSIVE`: seeding `UNDETERMINED_ID` puts a plist nothing can decode into the shared
/// [`STAGING_DIR`] for the length of the run, which is exactly what the host-wide listing assertions
/// elsewhere in this file (and `tests/cli_binary.rs`'s real `daemon list`) are about.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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

    // Seed `UNDETERMINED_ID`: bytes that are neither UTF-8 nor a binary plist, so nothing about
    // the file — its marker included — is ever established. Cleaned up here, like `FOREIGN_ID`.
    let undetermined_path = staging_path(manager::conformance::UNDETERMINED_ID);
    write_bytes(&undetermined_path, &UNDECODABLE_PLIST);
    let _undetermined_cleanup = FileGuard(undetermined_path);

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
        Outcome::Conflict { artifact_diff, .. } => assert!(!artifact_diff.is_empty(), "diff must be non-empty"),
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
/// first and asserts it actually appears in the unelevated output: an exit
/// code alone proves nothing, since a command that ran against nothing
/// produces the same one.
///
/// The exit code is a claim about the *whole host*: `/Library/LaunchDaemons`
/// holds every vendor's daemons, and any one of them `nobody` cannot open is
/// now an `undetermined` entry and a `4`. So this asserts the invariant that
/// is actually this test's — our daemon is listed, and is not what the
/// caller failed to determine — plus the tie between the third key and the
/// code, and leaves an unrelated vendor plist free to move that code without
/// failing a test that is not about it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_works() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    let listed = unelevated_list_json();

    let id = Some(spec.id.as_str().to_string());
    assert!(
        listed.ids("daemons").contains(&id),
        "the installed daemon must actually appear in the unelevated `list` output: {}",
        listed.context
    );
    assert!(
        !listed.ids("undetermined").contains(&id),
        "`install` writes an 0644 plist under world-searchable directories, so an unprivileged \
         caller reads it: {}",
        listed.context
    );
    assert!(
        matches!(listed.code, Some(0 | 4)),
        "an unelevated `list` must not fail: {}",
        listed.context
    );
    if !listed.ids("undetermined").is_empty() {
        assert_eq!(
            listed.code,
            Some(4),
            "an entry goetia could not determine is exactly what exit `4` reports: {}",
            listed.context
        );
    }
}

/// The motivating defect, end to end and at the privilege boundary that
/// produces it. A plist the caller cannot open must be named as
/// `undetermined` — never left out of the document, which says it is not
/// there — and must *not* be listed as one of goetia's own, since the marker
/// that would have said so is precisely what went unread.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_reports_a_root_only_plist_as_undetermined() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);
    std::fs::set_permissions(staging_path(spec.id.as_str()), std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600");

    let listed = unelevated_list_json();

    // Root opens this plist, decodes its marker and lists it under `daemons` at exit `0`, so a run
    // whose reader was not in fact unprivileged fails here instead of passing vacuously.
    let id = Some(spec.id.as_str().to_string());
    assert_eq!(listed.code, Some(4), "{}", listed.context);
    assert!(listed.ids("undetermined").contains(&id), "{}", listed.context);
    assert!(
        !listed.ids("daemons").contains(&id),
        "a plist goetia could not open is not a daemon it can report the state of: {}",
        listed.context
    );
}

/// Bytes that are neither UTF-8 nor a binary plist settle nothing about the
/// id, so it is reported rather than dropped — and one such plist must not
/// take down the listing of every other daemon on the host.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_reports_an_unreadable_plist_as_undetermined() {
    let mgr = LaunchdManager::new();
    let readable = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &readable);

    let id = support::random_test_id();
    let path = enabled_path(&id);
    write_bytes(&path, &UNDECODABLE_PLIST);
    let _cleanup = FileGuard(path.clone());

    let listed = mgr
        .list()
        .expect("one undecodable plist must not take down the whole listing");

    let reason = undetermined_named(&listed, &id)
        .unwrap_or_else(|| panic!("bytes goetia could not decode settle nothing about the id: {listed:?}"));
    assert!(
        reason.contains(&path.display().to_string()),
        "the reason must name the path that could not be read: {reason}"
    );
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.id == readable.id)),
        "every other daemon is still listed: {listed:?}"
    );
}

/// The macOS-wide regression the binary-plist rule prevents. `/Library/
/// LaunchDaemons` is every vendor's directory, and a `bplist00` plist is a
/// fully valid launchd artifact that `plutil`/`defaults` produce by default
/// — so treating "not UTF-8" as undetermined would hand a normal macOS host
/// a standing, unclearable `undetermined` entry and a permanent exit `4` for
/// a service that is in no way goetia's business.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_stays_clean_on_a_host_carrying_binary_plists() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = enabled_path(&id);
    write_bytes(&path, &binary_plist(&id));
    let _cleanup = FileGuard(path);

    let listed = mgr.list().expect("list");

    assert!(
        !may_account_for(&listed, &id),
        "a binary plist carries no goetia marker by construction, so it is foreign and omitted \
         exactly like an unmarked XML one: {listed:?}"
    );
    let undetermined: Vec<&Installed> = listed
        .iter()
        .filter(|e| matches!(e, Installed::Undetermined { .. }))
        .collect();
    assert!(
        undetermined.is_empty(),
        "nothing on this host was left undetermined, so `list` must report nothing under that key: \
         {undetermined:?}"
    );
}

/// The `discover` path — the one `install`/`preview_install` reads through,
/// and therefore what `goetia daemon diff` answers from. Leaving it on a
/// bare `read_to_string` would have `diff` report `Error::Io` (exit `1`)
/// while `status` reports `Error::Undetermined` (exit `4`) for the very same
/// file: two subcommands disagreeing about one machine.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn preview_install_over_an_unreadable_plist_is_undetermined() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    write_bytes(&path, &UNDECODABLE_PLIST);
    let _cleanup = FileGuard(path);

    let spec = sleepy(&id);

    let preview = mgr
        .preview_install(&spec)
        .expect_err("bytes goetia could not decode settle nothing about the id");
    assert!(matches!(preview, goetia::Error::Undetermined { .. }), "{preview:?}");

    let status = mgr
        .status(&spec.id)
        .expect_err("status must reach the same verdict as diff about one file");
    assert!(matches!(status, goetia::Error::Undetermined { .. }), "{status:?}");
}

/// Run `goetia <args>` as the `nobody` account and parse `--json`'s
/// document.
///
/// The binary is copied to `/tmp` first, rather than run from
/// `CARGO_BIN_EXE_goetia` in place: on CI the cargo target directory lives
/// under the runner's own home directory, which is not guaranteed
/// traversable by an arbitrary low-privilege account — a failure that would
/// be about the runner's directory layout, not about goetia's own file
/// permissions, which are what these tests need to prove. `/tmp` (the
/// literal path, not `std::env::temp_dir()` — macOS's per-user `$TMPDIR` is
/// `0700`) is world-traversable.
fn unelevated_list_json() -> ListJson {
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

    let copy_path = Path::new("/tmp").join(format!("goetia-unelevated-test-{}", support::random_test_id()));
    std::fs::copy(env!("CARGO_BIN_EXE_goetia"), &copy_path).expect("copy goetia binary to /tmp");
    std::fs::set_permissions(&copy_path, std::fs::Permissions::from_mode(0o755)).expect("chmod copy");
    let _cleanup = FileGuard(copy_path.clone());

    let output = Command::new(&copy_path)
        .args(["daemon", "list", "--json"])
        .uid(uid)
        .gid(gid)
        .output()
        .expect("spawn goetia as `nobody`");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");
    let doc = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {context}"));
    ListJson {
        doc,
        code: output.status.code(),
        context,
    }
}

/// The one document `--json` promises, plus the raw streams for a failure
/// message.
struct ListJson {
    doc: serde_json::Value,
    code: Option<i32>,
    context: String,
}

impl ListJson {
    fn ids(&self, key: &str) -> Vec<Option<String>> {
        self.doc[key]
            .as_array()
            .unwrap_or_else(|| panic!("`{key}` must always be present, as an array: {}", self.context))
            .iter()
            .map(|entry| match &entry[if key == "daemons" { "id" } else { "name" }] {
                serde_json::Value::Null => None,
                serde_json::Value::String(name) => Some(name.clone()),
                other => panic!("a name must be a string or null: {other}"),
            })
            .collect()
    }
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

    for (dir, what) in [(cwd.as_path(), "cwd"), (logs.parent().unwrap(), "logs' parent")] {
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
/// is "occupied" like anything else, and `discover`'s read classifies it
/// before opening it.
///
/// The refusal is `RefuseForeign`, not an `Err`: a directory at a plist path
/// is not something goetia failed to read, it is positively not a plist, and
/// naming the remedy beats a bare error. That is the same verdict the
/// systemd backend reaches for a non-regular file at a fragment path.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_over_a_directory_refuses_instead_of_recursing() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    std::fs::create_dir_all(&path).expect("seed a directory at the staging path");
    let _cleanup = DirGuard(path);

    let spec = sleepy(&id);
    // The mere fact this returns at all (rather than stack-overflowing) is
    // most of what this test proves; asserting the outcome pins the rest.
    let outcome = mgr
        .install(&spec, false)
        .expect("a directory at the target path is a refusal, not a failure");
    assert!(
        matches!(outcome, Outcome::RefuseForeign { .. }),
        "a directory occupying the target path must not be silently treated as absent: {outcome:?}"
    );
}

/// `open(2)` on a FIFO with `O_RDONLY` blocks until a writer arrives, and
/// `list` reads every `*.plist` name in `/Library/LaunchDaemons` — so one
/// `mkfifo` there would wedge the listing for the whole host. Classifying
/// the path before opening it is what keeps that from happening.
///
/// No `UNIT_DIR_EXCLUSIVE`: a FIFO is omitted rather than reported, so it
/// moves neither the host's `undetermined` set nor `list`'s exit code, and
/// this asserts about its own id alone.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn list_does_not_hang_on_a_fifo_in_the_plist_directory() {
    let id = support::random_test_id();
    let path = enabled_path(&id);
    mkfifo(&path);
    let _cleanup = FileGuard(path);

    let listed = without_blocking("LaunchdManager::list", || LaunchdManager::new().list()).expect("list");

    assert!(
        !may_account_for(&listed, &id),
        "a FIFO is not a plist, and nothing about it was left undetermined: {listed:?}"
    );
}

fn mkfifo(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent dir");
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a plist path with no NUL");
    // SAFETY: `c_path` is a NUL-terminated pointer valid for the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

/// Run `f` on its own thread and report — rather than hang — if it never
/// returns.
///
/// The bound is a failure bound on a kernel wait that may genuinely never
/// end: `open(2)` on a FIFO with `O_RDONLY` blocks until a writer arrives,
/// and nothing here ever creates one. It synchronizes nothing, and no
/// passing run's outcome depends on its value — only how long a regression
/// takes to be *reported* instead of wedging the whole suite, which is what
/// an unguarded call would do.
fn without_blocking<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || drop(tx.send(f())));
    rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap_or_else(|_| {
        panic!("{what} never returned: it opened the FIFO for reading, which blocks until a writer arrives")
    })
}

/// A job loaded under our label from a *different* plist must not be
/// mistaken for our own.
///
/// `start` used to ask two label-scoped questions — "is `system/<label>`
/// loaded?" and "is it running?" — and skip both bootstrap and kickstart
/// when both said yes. Neither can tell *whose* job answers to that label.
/// A predecessor job, or one an external actor booted out that has not
/// finished tearing down, satisfies both, so `start` returned `Ok` having
/// done nothing and the daemon never ran.
///
/// This is not hypothetical: it silently no-op'd `install --start` during a
/// real migration off hand-written LaunchDaemons, leaving the daemon
/// stopped while the command reported success.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn start_is_not_fooled_by_a_stale_job_holding_the_label() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();

    // Ours: `restart: always`, so launchd guarantees it runs once loaded.
    let mut spec = sleepy(&id);
    spec.restart = Restart::Always;

    // The decoy takes the label *before* install, which is the real
    // sequence: a predecessor job is still loaded when Goetia installs over
    // the same id.
    let decoy_dir = std::env::temp_dir().join(format!("{id}-decoy"));
    std::fs::create_dir_all(&decoy_dir).expect("decoy dir");
    let decoy = decoy_dir.join(format!("{id}.plist"));
    std::fs::write(
        &decoy,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{id}</string>
  <key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#
        ),
    )
    .expect("write decoy plist");
    let _decoy_cleanup = FileGuard(decoy.clone());
    cmd::run("launchctl", &["bootstrap", "system", decoy.to_str().unwrap()]);

    let _guard = Guard::install(&mgr, &spec);

    // The decoy now holds the label and is running, which is exactly the
    // state that used to make `start` a no-op.
    mgr.start(&spec.id)
        .expect("start must recover the label, not report a false success");

    let status = mgr.status(&spec.id).expect("status");
    assert_eq!(
        status.state,
        State::Running,
        "our daemon must be running after start; a stale job holding the label must not          be mistaken for it"
    );

    // And it must be *ours*: the decoy sleeps 600, ours sleeps 300.
    let running = cmd::run("pgrep", &["-fl", "sleep"]);
    assert!(
        running.stdout.contains("sleep 300"),
        "the running process should be our daemon (`sleep 300`), not the decoy (`sleep 600`):\n{}",
        running.stdout
    );

    let _ = cmd::run("launchctl", &["bootout", &format!("system/{id}")]);
    let _ = std::fs::remove_dir_all(&decoy_dir);
}
