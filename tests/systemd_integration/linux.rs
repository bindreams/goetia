//! The actual test bodies — see `tests/systemd_integration.rs` for why this lives in its own file
//! rather than an inline `mod linux { ... }`: an inline module has no real backing directory, so a
//! `#[path]`-relative import from inside it (to reach the shared `tests/support/mod.rs`) cannot
//! `../`-escape it — POSIX path resolution needs every intermediate component, including `linux`
//! itself, to exist as a real directory entry, not just resolve lexically. A real file avoids the
//! problem entirely, matching `tests/marker_inertness.rs`'s own structure.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use goetia::backend::Identity;
use goetia::backend::systemd::generate;
use goetia::backend::systemd::manager::Systemd;
use goetia::cli::ExitCode;
use goetia::decide::Outcome;
use goetia::manager::{Budget, Installed, ServiceManager, State, conformance};
use goetia::spec::{AccountId, DaemonFields, DaemonSpec, Id, Kind, Restart, User};

use crate::support::cleanup::{DirGuard, FileGuard, remove_or_warn, warn_left_behind};
use crate::support::{self, ELEVATED, ServiceGuard, cmd, may_account_for};

/// Held by every test that reads or seeds hostile state in the shared `/etc/systemd/system`, and by
/// every test that runs `daemon list` over it. While `list_does_not_hang_on_a_fifo_in_the_unit_directory`
/// has a FIFO there, a concurrent reader that opens it hangs the suite; serialising makes a FIFO
/// regression fail that one test, which guards its read, instead.
#[skuld::label]
pub(crate) const UNIT_DIR_EXCLUSIVE: skuld::Label;

// Fixtures ============================================================================================================

/// A minimal, real, long-running daemon: `sleep infinity` exists on every coreutils Ubuntu ships.
pub(crate) fn mk(id: &str) -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        restart: Restart::OnFailure,
        kind: Kind::Simple,
        ..support::plain_fields(id, vec!["/bin/sleep".to_string(), "infinity".to_string()])
    })
    .unwrap()
}

/// Bytes goetia never writes: `generate::unit` emits UTF-8 ini and nothing else, so a fragment that
/// will not decode is positively not goetia's — foreign, and omitted from `list` exactly like an
/// unmarked one. The systemd twin of `tests/launchd_integration/launchd.rs`'s `NON_UTF8_PLIST`,
/// and the reason those two backends now answer one question one way.
const NON_UTF8_UNIT: [u8; 4] = [b'[', 0xff, 0xfe, b']'];

pub(crate) fn unit_path(id: &str) -> PathBuf {
    PathBuf::from(support::SYSTEMD_UNIT_DIR).join(format!("{id}.service"))
}

fn dropin_dir(id: &str) -> PathBuf {
    PathBuf::from(support::SYSTEMD_UNIT_DIR).join(format!("{id}.service.d"))
}

pub(crate) fn wants_symlink(id: &str) -> PathBuf {
    PathBuf::from(support::SYSTEMD_UNIT_DIR)
        .join("multi-user.target.wants")
        .join(format!("{id}.service"))
}

/// A hand-written unit carrying no `[X-Goetia]` marker at all — what a real foreign service looks
/// like to discovery.
fn seed_foreign(id: &str) {
    let text = format!(
        "[Unit]\nDescription=not goetia ({id})\n\n[Service]\nType=oneshot\nExecStart=/bin/true\n\
         RemainAfterExit=yes\n"
    );
    fs::write(unit_path(id), text).unwrap_or_else(|e| panic!("seed foreign unit {id}: {e}"));
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
}

/// Hand-edit an already-installed unit the way `systemctl edit --full` would: add a directive the
/// spec cannot express (`MemoryMax=8G`, the design spec's own example), without touching the
/// `[X-Goetia]` section.
fn hand_edit(id: &str) {
    let path = unit_path(id);
    let text = fs::read_to_string(&path).expect("read installed unit");
    let edited = text.replacen("[Service]\n", "[Service]\nMemoryMax=8G\n", 1);
    assert_ne!(text, edited, "expected a [Service] header to hand-edit");
    fs::write(&path, edited).expect("write hand-edited unit");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
}

/// The way `systemctl edit` creates a drop-in: `<id>.service.d/override.conf`. `systemd.unit(5)`
/// only reads `*.conf` files there, which `write_dropin` exercises by using that exact extension.
fn write_dropin(id: &str) {
    let dir = dropin_dir(id);
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    fs::write(dir.join("override.conf"), "[Service]\nMemoryMax=8G\n").expect("write drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
}

/// The same drop-in, in a directory no unprivileged process can open. Mode `000` on a directory the
/// (root) test process owns is enough: `EACCES` applies to every uid but root's, and root is exactly
/// the uid the reader in these tests is not.
fn write_unreadable_dropin(id: &str) {
    write_dropin(id);
    fs::set_permissions(dropin_dir(id), fs::Permissions::from_mode(0o000)).expect("chmod 0000");
}

/// A regular file where `<id>.service.d` belongs. `read_dir` on it answers `ENOTDIR`, which no
/// privilege dissolves — `CAP_DAC_OVERRIDE` overrides permission bits, not the fact that a file is
/// not a directory — so this is the one artifact an *elevated* suite can leave unclassifiable, and
/// the seed both this file's own drop-in tests and `systemd_passes_conformance` use.
///
/// Its own removal, not `ServiceGuard`'s: that cleans a drop-in with `remove_dir_all`, which cannot
/// remove a file.
fn seed_unreadable_dropin_enotdir(id: &str) -> FileGuard {
    let path = dropin_dir(id);
    let guard = FileGuard::new(path.clone());
    fs::write(&path, b"").unwrap_or_else(|e| panic!("seed {}: {e}", path.display()));
    guard
}

/// [`seed_unreadable_dropin_enotdir`] under an arbitrary search root, creating that root where it is
/// missing (its parent must exist: see [`create_root`]). `scan_host` reaches any root but `UNIT_DIR`
/// through its cross-root pass alone, and only for the drop-in half — a *fragment* outside
/// `UNIT_DIR` deliberately names no id (see `HostScan`).
///
/// The root is removed only when this call created it, and with `remove_dir`, so a root systemd
/// itself populated is left exactly as it was found.
fn seed_unreadable_dropin_in_root(root: &str, id: &str) -> (PathBuf, RmInRoot<FileGuard>) {
    let root_path = PathBuf::from(root);
    let leaf = root_path.join(format!("{id}.service.d"));
    let guard = RmInRoot {
        _leaf: FileGuard::new(leaf.clone()),
        _root: create_root(&root_path),
    };
    fs::write(&leaf, b"").unwrap_or_else(|e| panic!("seed {}: {e}", leaf.display()));
    (leaf, guard)
}

/// Seed one `*.conf` into the drop-in directory `dir`, creating only that leaf, and hand back the RAII
/// removal of it. Its parent search root is guaranteed or was made by [`create_root`], and is left
/// exactly as found: `/etc/systemd/system` is shared with every other test running concurrently.
fn seed_dropin(dir: &Path, root: CreatedRoot) -> RmInRoot<DirGuard> {
    let guard = RmInRoot {
        _leaf: DirGuard::new(dir.to_path_buf()),
        _root: root,
    };
    fs::create_dir(dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    fs::write(dir.join("50-x.conf"), "[Service]\nMemoryMax=8G\n").expect("write drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
    guard
}

/// The same, under one of the two `.control` roots — where `systemctl set-property UNIT
/// PROPERTY=VALUE` writes, and the two highest-precedence entries on the system unit search path,
/// both above `/etc/systemd/system`.
///
/// The root is removed along with the drop-in only where this call created it. Each test that seeds
/// one takes a *different* root, so no concurrently running test can be inside the one this removes
/// — and the removal is `remove_dir`, so a root systemd itself populated for some other unit fails
/// to go and is left as it was found.
fn seed_control_dropin(root: &str, id: &str) -> (PathBuf, RmInRoot<DirGuard>) {
    let root_path = PathBuf::from(root);
    let leaf = root_path.join(format!("{id}.service.d"));
    let guard = seed_dropin(&leaf, create_root(&root_path));
    (leaf, guard)
}

/// A manifest at a path an unprivileged process can actually reach: `tempfile`'s own directory is
/// 0700, which `runuser -u nobody` cannot traverse.
pub(crate) fn world_readable_manifest(id: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod the tempdir 0755");
    let path = dir.path().join("goetia.yaml");
    fs::write(&path, format!("id: {id}\ncommand: [\"/bin/sleep\", \"infinity\"]\n")).expect("write manifest");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod the manifest 0644");
    (dir, path)
}

fn load_from_temp_manifest(id: &str) -> DaemonSpec {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("goetia.yaml");
    fs::write(&path, format!("id: {id}\ncommand: [\"/bin/sleep\", \"infinity\"]\n")).expect("write manifest");
    let (spec, _warnings) = goetia::spec::load(&path).expect("load manifest");
    // `dir` (and the manifest inside it) is dropped here, before the caller ever installs the
    // spec - the whole point of `install_then_show_round_trips_without_the_source_file`.
    spec
}

fn find_ours(installed: Vec<Installed>, id: &str) -> Option<DaemonSpec> {
    installed.into_iter().find_map(|entry| match entry {
        Installed::Ours { spec, .. } if spec.id.as_str() == id => Some(spec),
        _ => None,
    })
}

fn mkfifo(path: &Path) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a unit path with no NUL");
    // SAFETY: `c_path` is a NUL-terminated pointer valid for the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

/// Run `f` on its own thread and report — rather than hang — if it never returns.
///
/// The bound is a failure bound on a kernel wait that may genuinely never end: `open(2)` on a FIFO
/// with `O_RDONLY` blocks until a writer arrives, and nothing here ever creates one. It synchronizes
/// nothing, and no passing run's outcome depends on its value — only how long a regression takes to
/// be *reported* instead of wedging the whole suite, which is what an unguarded call would do.
fn without_blocking<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || drop(tx.send(f())));
    rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap_or_else(|_| {
        panic!("{what} never returned: it opened the FIFO for reading, which blocks until a writer arrives")
    })
}

/// The numeric uid of a real local account, looked up rather than hardcoded — `nobody`'s uid is
/// traditionally 65534 but is not guaranteed.
fn uid_of(account: &str) -> u32 {
    let run = cmd::run("id", &["-u", account]).expect_ok();
    run.stdout
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse uid of {account}: {e}"))
}

/// Creates the search root `root`, which must be the only thing missing: its parent is a system
/// directory (`/etc/systemd`, `/run/systemd`), shared by every test that runs concurrently, so no
/// test creates it or removes it. `create_dir` answers `AlreadyExists` for
/// a root that was there first, so ownership is not a check followed by a create.
fn create_root(root: &Path) -> CreatedRoot {
    support::require_system_dir(root.parent().expect("a search root has a parent"));
    match fs::create_dir(root) {
        Ok(()) => CreatedRoot(Some(root.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => CreatedRoot(None),
        Err(e) => panic!("mkdir {}: {e}", root.display()),
    }
}

/// The search root a seeding call created, removed on drop with `remove_dir`. "Not empty" is
/// expected, not a failure: a directory systemd itself populated for some other unit is left as it
/// was found.
#[derive(Default)]
struct CreatedRoot(Option<PathBuf>);

impl Drop for CreatedRoot {
    fn drop(&mut self) {
        let Some(dir) = &self.0 else { return };
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(e) => eprintln!("warning: left behind: remove {}: {e}", dir.display()),
        }
    }
}

/// RAII removal of a seeded leaf (a `DirGuard` for a drop-in directory, a `FileGuard` for a file
/// standing where one belongs) and, where the seeding call created it, the search root holding it.
/// The leaf field is declared first, so it is removed before the root; see [`seed_control_dropin`].
///
/// No `daemon-reload` on drop. A test that seeds and never reloads needs no `ServiceGuard`, because
/// systemd holds nothing from the seed; one that reloads holds a `ServiceGuard`, whose cleanup
/// reloads after this runs.
struct RmInRoot<L> {
    _leaf: L,
    _root: CreatedRoot,
}

/// Drive `daemon uninstall` through the CLI rather than calling `Systemd::uninstall` directly: the
/// thing under test is the exit code a shell sees, and `IdVerbCall::absent_is_success` turns
/// `Error::NotInstalled` — and only that variant — into `0` and "nothing to do" on the way there.
fn uninstall_via_cli(id: &str) -> (ExitCode, String, String) {
    let args = goetia::cli::uninstall::Args {
        ids: vec![id.to_string()],
    };
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Systemd::new())) };
    let mut out = Vec::new();
    let mut err = Vec::new();
    // Truthful, not a stub: `support::elevated` is this test's own precondition.
    let code = goetia::cli::uninstall::run(&args, &get_manager, &|| true, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("stdout is UTF-8"),
        String::from_utf8(err).expect("stderr is UTF-8"),
    )
}

// Step 1: conformance =================================================================================================

/// `UNIT_DIR_EXCLUSIVE`: the run installs, forces and uninstalls a dozen units in the shared
/// `/etc/systemd/system` for its whole length, so it takes turns with the tests that list or seed
/// hostile units there.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn systemd_passes_conformance() {
    let mgr = Systemd::new();

    // The three ids `conformance::run` cannot produce through the trait's own methods - see its
    // module doc comment. All three are guarded here before their seed is written; `run` also
    // removes `HAND_EDITED_ID`, which makes its guard's removal a no-op.
    let foreign_guard = ServiceGuard::new(conformance::FOREIGN_ID);
    seed_foreign(foreign_guard.id());

    let _hand_edited_guard = ServiceGuard::new(conformance::HAND_EDITED_ID);
    mgr.install(&mk(conformance::HAND_EDITED_ID), false)
        .expect("seed hand-edited install");
    hand_edit(conformance::HAND_EDITED_ID);

    // Seed `UNDETERMINED_ID`: `ENOTDIR`, the one unreadable artifact elevation does not dissolve —
    // see the scenario's own doc comment. `run` never writes to or removes this id, so both guards
    // are ours: the `ServiceGuard` covers a backend that writes anyway, which is the regression the
    // scenario exists to catch, and it is declared first so the seed's own removal runs before its
    // `remove_dir_all` reaches the same name.
    let _undetermined_guard = ServiceGuard::new(conformance::UNDETERMINED_ID);
    let _undetermined_seed = seed_unreadable_dropin_enotdir(conformance::UNDETERMINED_ID);

    conformance::run(&mgr, &mk);
}

// Step 2: scenarios for each correctness property =====================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_then_show_round_trips_without_the_source_file() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = load_from_temp_manifest(guard.id());

    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let found = find_ours(mgr.list().expect("list"), guard.id());
    assert_eq!(found, Some(spec));
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn hand_edit_is_detected_as_conflict() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    hand_edit(guard.id());

    let outcome = mgr.install(&spec, false).expect("install over a hand edit");
    match outcome {
        Outcome::Conflict {
            artifact_diff,
            unclearable_recovery,
        } => {
            assert!(artifact_diff.contains("MemoryMax=8G"), "{artifact_diff}");
            // The whole cause is the fragment goetia itself writes, so `--force` — which the CLI
            // offers for exactly this `None` — really does resolve it, as the forced install below
            // then demonstrates.
            assert_eq!(unclearable_recovery, None);
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    let forced = mgr.install(&spec, true).expect("forced install");
    assert!(matches!(forced, Outcome::Update { .. }), "{forced:?}");
    let on_disk = fs::read_to_string(unit_path(guard.id())).expect("read back");
    assert!(
        !on_disk.contains("MemoryMax"),
        "force must overwrite the hand-edit: {on_disk}"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn drop_in_override_is_detected_as_drift() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    write_dropin(guard.id());

    // The fragment itself is untouched - only the drop-in exists - so a naive comparison over
    // the fragment alone would say `UpToDate`. It must not.
    let outcome = mgr.install(&spec, false).expect("install with a drop-in present");
    assert!(matches!(outcome, Outcome::Conflict { .. }), "{outcome:?}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn forced_install_over_a_dropin_actually_clears_it() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    write_dropin(guard.id());

    let forced = mgr.install(&spec, true).expect("forced install over a drop-in");
    assert!(matches!(forced, Outcome::Update { .. }), "{forced:?}");
    assert!(
        !dropin_dir(guard.id()).exists(),
        "force must actually remove the drop-in directory, not just overwrite the fragment"
    );

    // The drift is really gone, not merely papered over: installing the same spec again, still
    // without force, must now be a clean no-op — mirroring
    // `manager::conformance::conflict_requires_force`'s own assertion for a hand-edited fragment.
    let after = mgr.install(&spec, false).expect("install after force");
    assert!(matches!(after, Outcome::UpToDate), "{after:?}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_a_stray_dropin_with_no_fragment() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_dropin(guard.id()); // no unit file installed at all

    let mgr = Systemd::new();
    let outcome = mgr
        .install(&mk(guard.id()), false)
        .expect("install over a stray drop-in");
    assert!(
        matches!(outcome, Outcome::RefuseForeign { .. }),
        "a drop-in with no fragment must not be silently adopted as Create, got {outcome:?}"
    );
    assert!(!unit_path(guard.id()).exists(), "no fragment must be written");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_over_a_masked_unit() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let path = unit_path(guard.id());
    std::os::unix::fs::symlink("/dev/null", &path).expect("mask (symlink to /dev/null)");

    let mgr = Systemd::new();
    let outcome = mgr.install(&mk(guard.id()), false).expect("install over a masked unit");
    assert!(matches!(outcome, Outcome::RefuseForeign { .. }), "{outcome:?}");

    let meta = fs::symlink_metadata(&path).expect("stat");
    assert!(
        meta.file_type().is_symlink(),
        "masked unit must survive install untouched"
    );
    assert_eq!(fs::read_link(&path).expect("read link"), Path::new("/dev/null"));
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn start_stop_status_reflect_reality() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let status = mgr.status(&spec.id).expect("status after start");
    assert_eq!(status.state, State::Running, "{status:?}");
    assert!(status.pid.is_some(), "{status:?}");

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    let status = mgr.status(&spec.id).expect("status after stop");
    assert_ne!(status.state, State::Running, "{status:?}");
    assert!(status.pid.is_none(), "{status:?}");
}

// kill ================================================================================================================

/// The test binary as a daemon that reports in on every start, under `restart`.
fn reporting(id: &str, restart: Restart, listener: &support::ConnectBack) -> DaemonSpec {
    let mut spec = mk(id).into_fields();
    spec.command = vec![
        support::current_exe_str(),
        support::sentinel::RUN_UNTIL_KILLED.to_string(),
        listener.port().to_string(),
    ];
    spec.restart = restart;
    DaemonSpec::new(spec).unwrap()
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_killed_always_restart_daemon_comes_back_with_a_new_pid() {
    let guard = ServiceGuard::new(support::random_test_id());
    let listener = support::ConnectBack::listen();
    let spec = reporting(guard.id(), Restart::Always, &listener);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    support::kill::a_killed_daemon_comes_back_with_a_new_pid(&mgr, &spec.id, &listener);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_killed_never_restart_daemon_stays_down() {
    let guard = ServiceGuard::new(support::random_test_id());
    let listener = support::ConnectBack::listen();
    let spec = reporting(guard.id(), Restart::Never, &listener);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let unit = format!("{}.service", spec.id);
    support::kill::a_killed_daemon_that_never_restarts_stays_down(&mgr, &spec.id, &listener, &|| {
        let shown = cmd::run(
            "systemctl",
            &["show", "--property=Restart,NRestarts,SubState", "--", &unit],
        )
        .expect_ok();
        for expected in ["Restart=no", "NRestarts=0"] {
            assert!(
                shown.stdout.lines().any(|line| line == expected),
                "no `{expected}` in {shown}"
            );
        }
        assert!(
            !shown.stdout.lines().any(|line| line == "SubState=auto-restart"),
            "the unit is waiting to restart: {shown}"
        );
    });
}

/// An installed unit that was never started is not loaded; one that was stopped is loaded or not
/// depending on what still refers to it. Both are `NotRunning`, off `systemctl kill`'s own answer.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn killing_a_stopped_unit_is_not_running() {
    let guard = ServiceGuard::new(support::random_test_id());
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Systemd::new())) };

    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.id);

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.id);
}

/// `ActiveState` and `Job` as `systemctl show` reports them: the job is empty when none is queued.
pub(crate) fn active_state_and_job(id: &str) -> (String, String) {
    let unit = format!("{id}.service");
    let run = cmd::run(
        "systemctl",
        &["show", "--property=ActiveState", "--property=Job", &unit],
    )
    .expect_ok();
    let prop = |name: &str| {
        run.stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} in {run}"))
            .to_string()
    };
    (prop("ActiveState"), prop("Job"))
}

/// `Budget::Immediate` is the native `systemctl start --no-block`, and it must actually reach
/// systemd. Which state the unit is in afterwards is a race — the job may be queued, running, or
/// done — but one state is impossible: systemd answers `StartUnit` only once the job is enqueued,
/// so after `start` returns the unit is never still `inactive` with no job. A `start` that skipped
/// the request is exactly that.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_start_with_no_budget_reaches_systemd() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id, Budget::Immediate)
        .expect("a start with no budget issues the request and returns");

    let (state, job) = active_state_and_job(guard.id());
    assert!(
        !(state == "inactive" && job.is_empty()),
        "the start job never reached systemd: ActiveState={state} Job={job:?}"
    );
}

/// The stop mirror of [`a_start_with_no_budget_reaches_systemd`]: after `StopUnit` is answered the
/// unit is never still `active` with no job.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_with_no_budget_reaches_systemd() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

    mgr.stop(&spec.id, Budget::Immediate)
        .expect("a stop with no budget issues the request and returns");

    let (state, job) = active_state_and_job(guard.id());
    assert!(
        !(state == "active" && job.is_empty()),
        "the stop job never reached systemd: ActiveState={state} Job={job:?}"
    );
}

/// A daemon that can never stop: it ignores `SIGTERM`, and a drop-in removes the `SIGKILL` systemd
/// would otherwise escalate to. [`Unstoppable`]'s `Drop` is what ends it.
fn unstoppable(id: &str) -> DaemonSpec {
    let mut spec = mk(id).into_fields();
    spec.command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "trap '' TERM; exec /bin/sleep infinity".to_string(),
    ];
    DaemonSpec::new(spec).unwrap()
}

/// Ends an [`unstoppable`] unit: `SIGKILL` lets the stop job it is stuck in complete, a blocking
/// `systemctl stop` waits for exactly that, and `reset-failed` clears the `failed` state the kill
/// leaves — which would otherwise outlive the unit file `ServiceGuard` removes. Never panics. Declared
/// after the `ServiceGuard`, so it drops first.
struct Unstoppable(String);

impl Unstoppable {
    fn install(mgr: &Systemd, spec: &DaemonSpec) -> Self {
        mgr.install(spec, false).expect("install");
        let dir = dropin_dir(spec.id.as_str());
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
        fs::write(dir.join("stop.conf"), "[Service]\nTimeoutStopSec=infinity\n").expect("write drop-in");
        cmd::run("systemctl", &["daemon-reload"]).expect_ok();
        Unstoppable(spec.id.as_str().to_string())
    }
}

impl Drop for Unstoppable {
    fn drop(&mut self) {
        let unit = format!("{}.service", self.0);
        warn_left_behind(cmd::run_cleanup_unless(
            "systemctl",
            &[
                &["kill", "--signal=SIGKILL", &unit],
                &["stop", &unit],
                &["reset-failed", &unit],
            ],
            not_loaded,
        ));
    }
}

/// A real expiry, deterministically: the stop can never complete, so a bounded `stop` has exactly
/// one correct answer — `WaitTimeout`, with the stop job left standing in systemd. A backend that
/// ignored `--timeout` would wait forever here, which the suite's watchdog surfaces.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_that_can_never_complete_times_out_with_the_job_standing() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = unstoppable(guard.id());
    let mgr = Systemd::new();
    let _unstoppable = Unstoppable::install(&mgr, &spec);
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

    let err = mgr
        .stop(&spec.id, Budget::Bounded(Duration::from_secs(1)))
        .expect_err("a stop that can never complete must not report stopped");

    assert!(
        matches!(err, goetia::Error::WaitTimeout { awaited: "stopped", .. }),
        "{err:?}"
    );
    let (state, job) = active_state_and_job(guard.id());
    assert_eq!(state, "deactivating", "the stop was not cancelled (Job={job:?})");
}

/// The budget never decides whether the request is sent. A budget spent before `systemctl` even
/// runs still enqueues the stop, and — the stop being unable to complete — still reports
/// `WaitTimeout` with the job standing: the same answer whatever is left, so no timing is bet on.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_on_a_spent_budget_still_reaches_systemd() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = unstoppable(guard.id());
    let mgr = Systemd::new();
    let _unstoppable = Unstoppable::install(&mgr, &spec);
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

    let err = mgr
        .stop(&spec.id, Budget::Bounded(Duration::from_nanos(1)))
        .expect_err("a stop that can never complete must not report stopped");

    assert!(
        matches!(err, goetia::Error::WaitTimeout { awaited: "stopped", .. }),
        "{err:?}"
    );
    let (state, job) = active_state_and_job(guard.id());
    assert_eq!(state, "deactivating", "the stop never reached systemd (Job={job:?})");
}

/// The start mirror: whatever a spent budget reports, the start job reached systemd, so the unit is
/// not still `inactive` with no job — see [`a_start_with_no_budget_reaches_systemd`].
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_start_on_a_spent_budget_still_reaches_systemd() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let result = mgr.start(&spec.id, Budget::Bounded(Duration::from_nanos(1)));

    assert!(
        matches!(
            result,
            Ok(()) | Err(goetia::Error::WaitTimeout { awaited: "running", .. })
        ),
        "{result:?}"
    );
    let (state, job) = active_state_and_job(guard.id());
    assert!(
        !(state == "inactive" && job.is_empty()),
        "the start job never reached systemd: ActiveState={state} Job={job:?}"
    );
}

/// The types of the jobs systemd has queued for `id`'s unit, as `systemctl list-jobs` reports them.
fn queued_job_types(id: &str) -> Vec<String> {
    let unit = format!("{id}.service");
    let run = cmd::run("systemctl", &["list-jobs", "--no-legend", "--plain", &unit]).expect_ok();
    run.stdout
        .lines()
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            [_, name, kind, ..] if name == unit => Some(kind.to_string()),
            _ => None,
        })
        .collect()
}

/// `MainPID` as `systemctl show` reports it.
pub(crate) fn main_pid(id: &str) -> String {
    let unit = format!("{id}.service");
    let run = cmd::run("systemctl", &["show", "--property=MainPID", "--value", &unit]).expect_ok();
    run.stdout.trim().to_string()
}

/// A runtime unit that `Requires=` and is `After=` the daemon, and whose own stop blocks on an
/// exclusive `flock` this guard holds. A stop of the daemon has to wait for it, so it stays a
/// *waiting* job until [`StopHolder::release`] — the shape in which a separate `start` replaces the
/// stop instead of queuing behind it. `Drop` releases the lock, stops the holder — its stop then
/// succeeds, so nothing is left `failed` — and removes it; declared after the `ServiceGuard`, so it
/// drops first.
///
/// The lock file lives in a temp dir, held in the last field so it is removed after `Drop` has
/// stopped the holder, whose `ExecStop=` still opens it. The temp dir is canonicalized, so a relative
/// `TMPDIR` cannot resolve against the unit's working directory `/`. The path is double-quoted in
/// that line, with backslash, double quote, `%` and `$` escaped; a path with a control character
/// (a newline would end the line) panics.
struct StopHolder {
    unit: String,
    path: PathBuf,
    held: Option<fs::File>,
    _lock_dir: tempfile::TempDir,
}

impl StopHolder {
    fn start(id: &str) -> Self {
        let unit = format!("{id}-holder.service");
        let path = PathBuf::from("/run/systemd/system").join(&unit);
        let lock_dir = tempfile::tempdir().expect("tempdir for the holder's lock");
        let lock = lock_dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canonicalize {}: {e}", lock_dir.path().display()))
            .join("lock");
        let mut holder = StopHolder {
            unit,
            path,
            held: None,
            _lock_dir: lock_dir,
        };
        let held = fs::File::create(&lock).unwrap_or_else(|e| panic!("create {}: {e}", lock.display()));
        // SAFETY: `held` owns the descriptor for the duration of the call.
        let locked = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&held), libc::LOCK_EX) };
        assert_eq!(
            locked,
            0,
            "flock {}: {}",
            lock.display(),
            std::io::Error::last_os_error()
        );
        holder.held = Some(held);
        let lock_str = lock.to_str().expect("a UTF-8 temp path");
        assert!(
            !lock_str.chars().any(char::is_control),
            "the lock path {lock_str:?} has a control character; set TMPDIR to a plain path"
        );
        let quoted = lock_str
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$");
        fs::write(
            &holder.path,
            format!(
                "[Unit]\nRequires={id}.service\nAfter={id}.service\n\
                 [Service]\nExecStart=/bin/sleep infinity\nExecStop=/usr/bin/flock \"{quoted}\" true\n\
                 TimeoutStopSec=infinity\n",
            ),
        )
        .expect("write the holder unit");
        cmd::run("systemctl", &["daemon-reload"]).expect_ok();
        cmd::run("systemctl", &["start", &holder.unit]).expect_ok();
        holder
    }

    fn release(&mut self) {
        self.held = None;
    }
}

impl Drop for StopHolder {
    fn drop(&mut self) {
        self.release();
        // `start` builds the holder before writing the unit, so a failed start leaves it unmade:
        // exit 5 is "unit not loaded", and a missing file is already gone.
        warn_left_behind(cmd::run_cleanup("systemctl", &[&["stop", &self.unit]], &[5]));
        remove_or_warn(&self.path, false);
        warn_left_behind(cmd::run_cleanup("systemctl", &[&["daemon-reload"]], &[]));
    }
}

/// `restart --timeout 0` must restart the daemon, even when systemd cannot run its stop at once. A
/// dependent holds the stop, so it waits; a separate `start` would replace it and, on a unit still
/// active, complete as a no-op. With the stop held, the daemon is still its old process and systemd
/// holds a `restart` job for it — nothing else. Released, a blocking `start` joins that job and
/// returns once it is done, and the daemon is a new process.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_restart_with_no_budget_restarts_behind_a_stop_that_has_to_wait() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let before = main_pid(guard.id());
    let mut holder = StopHolder::start(guard.id());
    let args = goetia::cli::restart::Args {
        ids: vec![guard.id().to_string()],
        wait: goetia::cli::wait::WaitArgs {
            timeout: Some(Duration::ZERO),
            no_timeout: false,
        },
    };
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Systemd::new())) };
    let (mut out, mut err) = (Vec::new(), Vec::new());

    // Truthful, not a stub: `support::elevated` is this test's own precondition.
    let code = goetia::cli::restart::run(&args, &get_manager, &|| true, &mut out, &mut err);

    let (out, err) = (String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    assert_eq!(code, ExitCode::Success, "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        queued_job_types(guard.id()),
        ["restart"],
        "systemd must hold a restart for the daemon behind the held stop"
    );
    assert_eq!(
        main_pid(guard.id()),
        before,
        "the stop is held, so nothing has stopped yet"
    );

    holder.release();
    cmd::run("systemctl", &["start", &format!("{}.service", guard.id())]).expect_ok();

    let after = main_pid(guard.id());
    assert!(
        after != before && after != "0",
        "the daemon was never restarted: MainPID {before} before, {after} after"
    );
}

/// The one `stop` with no job that is not a failure: a unit systemd cannot load, and that is not
/// running. `systemctl stop` answers it "not loaded" (exit `5`) and enqueues nothing — there is
/// nothing to stop — under every budget. A drop-in that empties `ExecStart=` is the unit it cannot
/// load.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_of_a_unit_systemd_cannot_load_has_nothing_to_stop() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false).expect("install");
    let dir = dropin_dir(guard.id());
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    fs::write(dir.join("bad.conf"), "[Service]\nExecStart=\n").expect("write drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
    let unit = format!("{}.service", guard.id());
    let load_state = cmd::run("systemctl", &["show", "--property=LoadState", "--value", &unit]).expect_ok();
    assert_eq!(load_state.stdout.trim(), "bad-setting");

    for budget in [Budget::DEFAULT, Budget::Unbounded, Budget::Immediate] {
        mgr.stop(&Id::try_from(guard.id()).unwrap(), budget)
            .unwrap_or_else(|e| panic!("{budget:?}: {e}"));
        assert_eq!(
            active_state_and_job(guard.id()),
            ("inactive".to_string(), String::new()),
            "{budget:?}"
        );
    }
}

/// `goetia <args>`, elevated as this test is, with `SYSTEMD_COLORS=<colors>` in its environment.
fn goetia_coloured(colors: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_goetia"))
        .args(args)
        .env("SYSTEMD_COLORS", colors)
        .output()
        .expect("spawn goetia")
}

/// `SYSTEMD_COLORS` forces colour into a pipe, and `systemctl --version` wraps its number in escapes
/// under it. None of its values may make goetia misread a supported systemd as one it refuses, on
/// any verb the version gate guards.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_coloured_environment_does_not_fail_the_version_gate() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_dir, manifest) = world_readable_manifest(guard.id());
    let manifest = manifest.to_str().expect("utf-8 temp path");
    let succeeds = |colors: &str, args: &[&str]| {
        let output = goetia_coloured(colors, args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "SYSTEMD_COLORS={colors} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    succeeds("1", &["daemon", "install", "--file", manifest]);
    for colors in ["1", "true", "16", "256", "24bit"] {
        for verb in ["start", "restart", "stop"] {
            succeeds(colors, &["daemon", verb, guard.id()]);
        }
    }
    succeeds("1", &["daemon", "uninstall", guard.id()]);
    assert!(!unit_path(guard.id()).exists(), "uninstall left the unit");
}

/// `goetia <args>`, elevated as this test is, with a `systemctl` first on its `PATH` whose running
/// manager reports `manager` and whose client reports `client`, and that fails anything else: a
/// refusal must come before goetia asks systemd for anything more.
///
/// Written by a child `sh`, not by this process: a descriptor this process held open for writing
/// could be inherited by a `fork` on another test thread, and `exec`ing the file would then fail
/// with `ETXTBSY`.
fn goetia_on_systemd(manager: u32, client: u32, args: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = dir.path().join("systemctl");
    let script = format!(
        "#!/bin/sh\n\
         [ \"$*\" = 'show --property=Version --value' ] && {{ echo {manager}; exit 0; }}\n\
         [ \"$*\" = --version ] && {{ echo 'systemd {client} ({client})'; exit 0; }}\n\
         echo \"stand-in: unexpected systemctl $*\" >&2; exit 99\n"
    );
    cmd::run(
        "/bin/sh",
        &[
            "-c",
            "printf '%s' \"$1\" > \"$0\" && chmod 0755 \"$0\"",
            fake.to_str().expect("utf-8 temp path"),
            &script,
        ],
    )
    .expect_ok();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").expect("PATH is set")
    );
    Command::new(env!("CARGO_BIN_EXE_goetia"))
        .args(args)
        .env("PATH", path)
        .output()
        .expect("spawn goetia")
}

/// Below systemd 242 goetia refuses before it writes a unit or runs a start or stop — rather than
/// install a `Type=exec` unit an older systemd silently runs as `Type=simple`, or fail every start
/// and stop on an unrecognized option. The running systemd and the client each count: an old one
/// is refused next to a new other, and named.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_older_systemd_is_refused_before_anything_is_written_or_run() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_dir, manifest) = world_readable_manifest(guard.id());
    let manifest = manifest.to_str().expect("utf-8 temp path");
    let versions = [
        (241, 257, "The running systemd is 241"),
        (257, 241, "The `systemctl` client is 241"),
    ];
    let refused = |args: &[&str]| {
        for (manager, client, named) in versions {
            let output = goetia_on_systemd(manager, client, args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
            assert!(stderr.contains("requires systemd 242 or newer"), "{args:?}: {stderr}");
            assert!(stderr.contains(named), "{args:?}: {stderr}");
        }
    };

    refused(&["daemon", "install", "--file", manifest]);
    assert!(!unit_path(guard.id()).exists(), "a refused install wrote the unit");

    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false)
        .expect("install on this host's systemd");
    for verb in ["start", "stop", "restart", "uninstall"] {
        refused(&["daemon", verb, guard.id()]);
        assert_eq!(
            active_state_and_job(guard.id()),
            ("inactive".to_string(), String::new()),
            "a refused {verb} ran something"
        );
    }
    assert!(unit_path(guard.id()).exists(), "a refused uninstall removed the unit");
}

/// On a system booted with systemd, a manager `systemctl` cannot reach is not evidence that none
/// runs: the version gate refuses, naming what `systemctl` said, before anything is written. Its
/// own mount namespace hides the manager's private socket, and the bus address points at nothing,
/// so `systemctl show` fails at once while `/run/systemd/system` stays.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_running_systemd_that_cannot_be_asked_is_refused() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_dir, manifest) = world_readable_manifest(guard.id());

    let output = Command::new("unshare")
        .args([
            "--mount",
            "--propagation",
            "private",
            "/bin/sh",
            "-c",
            "mount --bind /dev/null /run/systemd/private && \
             exec env DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_goetia"),
            "daemon",
            "install",
            "--file",
            manifest.to_str().expect("utf-8 temp path"),
        ])
        .output()
        .expect("spawn unshare");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("cannot tell whether the running systemd is 242+"),
        "{stderr}"
    );
    // systemctl's own words, up to where versions and hosts differ: 257 says "Failed to connect to
    // system scope bus via local transport: Connection refused" here, and the Ubuntu CI runner's
    // says "Failed to connect to bus: No such file or directory".
    assert!(stderr.contains("Failed to connect to"), "{stderr}");
    assert!(!unit_path(guard.id()).exists(), "a refused install wrote the unit");
}

/// `goetia <args>` with a `systemctl` first on its `PATH` that appends every argv it is run with to
/// `log`, one per line, and then runs the real one. Written by a child `sh`, as
/// [`goetia_on_systemd`]'s stand-in is, and for the same reason.
fn goetia_logging_systemctl(dir: &Path, args: &[&str]) -> std::process::Output {
    let path = std::env::var("PATH").expect("PATH is set");
    let real = std::env::split_paths(&path)
        .map(|dir| dir.join("systemctl"))
        .find(|candidate| candidate.is_file())
        .expect("systemctl on PATH");
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        dir.join("log").display(),
        real.display()
    );
    let stand_in = dir.join("systemctl");
    cmd::run(
        "/bin/sh",
        &[
            "-c",
            "printf '%s' \"$1\" > \"$0\" && chmod 0755 \"$0\"",
            stand_in.to_str().expect("utf-8 temp path"),
            &script,
        ],
    )
    .expect_ok();
    Command::new(env!("CARGO_BIN_EXE_goetia"))
        .args(args)
        .env("PATH", format!("{}:{path}", dir.display()))
        .output()
        .expect("spawn goetia")
}

/// The version gate probes `systemctl` once per daemon per verb, however many steps the verb takes:
/// `restart`'s stop and start, `install --start`'s install and start, `uninstall`'s stop, `disable`
/// and `daemon-reload`, share one pair of probes, and so one share of the budget. `enable` and
/// `disable` ask it too: no request is sent before the manager is asked.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn the_version_gate_probes_once_per_daemon_per_verb() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let (_manifest_dir, manifest) = world_readable_manifest(guard.id());
    let manifest = manifest.to_str().expect("utf-8 temp path");
    let probes = |args: &[&str]| {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = goetia_logging_systemctl(dir.path(), args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(dir.path().join("log")).expect("read the log");
        let count = |probe: &str| log.lines().filter(|line| line.starts_with(probe)).count();
        (count("--version"), count("show --property=Version"))
    };

    for args in [
        &["daemon", "install", "--file", manifest, "--start"][..],
        &["daemon", "restart", guard.id()],
        &["daemon", "restart", guard.id(), "--timeout", "0"],
        &["daemon", "start", guard.id()],
        &["daemon", "stop", guard.id()],
        &["daemon", "enable", guard.id()],
        &["daemon", "disable", guard.id()],
        &["daemon", "uninstall", guard.id()],
        &["daemon", "install", "--file", manifest],
    ] {
        assert_eq!(probes(args), (1, 1), "{args:?}");
    }
}

/// Exit code of [`SCOPE_PIDS_PREAMBLE`] when the scope's `pids.max` is not `$EXPECTED_TASKS_MAX`;
/// the script hardcodes it.
const CAP_NOT_IN_FORCE: i32 = 97;

/// Shell builtins only, so it forks nothing and the scope's task count is unchanged: prints the
/// scope's cgroup path, `pids.max` and `pids.current` on stderr (v1/hybrid `pids` line, else v2 `0::`
/// line), then `exec`s `$0 "$@"`. `printf '%s\n'`, not `echo`, which would interpret backslash
/// escapes in the evidence.
///
/// A scope whose `pids.max` is absent or not `$EXPECTED_TASKS_MAX` never got the cap, so the script
/// exits [`CAP_NOT_IN_FORCE`] without running the verb. The extra evidence tells a `pids` controller
/// missing from the root at boot (`root.controllers`) from the parent failing to enable it for its
/// children (`parent.subtree` lacking `pids` while `root.subtree` has it). `parent.subtree` and
/// `root.subtree` exist only on cgroup v2; on v1/hybrid they read `unreadable`.
const SCOPE_PIDS_PREAMBLE: &str = r#"
cg=; v1=; v1ctl=; seen=
while read -r line; do
  seen="$seen[$line]"
  rest=${line#*:}; controllers=${rest%%:*}; path=${rest#*:}
  case $line in 0::*) cg=$path;; esac
  case ",$controllers," in *,pids,*) v1=$path; v1ctl=$controllers;; esac
done < /proc/self/cgroup
tree=; dir=
if [ -n "$v1" ]; then tree=v1; dir=/sys/fs/cgroup/$v1ctl$v1
elif [ -n "$cg" ]; then tree=v2; dir=/sys/fs/cgroup$cg; fi
if [ -n "$dir" ]; then
  max=unreadable; cur=unreadable
  psub=unreadable; sctl=unreadable; rctl=unreadable; rsub=unreadable
  read -r psub < "${dir%/*}/cgroup.subtree_control"; read -r sctl < "$dir/cgroup.controllers"
  read -r rctl < /sys/fs/cgroup/cgroup.controllers; read -r rsub < /sys/fs/cgroup/cgroup.subtree_control
  read -r max < "$dir/pids.max"; read -r cur < "$dir/pids.current"
  printf '%s\n' "scope at start: tree=$tree cgroup=$dir pids.max=$max pids.current=$cur parent.subtree=[$psub] scope.controllers=[$sctl] root.controllers=[$rctl] root.subtree=[$rsub]" >&2
else
  max=unreadable
  printf '%s\n' "scope at start: no pids cgroup in /proc/self/cgroup: $seen" >&2
fi
if [ "$max" != "$EXPECTED_TASKS_MAX" ]; then exit 97; fi
exec "$0" "$@"
"#;

/// Removes the transient scope `unit` the test made: one still loaded, or left `failed` (a scope
/// that outlives its `TimeoutStopSec` stays loaded in that state until `reset-failed`). `stop`, then
/// `reset-failed`, each retried for `CLEANUP_BOUND` and then let go with a warning; never panics, so
/// it also runs while the test unwinds. A scope that never was, or is gone already, is "not loaded".
struct ScopeGuard(String);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        warn_left_behind(cmd::run_cleanup("systemctl", &[&["stop", &self.0]], &[5]));
        warn_left_behind(cmd::run_cleanup_unless(
            "systemctl",
            &[&["reset-failed", &self.0]],
            not_loaded,
        ));
    }
}

/// Whether a failed `systemctl` run says its unit is not loaded: `stop` exits 5, while `kill` and
/// `reset-failed` pass the bus error through as exit 1.
fn not_loaded(run: &cmd::Run) -> bool {
    run.code == Some(5) || (run.code == Some(1) && run.stderr.contains("not loaded"))
}

/// `goetia <args>` in the transient scope `unit` allowed `tasks` tasks — threads and processes
/// alike — so that making a thread, or spawning a process, fails once they are spent.
fn goetia_with_tasks(unit: &str, tasks: u32, args: &[&str]) -> std::process::Output {
    Command::new("systemd-run")
        .args(["--quiet", "--scope", "--unit", unit, "-p", &format!("TasksMax={tasks}")])
        .args(["sh", "-c", SCOPE_PIDS_PREAMBLE])
        .arg(env!("CARGO_BIN_EXE_goetia"))
        .args(args)
        .env("EXPECTED_TASKS_MAX", tasks.to_string())
        .output()
        .expect("spawn systemd-run")
}

/// The host's cgroup state after a scope that did not get its cap: root and `system.slice`
/// controllers, uptime, whether the unit is still loaded and PID 1's recent journal. It runs after
/// the scope exited, so it cannot show the cap at start; the scope's own stderr line is the only
/// start-time evidence.
fn cap_evidence(unit: &str) -> String {
    let script = format!(
        "cd /sys/fs/cgroup; for f in cgroup.controllers cgroup.subtree_control system.slice/cgroup.controllers \
         system.slice/cgroup.subtree_control; do echo \"$f: $(cat $f)\"; done; \
         echo \"uptime: $(cut -d' ' -f1 /proc/uptime)\"; \
         systemctl show -p ActiveState,LoadState {unit}; \
         journalctl -b --no-pager -o short-precise -n 40 _PID=1"
    );
    match Command::new("sh").args(["-c", &script]).output() {
        Ok(out) => format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(err) => format!("the evidence command failed to spawn: {err}"),
    }
}

/// goetia never panics for want of a task; a verb that cannot get what its request needs exits `1`,
/// and `start` and a waited `restart` then leave the daemon untouched. Each verb sweeps a rising
/// limit until it succeeds; the cutoff is goetia's business and is not asserted.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_verb_short_of_tasks_sends_nothing_and_never_panics() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    Systemd::new().install(&mk(guard.id()), false).expect("install");

    let scopes = std::cell::Cell::new(0u32);
    let sweep = |args: &[&str], untouched: &dyn Fn(u32)| {
        for tasks in 1.. {
            scopes.set(scopes.get() + 1);
            let unit = format!("{}-tasks-{}.scope", guard.id(), scopes.get());
            let _scope = ScopeGuard(unit.clone());
            let output = goetia_with_tasks(&unit, tasks, args);
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(!stderr.contains("panicked"), "{args:?} TasksMax={tasks}: {stderr}");
            match output.status.code() {
                Some(CAP_NOT_IN_FORCE) => panic!(
                    "harness defect: {unit} did not have pids.max={tasks} at start, so the verb was not run\n\
                     --- scope evidence (stderr) ---\n{stderr}\n--- host evidence ---\n{}",
                    cap_evidence(&unit),
                ),
                Some(0) => {
                    // One task is below goetia's own need, so a success here means the sweep never
                    // starved the verb.
                    assert!(
                        tasks > 1,
                        "{args:?} succeeded with a single task, so the sweep tested nothing\n\
                         scope: {unit} (TasksMax={tasks})\n\
                         --- stdout ---\n{}\n\
                         --- stderr ---\n{stderr}",
                        String::from_utf8_lossy(&output.stdout),
                    );
                    return;
                }
                // goetia's own refusal, not `systemd-run` failing to make the scope.
                Some(1) if stderr.contains("error: ") && !stderr.contains("Failed to start transient scope unit") => {
                    untouched(tasks)
                }
                other => panic!("{args:?} TasksMax={tasks}: exit {other:?}: {stderr}"),
            }
        }
    };

    sweep(&["daemon", "start", guard.id()], &|tasks| {
        assert_eq!(
            active_state_and_job(guard.id()),
            ("inactive".to_string(), String::new()),
            "a start short of tasks sent something: TasksMax={tasks}"
        );
    });

    let before = main_pid(guard.id());
    sweep(&["daemon", "restart", guard.id()], &|tasks| {
        assert_eq!(
            (active_state_and_job(guard.id()), main_pid(guard.id())),
            (("active".to_string(), String::new()), before.clone()),
            "a restart short of tasks touched the daemon: TasksMax={tasks}"
        );
    });
    assert_ne!(
        main_pid(guard.id()),
        before,
        "the provisioned restart restarted nothing"
    );

    // A no-wait restart is one `systemctl restart --no-block`: a failure sends nothing. Success is
    // checked as in `a_start_with_no_budget_reaches_systemd`.
    let before = main_pid(guard.id());
    sweep(&["daemon", "restart", guard.id(), "--timeout", "0"], &|tasks| {
        assert_eq!(
            (active_state_and_job(guard.id()), main_pid(guard.id())),
            (("active".to_string(), String::new()), before.clone()),
            "an unwatched restart short of tasks touched the daemon: TasksMax={tasks}"
        );
    });
    let (state, job) = active_state_and_job(guard.id());
    assert!(
        !job.is_empty() || !(state == "inactive" || (state == "active" && main_pid(guard.id()) == before)),
        "the provisioned restart --timeout 0 never reached systemd: ActiveState={state} Job={job:?}"
    );
}

/// `Type=exec` is what makes `systemctl start` report failure here at all — under `Type=simple` the
/// same spec's `start` returns `Ok`. `restart: always` is pinned explicitly (`mk`'s default is
/// `OnFailure`) so the unit under test is exactly `Type=exec` + `Restart=always` +
/// `StartLimitIntervalSec=0` (emitted only when restart is enabled), the shape measured to fail
/// `start` in 5/5.
///
/// Asserts only on `start`'s result: by the time it returns, the unit is already
/// `activating`/`auto-restart`, not `failed` — a follow-up state read would be asserting a race
/// against systemd's own restart loop.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_unit_whose_executable_does_not_exist_fails_start() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mut spec = mk(guard.id()).into_fields();
    spec.command = vec!["/nonexistent/goetia-test-executable".to_string()];
    spec.restart = Restart::Always;
    let spec = DaemonSpec::new(spec).unwrap();
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let result = mgr.start(&spec.id, Budget::DEFAULT);

    assert!(result.is_err(), "{result:?}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_leaves_nothing() {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = mk(&id);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&spec.id).expect("enable");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    write_dropin(&id);

    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(!unit_path(&id).exists(), "fragment must be gone");
    assert!(!dropin_dir(&id).exists(), "drop-in directory must be gone");
    assert!(
        fs::symlink_metadata(wants_symlink(&id)).is_err(),
        "the `.wants` symlink must be gone"
    );
}

// Residual artifacts: absence is about the id, not about the fragment file ============================================

/// The state `uninstall`'s own partial-failure path leaves behind when removing the drop-in
/// directory fails after the fragment is already gone: the retry it tells you to run must not then
/// exit `0` and print "nothing to do". `install` on this exact state is
/// `install_refuses_a_stray_dropin_with_no_fragment`'s `RefuseForeign`, and two verbs must not
/// describe one filesystem state differently.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_refuses_an_id_whose_fragment_is_gone_but_whose_dropin_remains() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id); // removes the drop-in directory
    write_dropin(guard.id()); // no fragment ever written
    assert!(!unit_path(guard.id()).exists(), "the fragment is what this state lacks");

    let (code, out, err) = uninstall_via_cli(guard.id());

    assert_ne!(code, ExitCode::Success, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("not installed (nothing to do)"), "{out}");
    assert!(
        dropin_dir(guard.id()).exists(),
        "and the drop-in survives: goetia cannot tell its own leftover from an administrator's \
         override of a unit shipped in /usr/lib, so it removes neither"
    );
}

/// The same for the other artifact that outlives the fragment. `systemctl disable` is impossible
/// once the `[Install]` section is gone, so this link keeps the id enrolled at boot
/// — the very reason the exit-code table gives for `disable` returning `1` here.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_refuses_an_id_whose_fragment_is_gone_but_whose_enablement_link_remains() {
    let id = support::random_test_id();
    let link = wants_symlink(&id);
    // No `ServiceGuard`: the id never gets a fragment, so only the planted link needs removing.
    let _cleanup = FileGuard::new(link.clone());
    // Like `systemctl enable`, create `multi-user.target.wants` if missing and leave it behind.
    fs::create_dir_all(link.parent().expect("the link has a parent")).expect("create the wants directory");
    std::os::unix::fs::symlink(unit_path(&id), &link).expect("plant a dangling .wants link");

    let (code, out, err) = uninstall_via_cli(&id);

    assert_ne!(code, ExitCode::Success, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("not installed (nothing to do)"), "{out}");
    assert!(fs::symlink_metadata(&link).is_ok(), "and the link survives, unremoved");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_a_stray_enablement_link_with_no_fragment() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let link = wants_symlink(guard.id());
    // Like `systemctl enable`, create `multi-user.target.wants` if missing and leave it behind.
    fs::create_dir_all(link.parent().expect("the link has a parent")).expect("create the wants directory");
    std::os::unix::fs::symlink(unit_path(guard.id()), &link).expect("plant a dangling .wants link");

    let mgr = Systemd::new();
    let outcome = mgr
        .install(&mk(guard.id()), false)
        .expect("install over a stray enablement link");
    assert!(
        matches!(outcome, Outcome::RefuseForeign { .. }),
        "adopting it would enroll the new daemon at boot without anyone asking, got {outcome:?}"
    );
    assert!(!unit_path(guard.id()).exists(), "no fragment must be written");
}

/// The read-only verb has to agree too, or `status` calls the id empty while `install` calls it
/// foreign — the same disagreement one step removed.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn status_does_not_call_an_id_with_a_stray_dropin_not_installed() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_dropin(guard.id());

    let err = Systemd::new()
        .status(&Id::try_from(guard.id().to_string()).expect("valid id"))
        .expect_err("a stray drop-in is not a reportable daemon");
    assert!(
        matches!(err, goetia::Error::Foreign { .. }),
        "must not be NotInstalled: {err:?}"
    );
}

/// The refusal for an id with no fragment names what is left, so the user has the paths to remove
/// without hunting for them. (`install` over the same state is `decide`'s generic foreign refusal;
/// only `status` and the other verbs carry this recovery.)
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn status_of_residue_names_the_leftover_paths() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_dropin(guard.id());

    let err = Systemd::new()
        .status(&Id::try_from(guard.id().to_string()).expect("valid id"))
        .expect_err("a stray drop-in is not a reportable daemon");
    let goetia::Error::Foreign { recovery, .. } = &err else {
        panic!("residue is foreign, not absent and not undetermined: {err:?}");
    };
    assert!(
        recovery.contains(&dropin_dir(guard.id()).display().to_string()),
        "the recovery must name the leftover directory: {recovery}"
    );
}

/// A fragment goetia wrote for a spec naming a Windows account (a SID) carries goetia's marker, so
/// it is goetia's, but the systemd backend cannot render that spec: it is `OursUnusable` in `list`
/// and an error in `status`.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_reports_a_sid_account_as_ours_unusable() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mut spec = mk(guard.id()).into_fields();
    spec.user = User::Id(AccountId::Sid("S-1-5-21-1-2-3-1001".to_string()));
    let spec = DaemonSpec::new(spec).unwrap();
    let text = generate::unit(&spec, &Identity { user: "0".to_string() }).unwrap();
    fs::write(unit_path(guard.id()), text).expect("seed a unit whose spec names a SID");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();

    let mgr = Systemd::new();
    let cannot_render = mgr
        .preview_install(&spec)
        .expect_err("a spec naming a SID cannot be rendered")
        .report()
        .to_string();
    let listed = mgr
        .list()
        .expect("one unrenderable unit must not take down the listing");
    let reason = listed
        .iter()
        .find_map(|entry| match entry {
            Installed::OursUnusable { name, .. } if name == guard.id() => entry.unusable_reason(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a marked fragment is ours even when its spec cannot be rendered: {listed:?}"));
    assert_eq!(
        reason,
        format!("marked ours, but its embedded spec cannot be regenerated: {cannot_render}")
    );

    let err = mgr
        .status(&Id::try_from(guard.id().to_string()).expect("valid id"))
        .expect_err("a spec that cannot be rendered has no status to report");
    assert_eq!(
        err.report().to_string(),
        format!(
            "`{}`'s embedded spec cannot be regenerated: {cannot_render}",
            guard.id()
        )
    );
}

// A read that failed establishes no ownership =========================================================================

/// A drop-in directory the caller cannot open leaves goetia unable to say whether *anything* is
/// installed at the id. Reporting `unusable` for it — "goetia owns the id but cannot report on
/// it", whose published remedy is `uninstall` — would put goetia's name and destructive advice on
/// what is just as plausibly an administrator's override of a unit shipped elsewhere.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn status_reports_undetermined_for_a_dropin_directory_it_cannot_read() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_unreadable_dropin(guard.id());
    assert!(!unit_path(guard.id()).exists(), "no fragment: this is the absence path");

    let output = run_unelevated(&["daemon", "status", guard.id(), "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");

    // Doubles as proof that the denial actually happened: root reads that directory fine and finds
    // one `.conf`, which reports `foreign`. So a run whose reader was not unprivileged fails here
    // instead of passing vacuously.
    assert!(stdout.contains(r#""kind":"undetermined""#), "{context}");
    assert_eq!(output.status.code(), Some(4), "{context}");
    assert!(
        stdout.contains(&format!("{}.service.d", guard.id())),
        "the message must name the path that could not be read: {context}"
    );
    assert!(
        stdout.contains("re-run as root"),
        "and say what would make it readable: {context}"
    );
    assert!(
        !stdout.contains("uninstall"),
        "nothing here establishes that the drop-in is goetia's to remove: {context}"
    );
}

/// The same absence path `status` takes above, reached through `diff` instead: with no fragment,
/// `residue`'s drop-in scan is what would have answered "is anything at this id", and it did not
/// complete. `4`, not the `1` `diff`'s catch-all `Err` arm gives: it was asked a question and could
/// not determine the answer — the same reasoning that already puts `Outcome::RefuseUnusable` at
/// `4` there.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn diff_reports_a_dropin_directory_it_cannot_read_as_indeterminate() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_unreadable_dropin(guard.id());
    let (_tempdir, manifest) = world_readable_manifest(guard.id());

    let output = run_unelevated(&["daemon", "diff", "-f", manifest.to_str().expect("a UTF-8 temp path")]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");

    // Root would read the drop-in and report a conflict (`5`), so this cannot pass vacuously either.
    assert_eq!(output.status.code(), Some(4), "{context}");
    assert!(stderr.contains("cannot determine"), "{context}");
    assert!(!stderr.contains("uninstall"), "{context}");
}

/// The fragment is open, read and decoded, so goetia's ownership of this id is an established fact
/// — and `Error::Undetermined` ("cannot determine whether daemon `X` is installed") would deny it
/// about an id goetia just decoded. What the unreadable drop-in directory actually costs is the
/// ability to report on the id, which is `Outcome::RefuseUnusable`.
///
/// `ENOTDIR` rather than `EACCES`, so this runs as root without a second uid: a regular file where
/// `<id>.service.d` has to be fails `read_dir` identically for everyone, and is exactly the shape an
/// elevated `install` meets when the drop-in directory fails for a reason no privilege fixes.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_an_unreadable_dropin_over_our_own_fragment() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false).expect("install");

    let blocker = dropin_dir(guard.id());
    let _cleanup = FileGuard::new(blocker.clone());
    fs::write(&blocker, "").expect("plant a regular file where the drop-in directory would be");

    let outcome = mgr
        .install(&mk(guard.id()), false)
        .expect("a drop-in read that failed is not a failure to determine whether the id is installed");
    let Outcome::RefuseUnusable { reason, .. } = &outcome else {
        panic!("goetia owns this id and cannot report on it, got {outcome:?}");
    };
    let reason = reason.to_string();
    assert!(
        reason.contains(&blocker.display().to_string()),
        "the refusal must name the path that could not be read: {reason}"
    );
    assert!(
        !reason.contains("cannot determine"),
        "installation and ownership are both established here: {reason}"
    );
    assert!(
        unit_path(guard.id()).exists(),
        "nothing was rewritten, and nothing was removed"
    );
}

/// The same state seen by an unprivileged reader, through the CLI: `diff` must refuse it as an id
/// goetia owns (exit `4`), never report that it cannot tell whether the id is installed.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn diff_refuses_our_own_fragment_with_an_unreadable_dropin() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    Systemd::new().install(&mk(guard.id()), false).expect("install");
    write_unreadable_dropin(guard.id());
    let (_tempdir, manifest) = world_readable_manifest(guard.id());

    let output = run_unelevated(&["daemon", "diff", "-f", manifest.to_str().expect("a UTF-8 temp path")]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");

    // Root reads the drop-in and reports a conflict (`5`), so a run whose reader was not
    // unprivileged fails here instead of passing vacuously.
    assert_eq!(output.status.code(), Some(4), "{context}");
    assert!(stdout.contains("would be refused"), "{context}");
    assert!(
        !stdout.contains("cannot determine") && !stderr.contains("cannot determine"),
        "the fragment was read and its marker decoded, so this id's installation is not in doubt: \
         {context}"
    );
}

/// The first read of the path, and the one where a plain `EACCES` is most likely: a drop-in
/// directory is usually world-searchable, while the fragment's own mode governs its readability.
/// `lstat` succeeds for an unreadable regular file on directory-search permission alone, so
/// classifying it from "the open failed and the `lstat` did not" reported `foreign` — "demonstrably
/// not managed by goetia" — about a file whose contents nobody had looked at.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn status_reports_undetermined_for_a_fragment_it_cannot_read() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());
    fs::set_permissions(unit_path(guard.id()), fs::Permissions::from_mode(0o600)).expect("chmod 0600");

    let output = run_unelevated(&["daemon", "status", guard.id(), "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");

    // Doubles as proof that the denial actually happened: root reads this fragment fine and reports
    // `foreign`, exit `1`.
    assert!(stdout.contains(r#""kind":"undetermined""#), "{context}");
    assert_eq!(output.status.code(), Some(4), "{context}");
    assert!(
        stdout.contains(&format!("{}.service", guard.id())),
        "the message must name the path that could not be read: {context}"
    );
    assert!(stdout.contains("re-run as root"), "{context}");
}

// Every drop-in systemd reads is drift ================================================================================

/// `/etc/systemd/system.control` is where `systemctl set-property UNIT PROPERTY=VALUE` writes, and
/// `systemd.unit(5)`'s System Unit Search Path puts it *above* `/etc/systemd/system`. Leaving it
/// unscanned reported an id "up to date" while systemd applied a memory cap goetia never wrote.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_control_dropin_is_drift() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false).expect("install");

    // Non-vacuity: the same id is clean until the drop-in lands.
    assert_eq!(
        mgr.preview_install(&mk(guard.id())).expect("preview"),
        Outcome::UpToDate
    );

    let (dir, _cleanup) = seed_control_dropin("/etc/systemd/system.control", guard.id());

    let outcome = mgr.preview_install(&mk(guard.id())).expect("preview");
    let Outcome::Conflict {
        unclearable_recovery, ..
    } = &outcome
    else {
        panic!(
            "systemd applies {} to this unit, so it is not up to date, got {outcome:?}",
            dir.display()
        );
    };

    // And `--force` cannot resolve it: goetia clears only its own `/etc/systemd/system` drop-in, so
    // forcing would rewrite the fragment, leave this directory, and report the identical conflict
    // next run. The message has to say so and name the directory.
    let recovery = unclearable_recovery
        .as_deref()
        .unwrap_or_else(|| panic!("`--force` is not the remedy here, got {outcome:?}"));
    assert!(
        recovery.contains(&dir.display().to_string()),
        "the operator needs the path to remove: {recovery}"
    );
    assert!(recovery.contains("daemon-reload"), "{recovery}");
}

/// The occupancy half of the same omission: with the fragment gone, `residue` found nothing under
/// the `.control` root, so `uninstall` certified "nothing to do" (exit `0`) for an id systemd still
/// holds configuration for — while `install` on that same state refuses it as foreign.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_refuses_an_id_whose_only_artifact_is_a_control_dropin() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);

    // Non-vacuity: with nothing at the id at all, this is the `0` the assertion below rejects.
    let (code, _out, _err) = uninstall_via_cli(guard.id());
    assert_eq!(code, ExitCode::Success, "an empty id is `nothing to do`");

    // The `/run` root this time, so the two `.control` tests never share a directory to clean up.
    let (dir, _cleanup) = seed_control_dropin("/run/systemd/system.control", guard.id());

    let (code, out, err) = uninstall_via_cli(guard.id());
    let context = format!("stdout:\n{out}\nstderr:\n{err}");
    assert_ne!(
        code,
        ExitCode::Success,
        "`uninstall && echo confirmed gone` must not print here: {context}"
    );
    assert!(
        err.contains(&dir.display().to_string()),
        "and the refusal must name what is still there: {context}"
    );

    // The two verbs must describe one filesystem state the same way.
    let outcome = Systemd::new().preview_install(&mk(guard.id())).expect("preview");
    assert!(
        matches!(outcome, Outcome::RefuseForeign { .. }),
        "install refuses this state, so uninstall cannot call it empty, got {outcome:?}"
    );
}

/// The stated limitation, pinned so it stays deliberate. `systemd.unit(5)` does read
/// `foo-.service.d` for `foo-bar.service`, and goetia does not scan it: it is named for a family of
/// units rather than for this id, so reporting it as this id's `Conflict` would be untrue, and
/// `--force` — the published remedy — rewrites the fragment without touching a directory that
/// governs unrelated units. The id carries its own random component *before* the truncation point,
/// so the directory this seeds belongs to this test alone.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_dash_truncated_dropin_is_not_this_ids_conflict() {
    let id = format!("{}-leaf", support::random_test_id());
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false).expect("install");

    let truncated = guard.id().rsplit_once('-').expect("the id has a dash").0.to_string();
    let dir = PathBuf::from(support::SYSTEMD_UNIT_DIR).join(format!("{truncated}-.service.d"));
    let _cleanup = seed_dropin(&dir, CreatedRoot::default());

    let outcome = mgr.preview_install(&mk(guard.id())).expect("preview");
    assert_eq!(
        outcome,
        Outcome::UpToDate,
        "{} is a sibling of this id's artifact, not part of it",
        dir.display()
    );

    // And the same id's *own* drop-in still is drift, so this is a boundary, not a hole — and one
    // `--force` does resolve, since `/etc/systemd/system/<id>.service.d` is goetia's to clear.
    let _own = seed_dropin(&dropin_dir(guard.id()), CreatedRoot::default());
    let outcome = mgr.preview_install(&mk(guard.id())).expect("preview");
    assert!(
        matches!(
            outcome,
            Outcome::Conflict {
                unclearable_recovery: None,
                ..
            }
        ),
        "`<id>.service.d` is this id's artifact, and goetia's own to clear, got {outcome:?}"
    );
}

/// The regression this boundary exists to prevent. A top-level `service.d` applies to *every*
/// service unit on the host (verified: one `.conf` there reached `cron.service`'s `Documentation=`),
/// so scanning it put every goetia daemon on such a host into a permanent `Conflict` that `--force`
/// rewrote the fragment for and never cleared — force, be told to force again, forever.
///
/// Seeded with an inert `[Unit] Documentation=` rather than the `MemoryMax=` the other drop-in tests
/// use: for as long as this file exists, systemd really is applying it to every service on the
/// machine, including the ones other tests are running.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_top_level_service_d_dropin_is_not_this_ids_conflict() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&mk(guard.id()), false).expect("install");

    let dir = PathBuf::from(support::SYSTEMD_UNIT_DIR).join("service.d");
    assert!(
        !dir.exists(),
        "{} already exists on this host; this test would remove an administrator's directory",
        dir.display()
    );
    fs::create_dir(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    // Built after the mkdir, not before: it is atomic, so on failure nothing was created and a guard
    // would remove a host-wide directory this test did not make.
    let _cleanup = DirGuard::new(dir.clone());
    fs::write(
        dir.join("50-goetia-test.conf"),
        "[Unit]\nDocumentation=https://example.invalid/goetia-test\n",
    )
    .expect("write the host-wide drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();

    let outcome = mgr.preview_install(&mk(guard.id())).expect("preview");
    assert_eq!(
        outcome,
        Outcome::UpToDate,
        "a host-wide policy modified no artifact of goetia's, and `--force` could not clear it"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_ignores_foreign_units() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());

    let listed = Systemd::new().list().expect("list");
    assert!(
        !may_account_for(&listed, guard.id()),
        "a foreign unit goetia read in full must not appear in list()"
    );
}

/// `list` needs no elevation: an unprivileged run reports an install it can read as ours, not as
/// `undetermined`. The ids of other daemons on the host are not asserted about.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_works() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    Systemd::new().install(&mk(guard.id()), false).expect("install");

    support::list_json(run_unelevated).assert_own(&[id.as_str()]);
}

// Parent directories ==================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_creates_and_owns_fresh_cwd_and_logs_dirs_for_a_named_user() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let base = std::env::temp_dir().join(format!("{}-parent-dirs", guard.id()));
    let cwd = base.join("work");
    let logs = base.join("logs").join("out.log");
    let _rm = DirGuard::new(base.clone());

    let mut spec = mk(guard.id()).into_fields();
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());
    spec.logs = Some(logs.clone());
    let spec = DaemonSpec::new(spec).unwrap();

    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let nobody_uid = uid_of("nobody");
    for dir in [cwd.as_path(), logs.parent().unwrap()] {
        let meta = fs::metadata(dir).unwrap_or_else(|e| panic!("stat {}: {e}", dir.display()));
        assert!(meta.is_dir(), "{} must exist and be a directory", dir.display());
        assert_eq!(meta.permissions().mode() & 0o777, 0o755, "{}: mode", dir.display());
        assert_eq!(meta.uid(), nobody_uid, "{}: owner", dir.display());
    }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_accepts_preexisting_dirs_the_target_account_cannot_write() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let base = std::env::temp_dir().join(format!("{}-preexisting", guard.id()));
    let cwd = base.join("work");
    let log_dir = base.join("logs");
    let _rm = DirGuard::new(base.clone());
    for dir in [&cwd, &log_dir] {
        fs::create_dir_all(dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    }

    let mut spec = mk(guard.id()).into_fields();
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());
    spec.logs = Some(log_dir.join("out.log"));
    let spec = DaemonSpec::new(spec).unwrap();

    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    for dir in [&cwd, &log_dir] {
        let meta = fs::metadata(dir).expect("stat");
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o700,
            "{}: mode must be untouched",
            dir.display()
        );
        assert_eq!(meta.uid(), 0, "{}: owner must be untouched", dir.display());
    }
}

/// The mixed case `ensure_dir`'s own doc comment is about: an ancestor that already exists
/// must never be touched, even when a deeper, freshly-created component under it is.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_leaves_a_preexisting_ancestor_directory_untouched() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let base = std::env::temp_dir().join(format!("{}-ancestor", guard.id()));
    let _rm = DirGuard::new(base.clone());
    fs::create_dir(&base).unwrap_or_else(|e| panic!("mkdir {}: {e}", base.display()));
    fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
    let base_meta_before = fs::metadata(&base).expect("stat base before");

    let cwd = base.join("work");
    let mut spec = mk(guard.id()).into_fields();
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());
    let spec = DaemonSpec::new(spec).unwrap();

    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let base_meta_after = fs::metadata(&base).expect("stat base after");
    assert_eq!(
        base_meta_after.permissions().mode() & 0o777,
        base_meta_before.permissions().mode() & 0o777,
        "the pre-existing ancestor's mode must be untouched"
    );
    assert_eq!(
        base_meta_after.uid(),
        base_meta_before.uid(),
        "the pre-existing ancestor's owner must be untouched"
    );

    let cwd_meta = fs::metadata(&cwd).expect("stat cwd");
    assert!(cwd_meta.is_dir());
    assert_eq!(
        cwd_meta.uid(),
        uid_of("nobody"),
        "the newly-created leaf must be chowned"
    );
}

// Backslashes =========================================================================================================

/// Values that end in backslashes are written so that systemd reads each back exactly, and the
/// directives after them are not swallowed. Read back from the loaded unit, not from the text.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn values_ending_in_backslashes_read_back_from_systemd_exactly() {
    let id = support::random_test_id();
    // Creation order: see the module doc of `support::service_guard`.
    let base = std::env::temp_dir().join(format!("{id}-backslashes"));
    let _rm = DirGuard::new(base.clone());
    let guard = ServiceGuard::new(&id);
    let cwd = PathBuf::from(format!("{}/work\\", base.display()));
    let logs = PathBuf::from(format!("{}/log\\\\\\", base.display()));

    let mut spec = mk(guard.id()).into_fields();
    spec.name = "Backslash name\\".to_string();
    // The shell ignores the arguments after the script, so the unit starts and stays up.
    spec.command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exec sleep infinity".to_string(),
        "C:\\".to_string(),
        "a\\\\b".to_string(),
        " a ".to_string(),
    ];
    spec.cwd = Some(cwd.clone());
    spec.logs = Some(logs);
    spec.env = std::collections::BTreeMap::from([
        ("K".to_string(), "C:\\".to_string()),
        ("L".to_string(), " v ".to_string()),
    ]);
    let spec = DaemonSpec::new(spec).unwrap();

    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    let unit = format!("{}.service", guard.id());
    let show = |property: &str| {
        cmd::run("systemctl", &["show", "--property", property, "--value", &unit])
            .expect_ok()
            .stdout
            .trim_end_matches('\n')
            .to_string()
    };
    assert_eq!(show("Description"), "Backslash name\\");
    assert_eq!(show("WorkingDirectory"), cwd.to_str().unwrap());
    // `show` prints the environment re-quoted, with each backslash doubled.
    // A quoted word keeps its edge whitespace; `show` joins the arguments with one space.
    assert_eq!(show("Environment"), "\"K=C:\\\\\" \"L= v \"");
    assert!(
        show("ExecStart").contains("argv[]=/bin/sh -c exec sleep infinity C:\\ a\\\\b  a  ;"),
        "{}",
        show("ExecStart")
    );
    // The line after the description and the working directory was not swallowed.
    assert_eq!(show("User"), "0");

    // `show` does not print the `append:` path, so start the unit: systemd creates the file at
    // exactly that path before it execs, in the working directory it could enter only if it read
    // that one back exactly too.
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("read {}: {e}", base.display()))
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        [
            std::ffi::OsString::from("log\\\\\\"),
            std::ffi::OsString::from("work\\")
        ]
    );
}

/// systemd takes the executable path as written but substitutes `$FOO`/`${FOO}` and collapses `$$`
/// in argv[0], so a path holding `$` must run the right file and still hand the daemon its own
/// path as argv[0]. The directory also holds a space and a `%`, which quote and double the word.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_executable_path_with_dollars_runs_with_the_path_as_argv0() {
    let id = support::random_test_id();
    let base = std::env::temp_dir().join(format!("{id}-argv0"));
    let _rm = DirGuard::new(base.clone());
    let guard = ServiceGuard::new(&id);
    let dir = base.join("x${y}$$z$w %q");
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    let executable = dir.join("sh");
    std::os::unix::fs::symlink("/bin/sh", &executable).expect("symlink");
    let out = base.join("cmdline");

    let mut spec = mk(guard.id()).into_fields();
    // `mk` restarts on failure: a failing script would restart forever and `start --wait` never return.
    spec.restart = Restart::Never;
    // The shell lists its own argv, one per line, into the file named by its last argument. The
    // trailing `:` stops dash from exec'ing `tr` in the shell's place (it does that to the last
    // command of `-c`), which would make `/proc/$$/cmdline` read `tr`'s argv.
    spec.command = vec![
        executable.to_str().unwrap().to_string(),
        "-c".to_string(),
        "tr '\\000' '\\n' < /proc/$$/cmdline > \"$1\"; :".to_string(),
        "sh".to_string(),
        out.to_str().unwrap().to_string(),
    ];
    let spec = DaemonSpec::new(spec).unwrap();

    Systemd::new().install(&spec, false).expect("install");
    // Blocks until the shell has exited, so the file is complete.
    cmd::run("systemctl", &["start", "--wait", &format!("{}.service", guard.id())]).expect_ok();

    let argv = fs::read_to_string(&out).unwrap_or_else(|e| panic!("read {}: {e}", out.display()));
    assert_eq!(
        argv.lines().next(),
        executable.to_str(),
        "argv[0] of the daemon; all of argv:\n{argv}"
    );
}

// A unit `list` could not read is reported, never omitted =============================================================

/// `list` runs unelevated by design, and a unit shipped non-world-readable (those carrying
/// `LoadCredential=` commonly are 0600) is what it meets. Skipping it certifies "no such daemon" off
/// a read that never happened — the id may be a stranger's *or* goetia's own, and the listing cannot
/// tell which. One unreadable unit still must not take down the listing of every other daemon.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_reports_a_unit_it_cannot_read_as_undetermined() {
    let readable = support::random_test_id();
    let readable_guard = ServiceGuard::new(&readable);
    Systemd::new()
        .install(&mk(readable_guard.id()), false)
        .expect("install");

    let denied = support::random_test_id();
    let denied_guard = ServiceGuard::new(&denied);
    seed_foreign(denied_guard.id());
    fs::set_permissions(unit_path(denied_guard.id()), fs::Permissions::from_mode(0o600)).expect("chmod 0600");

    let listed = support::list_json(run_unelevated);

    // Root reads this fragment fine and finds no marker, which reports nothing at all — so a run
    // whose reader was not unprivileged fails here instead of passing vacuously.
    assert!(
        listed.ids("undetermined").contains(&Some(denied.clone())),
        "{}",
        listed.context
    );
    assert!(
        !listed.ids("daemons").contains(&Some(denied.clone())),
        "goetia never read the marker, so it cannot list the id as one of its own: {}",
        listed.context
    );
    assert!(
        listed.ids("daemons").contains(&Some(readable.clone())),
        "one unreadable unit must not take down the listing: {}",
        listed.context
    );
}

/// The same denial at the same privilege boundary, over a unit goetia *does* own. The end-to-end
/// proof that an unelevated `daemon list` on a populated host stops exiting `0` with the daemon
/// missing from its document.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_list_reports_a_root_only_unit_as_undetermined() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    Systemd::new().install(&mk(guard.id()), false).expect("install");
    fs::set_permissions(unit_path(guard.id()), fs::Permissions::from_mode(0o600)).expect("chmod 0600");

    let listed = support::list_json(run_unelevated);

    // Root reads it, decodes the marker and lists it under `daemons`, so this cannot pass vacuously
    // either.
    assert!(
        listed.ids("undetermined").contains(&Some(id.clone())),
        "{}",
        listed.context
    );
    assert!(
        !listed.ids("daemons").contains(&Some(id.clone())),
        "a fragment goetia could not open is not a daemon it can report the state of: {}",
        listed.context
    );
}

/// The sibling half of the same property, on the path `residue` exists for: **the fragment is not
/// the id**. An `<id>.service.d` goetia cannot read, with no `<id>.service` at all, is an id whose
/// classification never completed — `status` says exactly that, exit `4` — so a listing that
/// enumerates fragments alone leaves it out and lets `show` answer "is not installed", a negative
/// drawn from a read that established nothing.
///
/// Seeded as a *regular file* where the directory belongs: `read_dir` answers `ENOTDIR` for every
/// uid, root's included, so this holds in an elevated binary rather than only below the privilege
/// boundary.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_reports_an_id_whose_dropin_it_cannot_read_with_no_fragment_at_all() {
    let id = support::random_test_id();
    let _cleanup = seed_unreadable_dropin_enotdir(&id);
    assert!(!unit_path(&id).exists(), "the fragment must really be absent");

    let listed = Systemd::new()
        .list()
        .expect("one unreadable drop-in must not take down the whole listing");

    assert!(
        listed.iter().any(|entry| matches!(
            entry,
            Installed::Undetermined { name: Some(name), .. } if name == &id
        )),
        "the read that would have classified {id} never completed, so omitting it from the \
         enumeration would say it is not there: {listed:?}"
    );
    assert!(
        !listed
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == id)),
        "nothing was read, so nothing may be claimed: {listed:?}"
    );

    // The two must describe one filesystem state identically — the divergence this closes was
    // `status` answering `4` for the same id `list` left out entirely.
    let err = Systemd::new()
        .status(&Id::try_from(id.clone()).expect("valid id"))
        .expect_err("a drop-in that could not be read leaves the id unclassified");
    assert!(matches!(err, goetia::Error::Undetermined { .. }), "{err:?}");
}

/// `scan_host`'s cross-root pass, and the dedup that pass makes necessary — both through a real
/// `list()`.
///
/// `residue` counts an `<id>.service.d` under **any** of the twelve search roots, so an id whose
/// only trace is one there is an id `status` already answers `4` for. A scan that enumerated
/// `UNIT_DIR` alone never named that id, and `list` therefore omitted it — the negative conclusion
/// `Installed::Undetermined` exists to forbid, and the exact divergence between the two verbs this
/// branch removes. `list_reports_an_id_whose_dropin_it_cannot_read_with_no_fragment_at_all` seeds
/// under `UNIT_DIR`, so it passes with the cross-root pass deleted; this one does not.
///
/// The second half is what the first makes possible: once two roots can name one id, the
/// `BTreeSet` is all that keeps it to a single entry, and `cli::support::partition_installed`
/// debug-asserts one entry per id.
///
/// `/etc/systemd/system.attached` because the two `.control` roots are already taken by
/// `a_control_dropin_is_drift` and `uninstall_refuses_an_id_whose_only_artifact_is_a_control_dropin`
/// — no two tests share a root to clean up. A regular *file* where the directory belongs, so
/// `read_dir` answers `ENOTDIR`: no privilege dissolves that, which is what leaves the id
/// unclassifiable to the elevated binary CI runs.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_names_an_id_whose_only_dropin_is_under_another_search_root() {
    let id = support::random_test_id();
    let mgr = Systemd::new();

    // Non-vacuity: nothing is seeded yet, so this id is absent from the listing and no aggregate
    // stands for it either.
    assert!(
        !may_account_for(&mgr.list().expect("list"), &id),
        "the id must start out unaccounted for, or the assertion below proves nothing"
    );

    let (elsewhere_leaf, _elsewhere) = seed_unreadable_dropin_in_root("/etc/systemd/system.attached", &id);
    assert!(!unit_path(&id).exists(), "the fragment must really be absent");
    assert!(
        !dropin_dir(&id).exists(),
        "and so must anything under the unit directory itself"
    );

    let entries = undetermined_for(&mgr.list().expect("list"), &id);
    match entries.as_slice() {
        [reason] => assert!(
            reason.contains(&elsewhere_leaf.display().to_string()),
            "the entry must name the read that did not complete: {reason}"
        ),
        other => panic!("an id named only outside `UNIT_DIR` is one undetermined entry, not {other:?}"),
    }

    // The same id, now named by two roots at once: still one entry.
    let _own = seed_unreadable_dropin_enotdir(&id);

    let entries = undetermined_for(&mgr.list().expect("list"), &id);
    assert_eq!(entries.len(), 1, "two roots naming one id is still one id: {entries:?}");
}

/// The `reason` of every named `undetermined` entry `listed` carries for `id`.
fn undetermined_for(listed: &[Installed], id: &str) -> Vec<String> {
    listed
        .iter()
        .filter_map(|entry| match entry {
            Installed::Undetermined { name, reason } if name.as_deref() == Some(id) => Some(reason.to_string()),
            _ => None,
        })
        .collect()
}

/// The same defect at the privilege boundary where it is routine — an administrator's drop-in
/// shipped `0700` for a unit that lives in `/usr/lib`, met by an unelevated `list` — and stated as
/// the CLI answers it: `show` may never call an id absent on a listing that did not establish
/// absence.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn unelevated_show_cannot_call_absent_an_id_whose_dropin_it_could_not_read() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_unreadable_dropin(guard.id());
    assert!(!unit_path(guard.id()).exists(), "the fragment must really be absent");

    let listed = support::list_json(run_unelevated);

    // Root reads this drop-in fine and reports nothing at all, so a run whose reader was not
    // unprivileged fails here instead of passing vacuously.
    assert!(
        listed.ids("undetermined").contains(&Some(id.clone())),
        "{}",
        listed.context
    );

    let shown = run_unelevated(&["daemon", "show", &id]);
    assert_eq!(
        shown.status.code(),
        Some(4),
        "`not installed` (exit 1) is the negative this listing cannot support: stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&shown.stdout),
        String::from_utf8_lossy(&shown.stderr)
    );
}

/// A *named* `undetermined` entry blocks exactly one negative answer: the one about the id it
/// names. `Installed::Undetermined`'s null-name rule forbids concluding absence while an entry
/// stands for ids it could not separate — and an entry that names its id separates it, so every
/// other id on the host is still answerable.
///
/// The runnable half of the same defect this branch fixes on Windows, where an aggregate entry
/// standing for one denied service dropped the name it was holding and made `show` answer "could
/// not be determined" for every id on the host, permanently, over one stranger's ACL.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_named_undetermined_entry_blocks_only_its_own_id() {
    let denied = support::random_test_id();
    let denied_guard = ServiceGuard::new(&denied);
    seed_foreign(denied_guard.id());
    fs::set_permissions(unit_path(denied_guard.id()), fs::Permissions::from_mode(0o600)).expect("chmod 0600");

    // Never installed, and nothing on this host is at it.
    let absent = support::random_test_id();
    assert!(!unit_path(&absent).exists(), "the absent id must really be absent");

    // Non-vacuity: the unreadable unit really does reach this reader as a named entry. Without it
    // the assertion below would pass on a host with no `undetermined` entry at all.
    let listed = support::list_json(run_unelevated);
    assert!(
        listed.ids("undetermined").contains(&Some(denied.clone())),
        "{}",
        listed.context
    );

    let blocked = run_unelevated(&["daemon", "show", &denied]);
    assert_eq!(
        blocked.status.code(),
        Some(4),
        "the id the entry names is the one that cannot be answered: stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&blocked.stdout),
        String::from_utf8_lossy(&blocked.stderr)
    );

    let answerable = run_unelevated(&["daemon", "show", &absent]);
    assert_eq!(
        answerable.status.code(),
        Some(1),
        "a named entry stands for its own id and no other, so `not installed` is still sound here: \
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&answerable.stdout),
        String::from_utf8_lossy(&answerable.stderr)
    );
}

/// One `*.service` whose bytes are not UTF-8 must neither make `list` return `Err` — taking down
/// the listing of every daemon on the host over a file that belongs to none of them — nor leave a
/// standing `undetermined` entry, which is the same regression one exit code further on. The bytes
/// were obtained: goetia writes UTF-8 ini and nothing else, so this file is positively not goetia's
/// and is omitted exactly like an unmarked one. A foreign `Description=Café` saved as Latin-1
/// otherwise gives the whole host a permanent exit `4`.
///
/// The accepted cost, stated where it is paid: a fragment goetia *did* write and something later
/// corrupted now reads as a stranger's. Ownership lives in the marker, and the marker is in the
/// bytes that would not decode.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_omits_a_non_utf8_unit_as_foreign_instead_of_reporting_it() {
    let readable = support::random_test_id();
    let readable_guard = ServiceGuard::new(&readable);
    Systemd::new()
        .install(&mk(readable_guard.id()), false)
        .expect("install");

    let undecodable = support::random_test_id();
    let undecodable_guard = ServiceGuard::new(&undecodable);
    fs::write(unit_path(undecodable_guard.id()), NON_UTF8_UNIT).expect("seed a non-UTF-8 unit");

    let listed = Systemd::new()
        .list()
        .expect("one undecodable file must not take down the whole listing");

    assert!(
        !may_account_for(&listed, undecodable_guard.id()),
        "bytes that are not the UTF-8 goetia writes are foreign, so the id is neither claimed nor \
         left undetermined: {listed:?}"
    );
    support::assert_own(&listed, &[readable_guard.id()]);

    // The same file through `status`, so the two cannot describe one machine differently. `Foreign`
    // and not `NotInstalled`: something is demonstrably there.
    let err = Systemd::new()
        .status(&Id::try_from(undecodable.clone()).expect("valid id"))
        .expect_err("a fragment goetia did not write is not a daemon it can report the state of");
    assert!(
        matches!(err, goetia::Error::Foreign { .. }),
        "presence is established and ownership is refused: {err:?}"
    );
}

/// `open(2)` on a FIFO with `O_RDONLY` blocks until a writer arrives. `list` opens every
/// `*.service` name in the unit directory, so classifying by type before opening for reading is
/// what keeps one `mkfifo` from wedging the listing for the whole host.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_does_not_hang_on_a_fifo_in_the_unit_directory() {
    let id = support::random_test_id();
    let path = unit_path(&id);
    let _cleanup = FileGuard::new(path.clone());
    mkfifo(&path);

    let listed = without_blocking("Systemd::list", || Systemd::new().list()).expect("list");

    assert!(
        !may_account_for(&listed, &id),
        "a FIFO is not a fragment, and nothing about it was left undetermined: {listed:?}"
    );
}

/// Run `goetia <args>` as an unprivileged user.
///
/// The binary is copied to a world-readable temp path first. Running it in
/// place fails on CI with `runuser: failed to execute ...: Permission denied`,
/// because `nobody` cannot traverse the runner's `/home/runner/work/...`
/// checkout — a directory-permission problem several levels above the binary,
/// not something `chmod` on the binary alone fixes.
///
/// `runuser` (util-linux), not `sudo`: invoked by root it never prompts, and
/// it has none of `sudo`'s `requiretty`/PAM-session pitfalls when spawned from
/// a test process with no controlling terminal.
fn run_unelevated(args: &[&str]) -> std::process::Output {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Per call, not per process: three tests stage a binary now, and skuld runs them concurrently —
    // two sharing one path would have each `remove_file` the copy the other was still executing.
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staged = std::env::temp_dir().join(format!("goetia-unelevated-{}-{n}", std::process::id()));
    let _staged = FileGuard::new(staged.clone());
    fs::copy(env!("CARGO_BIN_EXE_goetia"), &staged).expect("stage the binary somewhere traversable");
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).expect("chmod 0755");

    let mut argv = vec!["-u", "nobody", "--", staged.to_str().expect("utf-8 temp path")];
    argv.extend_from_slice(args);
    Command::new("runuser")
        .args(&argv)
        .output()
        .expect("spawn runuser -u nobody")
}
