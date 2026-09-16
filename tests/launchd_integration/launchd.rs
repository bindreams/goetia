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
use std::time::Duration;

use goetia::backend::launchd::manager::{ENABLED_DIR, LaunchdManager, STAGING_DIR};
use goetia::decide::Outcome;
use goetia::manager::{self, Budget, Installed, ServiceManager, State};
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

/// Bytes that are not UTF-8 and do not begin `bplist00`: still not the UTF-8 XML
/// `generate::plist` writes, so still a positive identification of a plist
/// goetia did not write. `FF FE` is also a UTF-16LE BOM, which is what a legal
/// UTF-16 property list starts with — the exact file that used to hand a macOS
/// host a permanent exit `4` for a vendor's service.
const NON_UTF8_PLIST: [u8; 4] = [b'<', 0xff, 0xfe, b'>'];

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

/// `UNIT_DIR_EXCLUSIVE`: the run installs, forces and uninstalls a dozen daemons in the shared
/// [`STAGING_DIR`] for its whole length, which is exactly what the host-wide listing assertions
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

    // Seed `UNDETERMINED_ID`: a symlink to itself. `stat` answers `ELOOP` for every uid — root
    // included, which is what a suite that has to be root to install anything at all needs — so
    // `obtain` gets no bytes, while `locate`'s `symlink_metadata` still sees the entry. Something is
    // at the id and nothing about it was established. Bytes that *arrive* and will not decode are
    // the other state (`Foreign`, and `list_omits_a_non_utf8_plist_as_foreign` covers it), which is
    // why this is not `NON_UTF8_PLIST`. Cleaned up here, like `FOREIGN_ID`.
    let undetermined_path = staging_path(manager::conformance::UNDETERMINED_ID);
    let _ = std::fs::remove_file(&undetermined_path);
    std::os::unix::fs::symlink(
        format!("{}.plist", manager::conformance::UNDETERMINED_ID),
        &undetermined_path,
    )
    .expect("plant the self-referential symlink");
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

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

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
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
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
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop"); // tidy up before the guard's uninstall
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

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to report in after start");
    let running = mgr.status(&spec.id).unwrap();
    assert_eq!(running.state, State::Running);
    assert!(running.pid.is_some(), "a running job should report a pid");

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
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
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

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

/// Bytes that are not the UTF-8 XML goetia writes are foreign by positive identification, so the id
/// is omitted exactly as an unmarked XML plist's is — never reported, and never claimed. One such
/// plist must also not take down the listing of every other daemon on the host.
///
/// This is the `bplist00` rule applied to the negative it always belonged to. `/Library/
/// LaunchDaemons` is every vendor's directory; answering `undetermined` here hands a normal macOS
/// host a standing, unclearable entry and a permanent exit `4` for one vendor plist saved as
/// UTF-16.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_omits_a_non_utf8_plist_as_foreign() {
    let mgr = LaunchdManager::new();
    let readable = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &readable);

    let id = support::random_test_id();
    let path = enabled_path(&id);
    write_bytes(&path, &NON_UTF8_PLIST);
    let _cleanup = FileGuard(path.clone());

    let listed = mgr
        .list()
        .expect("one undecodable plist must not take down the whole listing");

    assert!(
        !may_account_for(&listed, &id),
        "bytes that are not goetia's format leave the id neither claimed nor undetermined: \
         {listed:?}"
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

/// The `discover` path — the one `install`/`preview_install` reads through, and therefore what
/// `goetia daemon diff` answers from — reaching the same verdict as `status` and `list` about one
/// file. A plist that is not goetia's format is a stranger's artifact at this id, so `install`
/// refuses it with the remedy `Outcome::RefuseForeign` names, rather than erroring.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn preview_install_over_a_non_utf8_plist_refuses_it_as_foreign() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    write_bytes(&path, &NON_UTF8_PLIST);
    let _cleanup = FileGuard(path);

    let spec = sleepy(&id);

    let preview = mgr
        .preview_install(&spec)
        .expect("bytes that were obtained settle the id, so this is a refusal and not an error");
    assert!(matches!(preview, Outcome::RefuseForeign { .. }), "{preview:?}");

    // Never `NotInstalled`: something is demonstrably there.
    let status = mgr
        .status(&spec.id)
        .expect_err("status must reach the same verdict as diff about one file");
    assert!(matches!(status, goetia::Error::Foreign { .. }), "{status:?}");
}

/// [`locate`]'s two unresolved-probe arms, at the privilege boundary that produces them — the one
/// call site that decides between reporting an id and dropping it. A stat that could not be
/// performed establishes no absence, so an id under an unsearchable plist directory is exit `4` and
/// a named path; `symlink_metadata(..).is_ok()` used to answer "nothing is there", which made every
/// verb report a daemon that is right there as `NotInstalled`.
///
/// One iteration per arm. `locate` probes both directories and each answers for the *pair*, so
/// denying the staging directory reaches the first arm, and denying `/Library/LaunchDaemons` alone
/// — staging searchable, and this id absent from it — reaches the second. Each arm names its own
/// directory in the message, which is what tells them apart here.
///
/// `manager_tests.rs::occupied_distinguishes_a_denied_stat_from_absence` covers the `Presence`
/// mapping itself, with an `ENOTDIR` that holds under both uids. What only this test reaches is
/// `locate`'s use of it: a fixture whose parent is a regular file cannot be planted on either of
/// these two real directories without taking every other test's artifacts with it.
///
/// The staging half denies the *parent* of [`STAGING_DIR`] rather than that directory: `install`
/// and `disable` both call `ensure_dir(STAGING_DIR)`, which chmods it `0755` unconditionally, so a
/// concurrently running test that installs anything would lift the denial mid-run. Nothing goetia
/// does chmods either that parent or [`ENABLED_DIR`], so both denials hold for as long as the guard
/// does.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn an_unsearchable_plist_directory_is_undetermined_rather_than_absent() {
    let id = support::random_test_id();
    // `install` creates these on first use; nothing guarantees this host has installed anything yet.
    // The modes are restated rather than left to the umask, since the baseline below is exactly the
    // claim that an unprivileged reader can traverse them — `ensure_dir` restates `0755` too.
    std::fs::create_dir_all(staging_dir()).expect("create the staging directory");
    let staging_parent = staging_dir()
        .parent()
        .expect("the staging directory is not a filesystem root")
        .to_path_buf();
    for dir in [&staging_parent, &staging_dir()] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod 0755 {}: {e}", dir.display()));
    }

    // Non-vacuity: with both directories searchable, this id is *determinately* absent — exit `1`,
    // the answer the assertions below reject.
    let baseline = run_unelevated(&["daemon", "status", &id]);
    assert_eq!(
        baseline.status.code(),
        Some(1),
        "an id with no plist in either directory is established absent: stderr:\n{}",
        String::from_utf8_lossy(&baseline.stderr)
    );

    for (denied, probed) in [(staging_parent, staging_path(&id)), (enabled_dir(), enabled_path(&id))] {
        let _mode = ModeGuard::deny_traversal(&denied);

        let output = run_unelevated(&["daemon", "status", &id]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        assert_eq!(
            output.status.code(),
            Some(4),
            "{}: a stat that never completed settles nothing: {stderr}",
            denied.display()
        );
        assert!(
            !stderr.contains("not installed"),
            "{}: no absence was established: {stderr}",
            denied.display()
        );
        assert!(
            stderr.contains(&probed.display().to_string()),
            "{}: the message must name the probe that did not complete: {stderr}",
            denied.display()
        );
    }
}

/// Run `goetia <args>` as the `nobody` account.
///
/// The binary is copied to `/tmp` first, rather than run from
/// `CARGO_BIN_EXE_goetia` in place: on CI the cargo target directory lives
/// under the runner's own home directory, which is not guaranteed
/// traversable by an arbitrary low-privilege account — a failure that would
/// be about the runner's directory layout, not about goetia's own file
/// permissions, which are what these tests need to prove. `/tmp` (the
/// literal path, not `std::env::temp_dir()` — macOS's per-user `$TMPDIR` is
/// `0700`) is world-traversable.
fn run_unelevated(args: &[&str]) -> std::process::Output {
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

    Command::new(&copy_path)
        .args(args)
        .uid(uid)
        .gid(gid)
        .output()
        .expect("spawn goetia as `nobody`")
}

/// [`run_unelevated`] for `daemon list --json`, parsed.
fn unelevated_list_json() -> ListJson {
    let output = run_unelevated(&["daemon", "list", "--json"]);
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

/// `dir`'s mode, restored when this value drops.
///
/// Held by the one test that has to make a plist directory unsearchable to an unprivileged reader.
/// Mode `0700` and a second uid, rather than mode `000` in-process: CI runs this binary as root, and
/// root's DAC override turns a mode-`000` directory into an ordinary `NotFound` — a fixture that
/// would report absence and pass vacuously.
struct ModeGuard {
    dir: PathBuf,
    mode: u32,
}

impl ModeGuard {
    fn deny_traversal(dir: &Path) -> Self {
        let mode = std::fs::metadata(dir)
            .unwrap_or_else(|e| panic!("stat {}: {e}", dir.display()))
            .permissions()
            .mode()
            & 0o7777;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|e| panic!("chmod 0700 {}: {e}", dir.display()));
        ModeGuard {
            dir: dir.to_path_buf(),
            mode,
        }
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(self.mode));
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
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

    let reported_uid = listener.accept_value("the daemon to report its uid");
    assert_eq!(
        reported_uid, "0",
        "a `user: root` daemon must run as uid 0, got {reported_uid}"
    );
}

// Budgets =============================================================================================================

/// `launchctl` exposes no non-blocking `bootout`, so under `Budget::Immediate` `stop` still runs it
/// to completion: issuing the request *is* the wait here, which is why the trait doc comment says
/// so on `stop` rather than leaving it to be rediscovered per platform.
///
/// Against the trap — handing `wait_bounded` the `Immediate` budget's own already-expired deadline
/// — `bootout` is SIGKILLed the instant it is spawned, so this returns a `WaitTimeout` instead of
/// stopping anything, or trips `budget::timed_out`'s `debug_assert!` outright in a debug build.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_with_no_budget_runs_bootout_to_completion() {
    let mgr = LaunchdManager::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

    mgr.stop(&spec.id, Budget::Immediate)
        .expect("bootout has no non-blocking form, so a stop with no budget still completes");

    assert_ne!(
        mgr.status(&spec.id).expect("status after stop").state,
        State::Running,
        "the bootout ran to completion, so the job really is down"
    );
}

/// launchd will not report a crash-looping `restart: always` job running, so a bounded `start`
/// reports [`goetia::Error::WaitTimeout`] — bounded by the budget, not by launchd's respawn
/// throttle. Saying so is the contract (`ServiceManager::start`: `Ok(())` means the manager
/// reports it running), not a defect.
///
/// Two measured facts, neither of them assumed:
///
/// 1. The throttle is ~10.02s and is keyed to the **job**, not to the loaded instance:
///    `bootout` + `bootstrap` does not clear it. Probe run 35044604324 on macos-latest, a
///    `KeepAlive` job running `/usr/bin/true`: `kickstart -p` took 10.01s and 10.02s before a
///    bootout/bootstrap cycle, and 10.03s / 10.02s / 10.02s across three cycles after one. So no
///    prologue remedy is possible, and `start` no longer attempts one.
/// 2. `kickstart -p` is kept regardless. It is the only confirmation launchd offers — there is no
///    notification mechanism, and re-reading state on a timer is polling, which this project
///    forbids — and skipping it would fail a healthy job launchd is merely about to start.
///
/// The budget is **5s, not `DEFAULT`**, and that is deliberate. 5s and the ~10.02s throttle are two
/// orders of magnitude apart from the outcome's perspective, so `WaitTimeout` is not a race in
/// either direction; against `DEFAULT` (10s) they would sit ~20ms apart, which is exactly the bet
/// this project forbids. That closeness is itself the documented consequence for `DEFAULT`: see
/// this backend's module doc comment. No elapsed-time assertion is made or needed — the error
/// variant carries the outcome.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_bounded_start_of_a_crash_looping_job_reports_a_wait_timeout() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    // Exits immediately, so `restart: always` puts launchd into the respawn loop it throttles.
    let mut spec = base_spec(&id, vec!["/usr/bin/true".to_string()]);
    spec.restart = Restart::Always;
    let _guard = Guard::install(&mgr, &spec);

    // One start, not two. `install` leaves the job unloaded, so this bootstraps it — and for a
    // `restart: always` spec whose command exits at once, the bootstrap itself starts the job via
    // `RunAtLoad`, the process dies, and *that* creates the respawn history. The `kickstart -p`
    // that follows therefore already meets a throttled job on this very first start.
    let err = mgr
        .start(&spec.id, Budget::Bounded(Duration::from_secs(5)))
        .expect_err("launchd cannot confirm a crash-looping job running, so a bounded start times out");

    assert!(
        matches!(err, goetia::Error::WaitTimeout { awaited: "running", .. }),
        "an unconfirmed bounded start must be WaitTimeout, not a fabricated success or another error: {err:?}"
    );
}

/// The finding-5 regression. `Budget::Immediate` leaves the job in launchd's `spawn scheduled` /
/// `xpcproxy` window, which D2 classifies as `Unknown` — so a `restart: always` verification block
/// that runs unconditionally fails here with `Error::Other("… did not start …")` or a
/// `WaitTimeout`, for a request that was accepted exactly as asked.
///
/// **No assertion about the resulting state follows** (D7): `Immediate` establishes none, and a
/// block that asserts state cannot run on a budget that established none.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_start_with_no_budget_is_ok_for_a_restart_always_job() {
    let mgr = LaunchdManager::new();
    let id = support::random_test_id();
    let mut spec = sleepy(&id);
    spec.restart = Restart::Always;
    let _guard = Guard::install(&mgr, &spec);

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");

    mgr.start(&spec.id, Budget::Immediate)
        .expect("a start with no budget issues the request and returns Ok");
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
    mgr.start(&spec.id, Budget::DEFAULT)
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
