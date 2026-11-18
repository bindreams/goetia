//! Elevated end-to-end coverage for [`Launchd`] against the real
//! `/Library/Application Support/Goetia/daemons` and `/Library/LaunchDaemons`
//! directories and a real `launchctl`.
//!
//! Every test that mutates anything registers a [`Guard`], which
//! `uninstall`s its id on drop — including on a panicking assertion, since
//! `Drop` still runs during unwinding (the same reasoning
//! `tests/support/service_guard.rs` documents). A few tests seed raw,
//! non-Goetia content directly on disk instead of through the manager (a
//! foreign plist, a hand-edited artifact); those use the narrower
//! [`FileGuard`] (`support::cleanup`) instead, since `uninstall` would refuse to touch them.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use goetia::backend::Identity;
use goetia::backend::launchd::generate;
use goetia::backend::launchd::manager::{ENABLED_DIR, Launchd, STAGING_DIR};
use goetia::decide::Outcome;
use goetia::manager::{self, Budget, Installed, ServiceManager, State};
use goetia::spec::{
    DaemonFields, DaemonSpec, Id, Protocol, Restart, ServiceFields, ServiceSpec, StopAction, StopSignal, User,
};
use goetia::testing::service_fields;

use crate::support::cleanup::{DirGuard, FileGuard, warn_left_behind};
use crate::support::{self, ConnectBack, Group, cmd, launchd_group, may_account_for};

/// Held by every test that reads or seeds hostile state in the shared
/// [`ENABLED_DIR`]/[`STAGING_DIR`], and by every test that runs `daemon list`
/// over them. While `list_does_not_hang_on_a_fifo_in_the_plist_directory` has
/// a FIFO there, a concurrent reader that opens it hangs the suite;
/// serialising makes a FIFO regression fail that one test, which guards its
/// read, instead.
///
/// Named for systemd's unit directory rather than for launchd's plist ones
/// deliberately: skuld coordinates serialization across processes by label
/// *name*, and `tests/cli_binary.rs::native_backend_answers_list_unelevated`
/// runs a real `daemon list` — on macOS, through this backend — holding that
/// same name.
#[skuld::label]
pub(crate) const UNIT_DIR_EXCLUSIVE: skuld::Label;

// Fixtures ============================================================================================================

/// [`support::base_fields`] as a `simple` daemon: every spec this suite installs is one.
pub(crate) fn base_spec(id: &str, command: Vec<String>) -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        protocol: Protocol::Plain,
        ..support::base_fields(id, command)
    })
    .map(DaemonSpec::from)
    .unwrap()
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

pub(crate) fn staging_path(id: &str) -> PathBuf {
    staging_dir().join(format!("{id}.plist"))
}

/// Creates [`STAGING_DIR`] (and `/Library/Application Support/Goetia` above it) if missing, as
/// `install` does on first use, and restates `0755` on the leaf as `ensure_dir` does. Nothing removes
/// the directory, so a test that seeds a file into it calls this first.
fn ensure_staging_dir() {
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).expect("create the staging directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("chmod 0755 {}: {e}", dir.display()));
}

pub(crate) fn enabled_path(id: &str) -> PathBuf {
    enabled_dir().join(format!("{id}.plist"))
}

pub(crate) fn is_loaded(id: &str) -> bool {
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
    let _cleanup = FileGuard::new(source.clone());
    write_bytes(&source, foreign_plist(label).as_bytes());
    cmd::run(
        "plutil",
        &["-convert", "binary1", source.to_str().expect("a UTF-8 temp path")],
    )
    .expect_ok();
    let bytes = std::fs::read(&source).unwrap_or_else(|e| panic!("read {}: {e}", source.display()));
    assert!(
        bytes.starts_with(b"bplist00"),
        "`plutil -convert binary1` must produce a binary plist, got {:?}",
        &bytes[..bytes.len().min(16)]
    );
    bytes
}

/// Bytes that are not UTF-8 and do not begin `bplist00`: still not the UTF-8 XML
/// `generate::plists` writes, so still a positive identification of a plist
/// goetia did not write. `FF FE` is also a UTF-16LE BOM, which is what a legal
/// UTF-16 property list starts with — the exact file that used to hand a macOS
/// host a permanent exit `4` for a vendor's service.
const NON_UTF8_PLIST: [u8; 4] = [b'<', 0xff, 0xfe, b'>'];

fn write_plist(path: &Path, content: &str) {
    write_bytes(path, content.as_bytes());
}

/// Writes `bytes` to `path` and makes it `0644`. The parent must exist, and fails the test if not: a
/// seed into [`STAGING_DIR`] calls [`ensure_staging_dir`] first, and the system's own directories
/// are never created by a test.
fn write_bytes(path: &Path, bytes: &[u8]) {
    support::require_system_dir(path.parent().expect("a plist path has a parent"));
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod plist");
}

// Guards ==============================================================================================================

/// Installs `spec` through `mgr` and `uninstall`s it on drop. Covers both
/// the staging and enabled locations — `uninstall` locates the artifact
/// wherever it currently lives — so a test that calls `enable` doesn't need
/// a second guard.
///
/// After `uninstall`, which refuses what it does not read whole, it boots out the job and a task's
/// timer (`<id>@timer`) and removes both plists from both directories, so that a test that broke
/// halfway leaves nothing a later test could meet.
pub(crate) struct Guard(pub(crate) Id);

impl Guard {
    pub(crate) fn install(mgr: &Launchd, spec: &DaemonSpec) -> Self {
        let guard = Self(spec.common().id.clone());
        mgr.install(spec, false).expect("seed install");
        guard
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        match Launchd::new().uninstall(&self.0) {
            Ok(()) | Err(goetia::Error::NotInstalled { .. }) => {}
            Err(e) => eprintln!("Guard[{}]: cleanup failed: {e}", self.0),
        }
        let id = self.0.as_str();
        // Boots both labels out and removes the pair from `LaunchDaemons`.
        drop(support::ServiceGuard::new(id));
        for path in [staging_path(id), timer_staging_path(id)] {
            support::cleanup::remove_or_warn(&path, false);
        }
    }
}

/// Where a task's timer's plist is while it is not armed.
pub(crate) fn timer_staging_path(id: &str) -> PathBuf {
    staging_dir().join(format!("{id}@timer.plist"))
}

/// Where a task's timer's plist is while it is armed.
pub(crate) fn timer_enabled_path(id: &str) -> PathBuf {
    enabled_dir().join(format!("{id}@timer.plist"))
}

/// Kills the `launchctl kickstart -p system/<id>` that a `start` answering `WaitTimeout` leaves
/// running on purpose when its `print` did not show launchd holding the start, which a long
/// `ThrottleInterval` would keep alive for an hour. Whether the `bootout` in `Guard`'s uninstall, or
/// the wait for the job to be gone that follows it, ends it or waits on it is unmeasured, so this
/// kills it first.
/// Declare it after the [`Guard`], so it drops before the uninstall; launchd keeps the request, and
/// the `bootout` discards it. `id` is the test's random id, so only this test's process can match.
struct KickstartGuard(Id);

impl Drop for KickstartGuard {
    fn drop(&mut self) {
        match Command::new("pkill").args(["-f", &kickstart_pattern(&self.0)]).output() {
            // 1: no process matched.
            Ok(out) if out.status.success() || out.status.code() == Some(1) => {}
            Ok(out) => eprintln!(
                "KickstartGuard[{}]: pkill exited {:?}: {}",
                self.0,
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) => eprintln!("KickstartGuard[{}]: cleanup failed: {e}", self.0),
        }
    }
}

/// The command line of the `launchctl kickstart -p system/<id>` a `start` leaves running. Anchored: only
/// the whole command line matches. The id is hex and dashes, so it needs no escaping.
fn kickstart_pattern(id: &Id) -> String {
    format!("^launchctl kickstart -p system/{id}$")
}

/// Whether that `launchctl kickstart` is running.
fn kickstart_is_running(id: &Id) -> bool {
    let out = Command::new("pgrep")
        .args(["-f", &kickstart_pattern(id)])
        .output()
        .expect("run pgrep");
    match out.status.code() {
        Some(0) => true,
        // No process matched.
        Some(1) => false,
        other => panic!("pgrep exited {other:?}: {}", String::from_utf8_lossy(&out.stderr)),
    }
}

/// The trimmed value on the first `key`-prefixed line of a `launchctl print` body.
pub(crate) fn print_field<'a>(printed: &'a str, key: &str) -> Option<&'a str> {
    printed
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(key))
        .map(str::trim)
}

/// How long a failing `launchctl print` is retried before the test gives up on it. A failure bound
/// surfaced to a human, not a synchronization: `print` can fail transiently while launchd records a
/// run's exit.
const PRINT_FAILURE_BOUND: Duration = Duration::from_secs(5);

/// `launchctl print system/<id>`, retried while it fails; panics with the failure's output on 113
/// (no such job, which is final) or once [`PRINT_FAILURE_BOUND`] has passed. A success is
/// returned whatever it says.
#[track_caller]
pub(crate) fn print_job(id: &str) -> cmd::Run {
    let target = format!("system/{id}");
    let started = std::time::Instant::now();
    loop {
        let printed = cmd::run("launchctl", &["print", &target]);
        if printed.ok() || printed.code == Some(113) || started.elapsed() >= PRINT_FAILURE_BOUND {
            return printed.expect_ok();
        }
        // A pause between attempts, not a synchronization: the loop ends on `print`'s own answer.
        std::thread::sleep(Duration::from_millis(100));
    }
}

// The deliverable: conformance ========================================================================================

/// `UNIT_DIR_EXCLUSIVE`: the run installs, forces and uninstalls a dozen daemons in the shared
/// [`STAGING_DIR`] for its whole length, so it takes turns with the tests that list or seed hostile
/// plists there.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn launchd_passes_conformance(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();

    // Seed `FOREIGN_ID`: non-Goetia content the conformance suite itself
    // never writes to or removes (see `manager::conformance`'s module doc
    // comment) — cleaned up here, not by `run`. The `ServiceGuard` is the backstop for a regressed
    // scenario that enabled or bootstrapped it: it boots the id out and removes it from
    // `LaunchDaemons`.
    let _foreign_service = support::ServiceGuard::new(manager::conformance::FOREIGN_ID);
    let foreign_path = staging_path(manager::conformance::FOREIGN_ID);
    let _foreign_cleanup = FileGuard::new(foreign_path.clone());
    ensure_staging_dir();
    write_plist(&foreign_path, &foreign_plist(manager::conformance::FOREIGN_ID));

    // Seed `HAND_EDITED_ID`: install normally, then hand-edit the result so
    // it no longer matches what regenerating its own embedded spec would
    // produce. Guarded here so a panic before `run` leaves nothing behind;
    // `run` uninstalls it too, which makes the guard's removal a no-op.
    let spec = sleepy(manager::conformance::HAND_EDITED_ID);
    let _hand_edited_cleanup = Guard(spec.common().id.clone());
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
    // why this is not `NON_UTF8_PLIST`. Cleaned up here, like `FOREIGN_ID`, with the same backstop.
    let _undetermined_service = support::ServiceGuard::new(manager::conformance::UNDETERMINED_ID);
    let undetermined_path = staging_path(manager::conformance::UNDETERMINED_ID);
    let _undetermined_cleanup = FileGuard::new(undetermined_path.clone());
    // A leftover from an abrupt stop would make the symlink below fail.
    support::cleanup::remove_or_warn(&undetermined_path, false);
    std::os::unix::fs::symlink(
        format!("{}.plist", manager::conformance::UNDETERMINED_ID),
        &undetermined_path,
    )
    .expect("plant the self-referential symlink");

    manager::conformance::run(&mgr, &sleepy);
}

/// `UNIT_DIR_EXCLUSIVE`: like [`launchd_passes_conformance`], it installs tasks in the shared plist
/// directories for its whole length. Every id it uses is fresh and removed by `run_tasks`.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn launchd_passes_task_conformance(#[fixture(launchd_group)] _group: &Group) {
    manager::conformance::run_tasks(&Launchd::new(), &|id, schedule| {
        goetia::testing::task(id, vec!["/bin/sleep".to_string(), "300".to_string()], schedule)
    });
}

// A run's children and Ctrl-C =========================================================================================

/// Kills a process by pid when dropped, so a test that fails leaves nothing behind. Cleanup never panics.
struct KillOnDrop(libc::pid_t);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: `kill` takes a pid and a signal number; a pid that is gone answers `ESRCH`.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

/// The sentinel, a process group leader: its whole group is killed and the sentinel reaped when this
/// drops, so a test that fails leaves nothing behind. Cleanup never panics.
struct SentinelGroup(std::process::Child);

impl Drop for SentinelGroup {
    fn drop(&mut self) {
        // SAFETY: `killpg` takes a process group id and a signal number; a group that is gone answers `ESRCH`.
        unsafe { libc::killpg(self.0.id() as libc::pid_t, libc::SIGKILL) };
        drop(self.0.wait());
    }
}

/// Ctrl-C is a SIGINT to the terminal's foreground process group, which goetia is in. `run` defers
/// SIGINT only while a launch is held, so the `launchctl` calls it spawns (contained, as
/// `backend::bounded::command` spawns them) must not be in that group: an interrupt that ended one
/// mid-request would lose what goetia sent. The sentinel spawns a child as goetia spawns `launchctl`;
/// the test signals the sentinel's process group, which the sentinel is the leader of, and the child is
/// not in it and lives on.
#[skuld::test]
fn a_launchctl_child_survives_a_sigint_to_goetias_process_group() {
    use std::io::BufRead as _;
    use std::os::unix::process::ExitStatusExt as _;

    let mut goetia = SentinelGroup(
        Command::new(std::env::current_exe().expect("the test binary's path"))
            .arg(support::sentinel::CONTAINED_CHILD)
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn the sentinel"),
    );
    let goetia_group = goetia.0.id() as libc::pid_t;
    let mut line = String::new();
    std::io::BufReader::new(goetia.0.stdout.take().expect("the sentinel's stdout"))
        .read_line(&mut line)
        .expect("the child's pid");
    let child: libc::pid_t = line.trim().parse().expect("a pid");
    let _cleanup = KillOnDrop(child);

    // SAFETY: `getpgid` takes a pid, and every pid here is a process this test spawned.
    let child_group = unsafe { libc::getpgid(child) };
    assert_ne!(
        child_group, goetia_group,
        "the contained child is in goetia's process group"
    );

    // SAFETY: `killpg` takes a process group id and a signal number.
    assert_eq!(
        unsafe { libc::killpg(goetia_group, libc::SIGINT) },
        0,
        "signal goetia's group"
    );
    let status = goetia.0.wait().expect("wait for the sentinel");
    assert_eq!(
        status.signal(),
        Some(libc::SIGINT),
        "the interrupt reached goetia: {status:?}"
    );

    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(unsafe { libc::kill(child, 0) }, 0, "the child outlived the interrupt");
}

// Round trips and drift ===============================================================================================

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_round_trips_without_a_source_file(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    // Nothing outside the installed artifact backs this spec to begin with
    // — no `goetia.yaml` was ever involved — so `list` returning it intact
    // is exactly the "the artifact is the only source of truth" property
    // this proves.
    let listed = mgr.list().expect("list");
    let found = listed
        .into_iter()
        .find(|entry| matches!(entry, Installed::Ours { spec: s, .. } if s.common().id == spec.common().id));
    match found {
        Some(Installed::Ours { spec: got, .. }) => assert_eq!(got, spec),
        other => panic!("expected the installed spec back from list(), got {other:?}"),
    }
}

#[skuld::test]
fn hand_edit_is_detected_as_conflict(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    let path = staging_path(spec.common().id.as_str());
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

/// The plist directories of a default macOS volume are case-insensitive, so a request for `ID`
/// reaches the plist installed as `id`. Neither `install` (forced or not) nor `preview_install`
/// may write over it.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_differently_cased_id_is_refused_and_leaves_the_service_unchanged(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);
    let upper = spec.common().id.as_str().to_uppercase();
    let _upper_guard = Guard(Id::try_from(upper.as_str()).expect("an uppercased valid id is valid"));
    assert!(
        staging_path(&upper).exists(),
        "this test needs a case-insensitive filesystem at {}, and {} is not found there",
        staging_dir().display(),
        staging_path(&upper).display()
    );

    let requested = ServiceSpec::new({
        let base = service_fields(spec.clone());
        ServiceFields {
            common: DaemonFields {
                id: Id::try_from(upper.as_str()).unwrap(),
                ..base.common
            },
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap();
    let expected = Outcome::RefuseOtherDaemon {
        installed: spec.common().id.clone(),
    };
    let before = std::fs::read(staging_path(spec.common().id.as_str())).unwrap();

    assert_eq!(mgr.preview_install(&requested).expect("preview_install"), expected);
    for force in [false, true] {
        assert_eq!(
            mgr.install(&requested, force).expect("install"),
            expected,
            "force={force}"
        );
    }

    assert_eq!(
        std::fs::read(staging_path(spec.common().id.as_str())).unwrap(),
        before,
        "the installed plist must not be written over"
    );
    match mgr.list().expect("list").into_iter().find(
        |entry| matches!(entry, Installed::Ours { spec: s, .. } if s.common().id.as_str().eq_ignore_ascii_case(spec.common().id.as_str())),
    ) {
        Some(Installed::Ours { spec: got, .. }) => assert_eq!(got, spec),
        other => panic!("expected the installed spec back from list(), got {other:?}"),
    }
}

/// The genuine TOCTOU race this guards against — a foreign write landing
/// between `read` and the actual persist — is unit-tested
/// directly against `persist_new` in
/// `src/backend/launchd/manager_tests.rs::persist_new_does_not_clobber_existing_content`,
/// where the target is a plain tempdir path and the timing is therefore
/// deterministic rather than dependent on winning a race with the
/// scheduler. This is the end-to-end complement, through the real
/// `STAGING_DIR`: install must never destroy content that is already
/// sitting where it is about to write.
#[skuld::test]
fn create_does_not_clobber_a_unit_that_appears_after_classification(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    let foreign_content = "not a goetia artifact, seeded directly\n";
    let _cleanup = FileGuard::new(path.clone());
    ensure_staging_dir();
    write_plist(&path, foreign_content);

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

#[skuld::test]
fn install_does_not_start_the_daemon(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    assert!(
        !is_loaded(spec.common().id.as_str()),
        "install must not load the job at all — it must not appear in `launchctl print system`"
    );
}

#[skuld::test]
fn start_does_not_enable(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");

    assert!(
        staging_path(spec.common().id.as_str()).exists(),
        "plist must still be in staging after start"
    );
    assert!(
        !enabled_path(spec.common().id.as_str()).exists(),
        "plist must not have moved into LaunchDaemons after a plain start"
    );
}

#[skuld::test]
fn enable_does_not_start(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    mgr.enable(&spec.common().id).expect("enable");

    assert!(
        enabled_path(spec.common().id.as_str()).exists(),
        "plist must have moved into LaunchDaemons"
    );
    assert!(
        !staging_path(spec.common().id.as_str()).exists(),
        "plist must not remain in staging"
    );
    assert!(
        !is_loaded(spec.common().id.as_str()),
        "enable must not load/start the job"
    );
}

#[skuld::test]
fn disable_returns_the_plist_to_staging(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
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

    mgr.enable(&spec.common().id).expect("enable");
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to report in after start");
    assert_eq!(
        mgr.status(&spec.common().id).unwrap().common().state,
        State::Running,
        "precondition: actually running"
    );

    mgr.disable(&spec.common().id).expect("disable");

    assert!(
        staging_path(spec.common().id.as_str()).exists(),
        "plist must be back in staging"
    );
    assert!(
        !enabled_path(spec.common().id.as_str()).exists(),
        "plist must no longer be in LaunchDaemons"
    );
    // `ServiceManager::disable`'s contract is "does not stop it if
    // running" — a currently-loaded job must survive the plist moving back
    // to staging untouched.
    assert_eq!(
        mgr.status(&spec.common().id).unwrap().common().state,
        State::Running,
        "disable must not stop a job that was loaded"
    );
    mgr.stop(&spec.common().id, Some(Budget::DEFAULT)).expect("stop");
}

// Runtime behavior ====================================================================================================

#[skuld::test]
fn start_stop_status_reflect_reality(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
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

    assert_eq!(mgr.status(&spec.common().id).unwrap().common().state, State::Stopped);

    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to report in after start");
    let running = mgr.status(&spec.common().id).unwrap();
    assert_eq!(running.common().state, State::Running);
    assert!(running.common().pid.is_some(), "a running job should report a pid");

    mgr.stop(&spec.common().id, Some(Budget::DEFAULT)).expect("stop");
    assert_ne!(mgr.status(&spec.common().id).unwrap().common().state, State::Running);
}

// stop-action =========================================================================================================

/// A daemon that ignores `SIGTERM` (an ignored signal stays ignored across `exec`) is ended once its
/// `ExitTimeOut` has passed, and the stop reports stopped: the timeout is the grace, not a wait that
/// fails. The `ExitTimeOut` itself is pinned by the generator's tests; this is what launchd does with it.
/// The daemon reports in after its `trap`, so the stop meets a run that ignores `SIGTERM`: the wait
/// lasts the timeout, and only a kill ends the run after it.
#[skuld::test]
fn a_daemon_that_ignores_the_stop_signal_is_ended_after_the_timeout(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let timeout = Duration::from_secs(2);
    let spec = support::stopping_by(
        sleepy(&id),
        listener.sh_after("trap '' TERM", "exec /bin/sleep 300"),
        StopAction {
            send_signal: StopSignal::Terminate,
            timeout: Some(timeout),
        },
    );
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to be ignoring SIGTERM");

    let stopping = std::time::Instant::now();
    mgr.stop(&spec.common().id, Some(Budget::Bounded(Duration::from_secs(90))))
        .expect("stop");

    assert!(stopping.elapsed() >= timeout, "the run was ended before its timeout");
    assert_ne!(mgr.status(&spec.common().id).unwrap().common().state, State::Running);
}

/// `kill` on a daemon that ignores `SIGTERM` ends it a second after the stop (`ExitTimeOut` 1), well
/// inside a bound shorter than launchd's own 5 s default. The daemon reports in after its `trap`, so the
/// wait lasts that second, and only a kill ends the run after it.
#[skuld::test]
fn a_kill_stop_ends_a_daemon_that_ignores_sigterm(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let spec = support::stopping_by(
        sleepy(&id),
        listener.sh_after("trap '' TERM", "exec /bin/sleep 300"),
        StopAction {
            send_signal: StopSignal::Kill,
            timeout: None,
        },
    );
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to be ignoring SIGTERM");

    let stopping = std::time::Instant::now();
    mgr.stop(&spec.common().id, Some(Budget::Bounded(Duration::from_secs(4))))
        .expect("stop");

    assert!(
        stopping.elapsed() >= Duration::from_secs(1),
        "the run was ended before the one second `kill` gives it"
    );
    assert_ne!(mgr.status(&spec.common().id).unwrap().common().state, State::Running);
}

// kill ================================================================================================================

/// The test binary as a daemon that reports in on every start, under `restart`.
fn reporting(id: &str, restart: Restart, listener: &ConnectBack) -> DaemonSpec {
    let mut spec = service_fields(base_spec(
        id,
        vec![
            support::current_exe_str(),
            support::sentinel::RUN_UNTIL_KILLED.to_string(),
            listener.port().to_string(),
        ],
    ));
    spec.restart = restart;
    ServiceSpec::new(spec).map(DaemonSpec::from).unwrap()
}

#[skuld::test]
fn a_killed_always_restart_daemon_comes_back_with_a_new_pid(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let listener = ConnectBack::listen();
    let spec = reporting(&support::random_test_id(), Restart::Always, &listener);
    let _guard = Guard::install(&mgr, &spec);

    support::kill::a_killed_daemon_comes_back_with_a_new_pid(&mgr, &spec.common().id, &listener);
}

#[skuld::test]
fn a_killed_never_restart_daemon_stays_down(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let listener = ConnectBack::listen();
    let spec = reporting(&support::random_test_id(), Restart::Never, &listener);
    let _guard = Guard::install(&mgr, &spec);

    support::kill::a_killed_daemon_that_never_restarts_stays_down(&mgr, &spec.common().id, &listener, &|| {
        let id = spec.common().id.as_str();
        let plist = [enabled_path(id), staging_path(id)]
            .into_iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_else(|| panic!("no plist of `{id}` to read"));
        assert!(
            !plist.contains("<key>KeepAlive</key>"),
            "the job has a KeepAlive: {plist}"
        );
        let printed = cmd::run("launchctl", &["print", &format!("system/{id}")]);
        assert!(
            !printed.stdout.contains("state = running"),
            "the job is running: {printed}"
        );
    });
}

/// After `kill`, `start` sees the restart the policy made, or makes it now, and returns `Ok`: launchd
/// holds the restart behind its throttle, and `start` is a `kickstart` of the job it holds.
#[skuld::test]
fn a_start_after_a_kill_sees_the_restart(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let listener = ConnectBack::listen();
    let mut spec = service_fields(base_spec(
        &support::random_test_id(),
        vec![
            support::current_exe_str(),
            support::sentinel::RUN_UNTIL_KILLED.to_string(),
            listener.port().to_string(),
        ],
    ));
    spec.restart = Restart::Always;
    spec.restart_delay = Some(Duration::from_secs(5));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    let _guard = Guard::install(&mgr, &spec);

    support::kill::a_start_after_a_kill_sees_the_restart(&mgr, &spec.common().id, &listener);
}

/// Both ways a job has no process: never loaded, and loaded then booted out by `stop`. And the
/// third, which only a job launchd keeps loaded can show: a `restart: never` job that was killed
/// stays loaded with no process, and a second kill finds nothing to signal.
#[skuld::test]
fn killing_a_job_with_no_process_is_not_running(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let listener = ConnectBack::listen();
    let spec = reporting(&support::random_test_id(), Restart::Never, &listener);
    let _guard = Guard::install(&mgr, &spec);
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Launchd::new())) };

    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.common().id);

    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to report in after start");
    mgr.kill(&spec.common().id).expect("kill");
    assert_ne!(
        mgr.status(&spec.common().id).expect("status").common().state,
        State::Running,
        "kill returned while the job was still shown running"
    );
    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.common().id);

    mgr.stop(&spec.common().id, Some(Budget::DEFAULT)).expect("stop");
    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.common().id);
}

/// launchd cannot update a loaded job, so a changed spec over one is refused with the plist and the
/// running daemon untouched, and goes through once the job has been stopped.
#[skuld::test]
fn install_refuses_a_changed_spec_while_the_job_is_loaded(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let installed_spec = base_spec(
        &id,
        vec![
            support::current_exe_str(),
            support::sentinel::RUN_UNTIL_KILLED.to_string(),
            listener.port().to_string(),
        ],
    );
    let _guard = Guard::install(&mgr, &installed_spec);
    mgr.start(&installed_spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the daemon to report in after start");
    let installed = std::fs::read_to_string(staging_path(&id)).unwrap();

    let mut fields = service_fields(installed_spec);
    fields
        .common
        .env
        .insert("GOETIA_TEST_REVISION".to_string(), "2".to_string());
    let spec = ServiceSpec::new(fields).map(DaemonSpec::from).unwrap();
    let refused = mgr
        .install(&spec, false)
        .expect_err("a changed spec over a loaded job must be refused");
    assert!(
        refused.to_string().contains(&format!("goetia daemon stop {id}")),
        "the refusal must give the remedy: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(staging_path(&id)).unwrap(),
        installed,
        "a refused install must leave the plist as it was"
    );
    assert_eq!(
        mgr.status(&spec.common().id).unwrap().common().state,
        State::Running,
        "a refused install must not stop the daemon"
    );

    mgr.stop(&spec.common().id, Some(Budget::DEFAULT)).expect("stop");
    assert!(
        matches!(mgr.install(&spec, false), Ok(Outcome::Update { .. })),
        "the same install must go through once the job is not loaded"
    );
    assert_ne!(std::fs::read_to_string(staging_path(&id)).unwrap(), installed);
    mgr.start(&spec.common().id, Budget::DEFAULT)
        .expect("start the updated daemon");
    listener.accept("the updated daemon to report in");
    mgr.stop(&spec.common().id, Some(Budget::DEFAULT)).expect("stop");
}

#[skuld::test]
fn uninstall_leaves_nothing(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let spec = sleepy(&id);
    // `Guard::drop` re-running `uninstall` after the explicit call below is
    // a harmless, already-gone no-op — see its `Drop` impl — so this still
    // doubles as the panic-safety net every other test gets.
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");

    mgr.uninstall(&spec.common().id).expect("uninstall");

    assert!(!staging_path(&id).exists(), "no plist should remain in staging");
    assert!(!enabled_path(&id).exists(), "no plist should remain in LaunchDaemons");
    assert!(!is_loaded(&id), "job must not still be loaded after uninstall");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_ignores_foreign_plists(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    ensure_staging_dir();
    write_plist(&path, &foreign_plist(&id));

    let listed = mgr.list().expect("list");
    assert!(
        !listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.common().id.as_str() == id)),
        "a foreign plist must not be reported as Ours"
    );
    assert!(
        !listed
            .iter()
            .any(|e| matches!(e, Installed::OursUnusable { name, .. } if name == &id)),
        "a foreign plist carries no marker at all, so it must not even be OursUnusable"
    );
}

/// A plist goetia wrote for a user who has since been deleted carries goetia's marker, so it is
/// goetia's, but its spec can no longer be rendered: it is `OursUnusable` in `list` and an error in
/// `status`.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_reports_a_deleted_user_as_ours_unusable(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let user = "goetia-no-such-user-xyz";
    let mut spec = service_fields(sleepy(&id));
    spec.common.user = User::Name(user.to_string());
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    let path = staging_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    ensure_staging_dir();
    write_plist(
        &path,
        &generate::plists(&spec, &Identity { user: user.to_string() })
            .unwrap()
            .job,
    );
    let cannot_render = mgr
        .preview_install(&spec)
        .expect_err("a spec naming a deleted user cannot be rendered")
        .report()
        .to_string();

    let listed = mgr
        .list()
        .expect("one unrenderable plist must not take down the listing");
    let reason = listed
        .iter()
        .find_map(|entry| match entry {
            Installed::OursUnusable { name, .. } if name == &id => entry.unusable_reason(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a marked plist is ours even when its spec cannot be rendered: {listed:?}"));
    assert_eq!(
        reason,
        format!("marked ours, but its embedded spec cannot be regenerated: {cannot_render}")
    );

    let err = mgr
        .status(&Id::try_from(id.clone()).expect("valid id"))
        .expect_err("a spec that cannot be rendered has no status to report");
    assert_eq!(
        err.report().to_string(),
        format!("`{id}`'s embedded spec cannot be regenerated: {cannot_render}")
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
/// Only our id is asserted: `/Library/LaunchDaemons` holds every vendor's
/// daemons, and one `nobody` cannot open is an unrelated `undetermined` entry.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_works(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);

    // `install` writes an 0644 plist under world-searchable directories, so an unprivileged caller
    // reads it.
    support::list_json(run_unelevated).assert_own(&[spec.common().id.as_str()]);
}

/// The motivating defect, end to end and at the privilege boundary that
/// produces it. A plist the caller cannot open must be named as
/// `undetermined` — never left out of the document, which says it is not
/// there — and must *not* be listed as one of goetia's own, since the marker
/// that would have said so is precisely what went unread.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_reports_a_root_only_plist_as_undetermined(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);
    std::fs::set_permissions(
        staging_path(spec.common().id.as_str()),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("chmod 0600");

    let listed = support::list_json(run_unelevated);

    // Root opens this plist, decodes its marker and lists it under `daemons`, so a run whose reader
    // was not in fact unprivileged fails here instead of passing vacuously.
    let id = Some(spec.common().id.as_str().to_string());
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
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_omits_a_non_utf8_plist_as_foreign(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let readable = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &readable);

    let id = support::random_test_id();
    let path = enabled_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    write_bytes(&path, &NON_UTF8_PLIST);

    let listed = mgr
        .list()
        .expect("one undecodable plist must not take down the whole listing");

    assert!(
        !may_account_for(&listed, &id),
        "bytes that are not goetia's format leave the id neither claimed nor undetermined: \
         {listed:?}"
    );
    support::assert_own(&listed, &[readable.common().id.as_str()]);
}

/// The macOS-wide regression the binary-plist rule prevents. `/Library/
/// LaunchDaemons` is every vendor's directory, and a `bplist00` plist is a
/// fully valid launchd artifact that `plutil`/`defaults` produce by default
/// — so treating "not UTF-8" as undetermined would hand a normal macOS host
/// a standing, unclearable `undetermined` entry and a permanent exit `4` for
/// a service that is in no way goetia's business.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_stays_clean_on_a_host_carrying_binary_plists(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let path = enabled_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    write_bytes(&path, &binary_plist(&id));

    let listed = mgr.list().expect("list");

    assert!(
        !may_account_for(&listed, &id),
        "a binary plist carries no goetia marker by construction, so it is foreign and omitted \
         exactly like an unmarked XML one: {listed:?}"
    );
}

/// The `read` path — the one `install`/`preview_install` reads through, and therefore what
/// `goetia daemon diff` answers from — reaching the same verdict as `status` and `list` about one
/// file. A plist that is not goetia's format is a stranger's artifact at this id, so `install`
/// refuses it with the remedy `Outcome::RefuseForeign` names, rather than erroring.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn preview_install_over_a_non_utf8_plist_refuses_it_as_foreign(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    ensure_staging_dir();
    write_bytes(&path, &NON_UTF8_PLIST);

    let spec = sleepy(&id);

    let preview = mgr
        .preview_install(&spec)
        .expect("bytes that were obtained settle the id, so this is a refusal and not an error");
    assert!(matches!(preview, Outcome::RefuseForeign { .. }), "{preview:?}");

    // Never `NotInstalled`: something is demonstrably there.
    let status = mgr
        .status(&spec.common().id)
        .expect_err("status must reach the same verdict as diff about one file");
    assert!(matches!(status, goetia::Error::Foreign { .. }), "{status:?}");
}

/// `locate`'s two unresolved-probe arms, at the privilege boundary that produces them — the one
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
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn an_unsearchable_plist_directory_is_undetermined_rather_than_absent(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    // The baseline below claims an unprivileged reader can traverse both directories, so the
    // parent's mode is restated too rather than left to the umask.
    ensure_staging_dir();
    let staging_parent = staging_dir()
        .parent()
        .expect("the staging directory is not a filesystem root")
        .to_path_buf();
    std::fs::set_permissions(&staging_parent, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("chmod 0755 {}: {e}", staging_parent.display()));

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
    let _cleanup = FileGuard::new(copy_path.clone());
    std::fs::copy(env!("CARGO_BIN_EXE_goetia"), &copy_path).expect("copy goetia binary to /tmp");
    std::fs::set_permissions(&copy_path, std::fs::Permissions::from_mode(0o755)).expect("chmod copy");

    Command::new(&copy_path)
        .args(args)
        .uid(uid)
        .gid(gid)
        .output()
        .expect("spawn goetia as `nobody`")
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

/// Verifies that launchd's `UserName` honours the account this backend
/// resolves `user:` to (always a real name, never a numeric uid — see
/// `backend::account::resolve_account`'s doc comment): this is the empirical proof that the
/// resolved account actually runs the job, for the most consequential case.
#[skuld::test]
fn root_user_runs_as_uid_zero(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
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
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");

    let reported_uid = listener.accept_value("the daemon to report its uid");
    assert_eq!(
        reported_uid, "0",
        "a `user: root` daemon must run as uid 0, got {reported_uid}"
    );
}

// Budgets =============================================================================================================

/// Under `Budget::Immediate` `stop` still waits until launchd no longer has the job (see the trait
/// doc).
///
/// Against the trap — handing `wait_bounded` the `Immediate` budget's own already-expired deadline
/// — `bootout` is SIGKILLed the instant it is spawned, so this returns a `WaitTimeout` instead of
/// stopping anything, or trips `budget::timed_out`'s `debug_assert!` outright in a debug build.
#[skuld::test]
fn a_stop_under_immediate_waits_for_the_job_to_be_gone(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let spec = sleepy(&support::random_test_id());
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");

    mgr.stop(&spec.common().id, Some(Budget::Immediate))
        .expect("a stop under `Immediate` still waits for the job to be gone, and completes");

    assert_eq!(
        mgr.status(&spec.common().id).expect("status after stop").common().state,
        State::Stopped,
        "launchd no longer has the job, so it reads as stopped"
    );
}

/// `stop(id, None)` waits longer than [`Budget::DEFAULT`]: the job ignores SIGTERM and launchd kills it only
/// after its 12 s of grace, so a stop that waited 10 s would give up with the job still there. That the wait
/// is the stop action's `timeout` plus the teardown margin (22 s) is pinned by the `stop_budget` unit tests.
#[skuld::test]
fn a_stop_that_names_no_budget_waits_for_the_daemons_own_stop(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    let spec = support::stopping_by(
        sleepy(&id),
        listener.sh_after("trap '' TERM", "while :; do /bin/sleep 1; done"),
        StopAction {
            send_signal: StopSignal::Terminate,
            timeout: Some(Duration::from_secs(12)),
        },
    );
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the job to be up with SIGTERM ignored");

    mgr.stop(&spec.common().id, None)
        .expect("the stop waits for launchd's SIGKILL at the end of the 12 s, which a 10 s wait would not");

    assert_eq!(
        mgr.status(&spec.common().id).expect("status after stop").common().state,
        State::Stopped,
        "launchd no longer has the job"
    );
}

/// A `stop` that timed out leaves the job `state = SIGTERMed` and still loaded, and the next `stop`
/// boots it out a second time. What `launchctl bootout` answers a job already going away is
/// unmeasured: the second `stop` is the measurement, and a failure carries the exit code and stderr.
/// The state read just before it is printed, so a pass over a job that was already gone is visible.
///
/// The job ignores SIGTERM, so it outlives the first `stop`'s small budget, and its `ExitTimeOut` is
/// short so that launchd's SIGKILL ends it soon after.
#[skuld::test]
fn a_second_stop_over_a_job_already_going_away_succeeds_once_it_is_gone(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let listener = ConnectBack::listen();
    // `trap '' TERM` first, then the report: once the listener has accepted, SIGTERM is ignored.
    let spec = support::stopping_by(
        sleepy(&id),
        listener.sh_after("trap '' TERM", "while :; do /bin/sleep 1; done"),
        StopAction {
            send_signal: StopSignal::Terminate,
            timeout: Some(Duration::from_secs(5)),
        },
    );
    let _guard = Guard::install(&mgr, &spec);
    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    listener.accept("the job to be up with SIGTERM ignored");

    let first = mgr
        .stop(&spec.common().id, Some(Budget::Bounded(Duration::from_millis(500))))
        .expect_err("a job that ignores SIGTERM outlives the budget");
    assert!(
        matches!(first, goetia::Error::WaitTimeout { awaited: "stopped", .. }),
        "{first:?}"
    );

    let before = cmd::run("launchctl", &["print", &format!("system/{id}")]);
    println!(
        "before the second stop: print exited {:?}, state = {:?}, pid = {:?}",
        before.code,
        print_field(&before.stdout, "state = "),
        print_field(&before.stdout, "pid = ")
    );

    mgr.stop(&spec.common().id, Some(Budget::Unbounded))
        .expect("the second stop returns Ok once the job is gone");
    assert!(!is_loaded(&id), "the job is still loaded after the second stop");
}

/// With `ThrottleInterval` longer than the budget, a bounded `start` of a job with respawn history
/// and no live run reports [`goetia::Error::WaitTimeout`]: launchd cannot confirm the job running
/// within the budget, and saying so is the contract (`ServiceManager::start`: `Ok(())` means the
/// manager reports it running), not a defect.
///
/// `launchctl bootstrap` returns at load, before launchd forks the `RunAtLoad` run, so a `start`
/// straight after it can be the job's first fork: nothing is throttled and any of `Ok`,
/// `WaitTimeout`, `StartUnconfirmed`, `NotStarted` is correct. The test therefore waits for the first
/// run and for its exit before calling `start`.
///
/// Three measured facts:
///
/// 1. The throttle is ~10.02s by default and is keyed to the **job**, not to the loaded instance:
///    `bootout` + `bootstrap` does not clear it. Measured on macos-latest with a
///    `KeepAlive` job running `/usr/bin/true`: `kickstart -p` took 10.01s and 10.02s before a
///    bootout/bootstrap cycle, and 10.03s / 10.02s / 10.02s across three cycles after one. So no
///    prologue remedy is possible, and `start` no longer attempts one.
/// 2. `kickstart -p` is kept regardless. It is the only confirmation launchd offers — there is no
///    notification mechanism, and re-reading state on a timer is polling, which this project
///    forbids — and skipping it would fail a healthy job launchd is merely about to start.
/// 3. `kickstart -p` honours the plist's `ThrottleInterval`: with 3600 this test reported
///    `WaitTimeout` after the full 10s budget on macos-latest (goetia CI run 42176044016).
///
/// The budget is `Budget::DEFAULT`, the path users take. The test's `restart_delay` of an hour,
/// which goetia writes as the plist's `ThrottleInterval`, supplies the margin, not the product's
/// default: whatever the setup costs, the kick cannot be confirmed inside the budget. No
/// elapsed-time assertion is made: the error variant carries the outcome.
#[skuld::test]
fn a_bounded_start_of_a_crash_looping_job_reports_a_wait_timeout(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    // Reports each run and exits at once, so `restart: always` puts launchd into the respawn loop it
    // throttles.
    let listener = ConnectBack::listen();
    let mut spec = service_fields(base_spec(
        &id,
        vec![
            support::current_exe_str(),
            support::sentinel::CONNECT_BACK.to_string(),
            listener.port().to_string(),
        ],
    ));
    spec.restart = Restart::Always;
    spec.restart_delay = Some(Duration::from_secs(3600));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    // Registered before the job is loaded: its `uninstall` boots the job out.
    let _guard = Guard::install(&mgr, &spec);
    // Declared after `_guard`, so the leftover kick is killed before the uninstall's `bootout`.
    let _kickstart = KickstartGuard(spec.common().id.clone());

    // Load the job without `start`: `RunAtLoad` runs it.
    cmd::run(
        "launchctl",
        &["bootstrap", "system", staging_path(&id).to_str().unwrap()],
    )
    .expect_ok();
    listener.accept("the crash-looping job's first run");
    // `accept` returns while the run is still exiting, and `start` reads `Running` from a process
    // with a pid, so wait for a whole `launchctl print` showing the exit and no pid. Not `status`: it
    // reads a failed `print` as `Unknown` with no pid, which would end the wait early.
    support::kill::until(|| {
        let printed = print_job(&id).stdout;
        // A job that has not exited yet reads `last exit code = (never exited)`.
        print_field(&printed, "last exit code = ").is_some_and(|code| !code.contains("never exited"))
            && print_field(&printed, "pid = ").is_none()
    });

    let err = mgr.start(&spec.common().id, Budget::DEFAULT).expect_err(
        "start succeeded on a throttled job: check whether kickstart -p honoured ThrottleInterval (restart_delay)",
    );

    assert!(
        matches!(err.cause(), goetia::Error::WaitTimeout { awaited: "running", .. }),
        "an unconfirmed bounded start must be WaitTimeout; if it is another error, check whether \
         kickstart -p honoured ThrottleInterval (restart_delay): {err:?}"
    );

    // A `kickstart` that is gone was ended, and ending it keeps the start: the job still holds it. One
    // still running asserts nothing: goetia left it on doubt, or its kill is not yet reaped.
    if !kickstart_is_running(&spec.common().id) {
        let printed = print_job(&id).stdout;
        let state = print_field(&printed, "state = ");
        let holds_start = match state {
            Some("spawn scheduled" | "running") => true,
            Some("xpcproxy" | "trampoline") => print_field(&printed, "pid = ").is_some(),
            _ => false,
        };
        assert!(
            holds_start,
            "start ended its kickstart but the job holds no start (state {state:?}):
{printed}"
        );
    }
}

/// `Budget::Immediate` leaves the job in launchd's `spawn scheduled` /
/// `xpcproxy` window, which `state::classify`'s catch-all folds into `Unknown` — so a
/// `restart: always` verification block that runs unconditionally fails here with
/// `Error::NotStarted` or a `WaitTimeout`, for a request that was accepted exactly
/// as asked. This test is the regression guard for exactly that.
///
/// **No assertion about the resulting state follows**: `Immediate` establishes none, and a
/// block that asserts state cannot run on a budget that established none.
#[skuld::test]
fn a_start_with_no_budget_is_ok_for_a_restart_always_job(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let mut spec = service_fields(sleepy(&id));
    spec.restart = Restart::Always;
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    let _guard = Guard::install(&mgr, &spec);

    mgr.stop(&spec.common().id, Some(Budget::DEFAULT)).expect("stop");

    mgr.start(&spec.common().id, Budget::Immediate)
        .expect("a start with no budget issues the request and returns Ok");
}

// Parent directories ==================================================================================================

/// A directory of one test's own under a root goetia makes directories in: `/Library/Application
/// Support` for a `cwd`, `/Library/Logs` for a log. Tests of a missing directory cannot use `/tmp`,
/// where goetia makes none.
fn under_state(id: &str, what: &str) -> PathBuf {
    PathBuf::from("/Library/Application Support").join(format!("{id}-{what}"))
}

fn under_logs(id: &str, what: &str) -> PathBuf {
    PathBuf::from("/Library/Logs").join(format!("{id}-{what}"))
}

fn nobody_ids() -> (u32, u32) {
    let id = |flag: &str| -> u32 {
        cmd::run("id", &[flag, "nobody"])
            .stdout
            .trim()
            .parse()
            .expect("nobody's id")
    };
    (id("-u"), id("-g"))
}

/// `install` makes a missing `cwd` and parent of `logs` under the roots, and hands each deepest
/// directory to the daemon's own account, whose job launchd runs and whose log launchd opens as that
/// account: otherwise a non-root daemon whose `WorkingDirectory` or `StandardOutPath` names a directory
/// only root can write into fails at launch with an opaque status. What is above them stays root's.
#[skuld::test]
fn install_makes_and_hands_over_cwd_and_logs_parent_for_a_non_root_account(#[fixture(launchd_group)] _group: &Group) {
    let (nobody_uid, nobody_gid) = nobody_ids();
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let state = under_state(&id, "state");
    let cwd = state.join("work");
    let logs_parent = under_logs(&id, "logs");
    let _cleanup_cwd = DirGuard::new(state.clone());
    let _cleanup_logs_parent = DirGuard::new(logs_parent.clone());

    let mut spec = service_fields(base_spec(&id, vec!["/bin/true".to_string()]));
    spec.common.user = User::Name("nobody".to_string());
    spec.common.cwd = Some(cwd.clone());
    spec.common.logs = Some(logs_parent.join("out.log"));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    let _guard = Guard::install(&mgr, &spec);

    for (dir, what) in [(cwd.as_path(), "cwd"), (logs_parent.as_path(), "logs' parent")] {
        let meta = std::fs::metadata(dir).unwrap_or_else(|e| panic!("{what} ({}) must exist: {e}", dir.display()));
        assert!(meta.is_dir(), "{what} ({}) must be a directory", dir.display());
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o755,
            "{what} ({}) must be 0755",
            dir.display()
        );
        assert_eq!(
            (meta.uid(), meta.gid()),
            (nobody_uid, nobody_gid),
            "{what} ({}) must be nobody's",
            dir.display()
        );
    }
    assert_eq!(
        std::fs::metadata(&state).expect("stat").uid(),
        0,
        "what is above the cwd stays root's"
    );
}

/// The daemon runs: launchd enters its `cwd` and opens its log as its account, and the daemon writes in
/// the `cwd`. It writes `proof` there as its first act, so the file is the event waited for, and the
/// log has been opened by then.
#[skuld::test]
fn a_daemon_runs_in_the_directories_install_made_for_its_account(#[fixture(launchd_group)] _group: &Group) {
    let (nobody_uid, _) = nobody_ids();
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let state = under_state(&id, "state");
    let logs_parent = under_logs(&id, "logs");
    let _cleanup_cwd = DirGuard::new(state.clone());
    let _cleanup_logs_parent = DirGuard::new(logs_parent.clone());
    let mut spec = service_fields(base_spec(
        &id,
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo started > proof; exec /bin/sleep 300".to_string(),
        ],
    ));
    spec.common.user = User::Name("nobody".to_string());
    spec.common.cwd = Some(state.join("work"));
    spec.common.logs = Some(logs_parent.join("out.log"));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    let _guard = Guard::install(&mgr, &spec);

    mgr.start(&spec.common().id, Budget::DEFAULT).expect("start");
    support::kill::until(|| state.join("work/proof").is_file());

    let log = std::fs::metadata(logs_parent.join("out.log")).expect("launchd opened the log");
    assert_eq!(log.uid(), nobody_uid, "launchd opens the log as the job's account");
}

#[skuld::test]
fn install_refuses_a_missing_directory_outside_the_roots_and_makes_nothing(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let base = Path::new("/tmp").join(format!("{id}-outside"));
    let _cleanup = DirGuard::new(base.clone());
    let mut spec = service_fields(base_spec(&id, vec!["/bin/true".to_string()]));
    spec.common.cwd = Some(base.join("work"));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();
    let _guard = Guard(spec.common().id.clone());

    let err = Launchd::new().install(&spec, false).expect_err("outside every root");

    let goetia::Error::DirectoryRefused { why, .. } = &err else {
        panic!("expected a refused directory, got {err:?}");
    };
    assert!(
        why.contains("create it first") && why.contains("/Library/Logs"),
        "{why}"
    );
    assert!(!base.exists(), "nothing was made outside the roots");
    assert!(!staging_path(&id).exists(), "a refused install writes no plist");
}

/// A spec whose `cwd` is `cwd` and whose `logs` parent lies under a regular file, so the install fails
/// on the latter while judging it, before it makes what is missing of the former.
fn spec_failing_on_logs_after_cwd(id: &str, base: &Path, cwd: &Path) -> DaemonSpec {
    let file = base.join("file");
    std::fs::write(&file, b"x").unwrap_or_else(|e| panic!("write {}: {e}", file.display()));
    let mut spec = service_fields(base_spec(id, vec!["/bin/true".to_string()]));
    spec.common.cwd = Some(cwd.to_path_buf());
    spec.common.logs = Some(file.join("sub").join("out.log"));
    ServiceSpec::new(spec).map(DaemonSpec::from).unwrap()
}

#[skuld::test]
fn an_install_refused_on_its_logs_makes_none_of_the_directories_it_would_have_made(
    #[fixture(launchd_group)] _group: &Group,
) {
    let id = support::random_test_id();
    let base = under_state(&id, "undo-made");
    let _cleanup = DirGuard::new(base.clone());
    std::fs::create_dir(&base).unwrap_or_else(|e| panic!("mkdir {}: {e}", base.display()));
    let spec = spec_failing_on_logs_after_cwd(&id, &base, &base.join("a").join("b"));
    let _guard = Guard(spec.common().id.clone());

    let err = Launchd::new()
        .install(&spec, false)
        .expect_err("the logs parent is under a file");
    support::assert_failed_on_a_path_under_a_file(&err);

    assert!(!base.join("a").exists(), "the refused install made its cwd");
    assert!(base.is_dir(), "a directory that existed before must stay");
}

#[skuld::test]
fn an_install_refused_on_its_logs_makes_no_new_directory_in_one_that_existed_before(
    #[fixture(launchd_group)] _group: &Group,
) {
    let id = support::random_test_id();
    let base = under_state(&id, "undo-preexisting");
    let _cleanup = DirGuard::new(base.clone());
    let existing = base.join("cwd");
    std::fs::create_dir_all(&existing).unwrap_or_else(|e| panic!("mkdir {}: {e}", existing.display()));
    let made = existing.join("new");
    let spec = spec_failing_on_logs_after_cwd(&id, &base, &made);
    let _guard = Guard(spec.common().id.clone());

    let err = Launchd::new()
        .install(&spec, false)
        .expect_err("the logs parent is under a file");
    support::assert_failed_on_a_path_under_a_file(&err);

    assert!(!made.exists(), "the refused install made {}", made.display());
    assert!(existing.is_dir(), "a directory that existed before must stay");
}

/// Makes a file immutable (`uchg`), so a plist cannot be replaced.
struct Immutable(PathBuf);

impl Immutable {
    fn new(path: PathBuf) -> Self {
        cmd::run("chflags", &["uchg", path.to_str().unwrap()]).expect_ok();
        Self(path)
    }
}

impl Drop for Immutable {
    fn drop(&mut self) {
        warn_left_behind(cmd::run_cleanup(
            "chflags",
            &[&["nouchg", self.0.to_str().unwrap()]],
            &[],
        ));
    }
}

/// The plist cannot be replaced, after the directories for the new `cwd` and `logs` were made: none of
/// them is left, `logs_root` (made directly under `/Library/Logs`, which the `admin` group can write)
/// included. This is the case the undo is for: the write fails with the old plist standing, which
/// names none of the new directories.
#[skuld::test]
fn an_install_whose_plist_write_fails_removes_every_directory_it_made(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let spec = base_spec(&id, vec!["/bin/true".to_string()]);
    let _guard = Guard::install(&mgr, &spec);
    let old = std::fs::read_to_string(staging_path(&id)).expect("read the plist");
    // Declared after the guard, so it drops first: the guard's cleanup needs the plist removable.
    let _immutable = Immutable::new(staging_path(&id));
    let cwd = under_state(&id, "write-fails");
    let logs_root = under_logs(&id, "write-fails");
    let logs_parent = logs_root.join("logs");
    let logs = logs_parent.join("out.log");
    let _cleanup_cwd = DirGuard::new(cwd.clone());
    let _cleanup_logs_root = DirGuard::new(logs_root.clone());
    let mut fields = service_fields(spec);
    fields.common.cwd = Some(cwd.clone());
    fields.common.logs = Some(logs.clone());
    let updated = ServiceSpec::new(fields).map(DaemonSpec::from).unwrap();

    let err = mgr.install(&updated, false).expect_err("the plist cannot be replaced");

    assert!(
        err.to_string().contains("replace"),
        "the install must fail on the plist, not earlier: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(staging_path(&id)).expect("read the plist"),
        old,
        "the write must not have happened"
    );
    assert!(!cwd.exists(), "the failed install left {}", cwd.display());
    for made in [&logs_parent, &logs_root] {
        assert!(!made.exists(), "the failed install left {}", made.display());
    }
}

/// An update that writes a plist asking for a new `cwd` makes it and keeps it: the directories are kept
/// because the plist the install wrote stands where `read` found it.
#[skuld::test]
fn an_update_keeps_the_cwd_it_made(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let spec = base_spec(&id, vec!["/bin/true".to_string()]);
    let _guard = Guard::install(&mgr, &spec);
    let cwd = under_state(&id, "update-cwd");
    let _cleanup = DirGuard::new(cwd.clone());
    let mut fields = service_fields(spec);
    fields.common.cwd = Some(cwd.clone());
    let updated = ServiceSpec::new(fields).map(DaemonSpec::from).unwrap();

    let outcome = mgr.install(&updated, false).expect("update");

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(cwd.is_dir(), "the update's cwd must stay");
}

// Non-regular occupants ===============================================================================================

/// Regression coverage for the `locate`/`persist_new` classification
/// mismatch: `Path::is_file` (what `locate` used to check) is `false` for
/// a directory, while `link`(2) (what `persist_noclobber` uses) refuses to
/// create over one exactly as it would over a regular file. That mismatch
/// made a directory sitting at a fresh id's staging path classify as
/// `Absent` -> `Create` -> `persist_new` -> `Raced` -> re-`install` -> the
/// identical classification, an unbounded recursion that would eventually
/// abort the process. `locate` now uses `symlink_metadata`, so a directory
/// is "occupied" like anything else, and `read` classifies it
/// before opening it.
///
/// The refusal is `RefuseForeign`, not an `Err`: a directory at a plist path
/// is not something goetia failed to read, it is positively not a plist, and
/// naming the remedy beats a bare error. That is the same verdict the
/// systemd backend reaches for a non-regular file at a fragment path.
#[skuld::test]
fn install_over_a_directory_refuses_instead_of_recursing(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();
    let path = staging_path(&id);
    let _cleanup = DirGuard::new(path.clone());
    ensure_staging_dir();
    std::fs::create_dir(&path).expect("seed a directory at the staging path");

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
/// `UNIT_DIR_EXCLUSIVE`: the FIFO sits where a concurrent unprivileged
/// `daemon list` reads, and a regression would hang that reader, not only
/// this test's guarded one.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_does_not_hang_on_a_fifo_in_the_plist_directory(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let path = enabled_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    support::require_system_dir(path.parent().unwrap());
    support::tasks::mkfifo(&path);

    let listed = without_blocking("Launchd::list", || Launchd::new().list()).expect("list");

    assert!(
        !may_account_for(&listed, &id),
        "a FIFO is not a plist, and nothing about it was left undetermined: {listed:?}"
    );
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
#[skuld::test]
fn start_is_not_fooled_by_a_stale_job_holding_the_label(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    let id = support::random_test_id();

    // Ours: `restart: always`, so launchd guarantees it runs once loaded.
    let mut spec = service_fields(sleepy(&id));
    spec.restart = Restart::Always;
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();

    // The decoy takes the label *before* install, which is the real
    // sequence: a predecessor job is still loaded when Goetia installs over
    // the same id.
    let decoy_dir = std::env::temp_dir().join(format!("{id}-decoy"));
    let _decoy_cleanup = DirGuard::new(decoy_dir.clone());
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
    // Boots out whichever job holds the label, the decoy included, after `_guard` and before
    // `_decoy_cleanup` removes its plist.
    let _decoy_job = support::ServiceGuard::new(&id);
    cmd::run("launchctl", &["bootstrap", "system", decoy.to_str().unwrap()]);

    let _guard = Guard::install(&mgr, &spec);

    // The decoy now holds the label and is running, which is exactly the
    // state that used to make `start` a no-op.
    mgr.start(&spec.common().id, Budget::DEFAULT)
        .expect("start must recover the label, not report a false success");

    let status = mgr.status(&spec.common().id).expect("status");
    assert_eq!(
        status.common().state,
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
}
