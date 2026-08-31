//! The actual test bodies — see `tests/systemd_integration.rs` for why this lives in its own file
//! rather than an inline `mod linux { ... }`: an inline module has no real backing directory, so a
//! `#[path]`-relative import from inside it (to reach the shared `tests/support/mod.rs`) cannot
//! `../`-escape it — POSIX path resolution needs every intermediate component, including `linux`
//! itself, to exist as a real directory entry, not just resolve lexically. A real file avoids the
//! problem entirely, matching `tests/marker_inertness.rs`'s own structure.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use goetia::backend::systemd::manager::Systemd;
use goetia::decide::Outcome;
use goetia::manager::{Installed, ServiceManager, State, conformance};
use goetia::spec::{DaemonSpec, Id, Kind, Restart, User};

use crate::support::{self, ELEVATED, ServiceGuard, cmd};

// Fixtures ============================================================================================================

/// A minimal, real, long-running daemon: `sleep infinity` exists on every coreutils Ubuntu ships.
fn mk(id: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id.to_string()).expect("valid id"),
        name: id.to_string(),
        command: vec!["/bin/sleep".to_string(), "infinity".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

fn unit_path(id: &str) -> PathBuf {
    PathBuf::from(support::SYSTEMD_UNIT_DIR).join(format!("{id}.service"))
}

fn dropin_dir(id: &str) -> PathBuf {
    PathBuf::from(support::SYSTEMD_UNIT_DIR).join(format!("{id}.service.d"))
}

fn wants_symlink(id: &str) -> PathBuf {
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

fn load_from_temp_manifest(id: &str) -> DaemonSpec {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("goetia.yaml");
    fs::write(
        &path,
        format!("daemons:\n  {id}:\n    command: [\"/bin/sleep\", \"infinity\"]\n"),
    )
    .expect("write manifest");
    let (specs, _warnings) = goetia::spec::load(&path).expect("load manifest");
    // `dir` (and the manifest inside it) is dropped here, before the caller ever installs the
    // spec - the whole point of `install_then_show_round_trips_without_the_source_file`.
    specs
        .into_iter()
        .find(|s| s.id.as_str() == id)
        .expect("spec present in manifest")
}

fn find_ours(installed: Vec<Installed>, id: &str) -> Option<DaemonSpec> {
    installed.into_iter().find_map(|entry| match entry {
        Installed::Ours { spec, .. } if spec.id.as_str() == id => Some(spec),
        _ => None,
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

struct RmDirAll(PathBuf);

impl Drop for RmDirAll {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Step 1: conformance =================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn systemd_passes_conformance() {
    let mgr = Systemd::new();

    // The two ids `conformance::run` cannot produce through the trait's own methods - see its
    // module doc comment. `run` cleans up `HAND_EDITED_ID` itself; `FOREIGN_ID` is ours.
    let foreign_guard = ServiceGuard::new(conformance::FOREIGN_ID);
    seed_foreign(foreign_guard.id());

    mgr.install(&mk(conformance::HAND_EDITED_ID), false)
        .expect("seed hand-edited install");
    hand_edit(conformance::HAND_EDITED_ID);

    conformance::run(&mgr, &mk);
}

// Step 2: obligation-specific scenarios ===============================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
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
        Outcome::Conflict { artifact_diff } => {
            assert!(artifact_diff.contains("MemoryMax=8G"), "{artifact_diff}");
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

    mgr.start(&spec.id).expect("start");
    let status = mgr.status(&spec.id).expect("status after start");
    assert_eq!(status.state, State::Running, "{status:?}");
    assert!(status.pid.is_some(), "{status:?}");

    mgr.stop(&spec.id).expect("stop");
    let status = mgr.status(&spec.id).expect("status after stop");
    assert_ne!(status.state, State::Running, "{status:?}");
    assert!(status.pid.is_none(), "{status:?}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_leaves_nothing() {
    let id = support::random_test_id();
    // No `ServiceGuard`: this test is itself the proof that nothing is left for one to clean up.
    let spec = mk(&id);
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&spec.id).expect("enable");
    mgr.start(&spec.id).expect("start");
    write_dropin(&id);

    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(!unit_path(&id).exists(), "fragment must be gone");
    assert!(!dropin_dir(&id).exists(), "drop-in directory must be gone");
    assert!(
        fs::symlink_metadata(wants_symlink(&id)).is_err(),
        "the `.wants` symlink must be gone"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn list_ignores_foreign_units() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());

    let mgr = Systemd::new();
    let listed = mgr.list().expect("list");
    let present = listed.into_iter().any(|entry| match entry {
        Installed::Ours { spec, .. } => spec.id.as_str() == guard.id(),
        Installed::OursUnreadable { name, .. } => name == guard.id(),
    });
    assert!(!present, "a foreign unit must not appear in list()");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn unelevated_list_works() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk(guard.id());
    let mgr = Systemd::new();
    mgr.install(&spec, false).expect("install");

    // `runuser` (util-linux), not `sudo`: it never asks for a password when invoked by root, and
    // unlike `sudo` it has no `requiretty`/PAM-session pitfalls to trip over when spawned from a
    // test process with no controlling terminal.
    let output = Command::new("runuser")
        .args(["-u", "nobody", "--", env!("CARGO_BIN_EXE_goetia"), "daemon", "list"])
        .output()
        .expect("spawn runuser -u nobody");
    assert!(
        output.status.success(),
        "unelevated `daemon list` failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(guard.id()), "stdout:\n{stdout}");
}

// Obligation 5: parent directories ====================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_creates_and_owns_fresh_cwd_and_logs_dirs_for_a_named_user() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let base = std::env::temp_dir().join(format!("{}-parent-dirs", guard.id()));
    let cwd = base.join("work");
    let logs = base.join("logs").join("out.log");
    let _rm = RmDirAll(base.clone());

    let mut spec = mk(guard.id());
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());
    spec.logs = Some(logs.clone());

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
fn install_refuses_a_preexisting_cwd_the_target_account_cannot_write() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let cwd = std::env::temp_dir().join(format!("{}-preexisting-cwd", guard.id()));
    fs::create_dir(&cwd).unwrap_or_else(|e| panic!("mkdir {}: {e}", cwd.display()));
    fs::set_permissions(&cwd, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let _rm = RmDirAll(cwd.clone());

    let mut spec = mk(guard.id());
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());

    let mgr = Systemd::new();
    let err = mgr
        .install(&spec, false)
        .expect_err("install must refuse a cwd the target account cannot write");
    assert!(err.to_string().contains("not writable"), "{err}");

    let meta = fs::metadata(&cwd).expect("stat");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "a pre-existing directory's mode must not be widened, even on refusal"
    );
    assert_eq!(
        meta.uid(),
        0,
        "a pre-existing directory's owner must not be reassigned, even on refusal"
    );
}

/// The mixed case `ensure_writable_dir`'s own doc comment is about: an ancestor that already exists
/// must never be touched, even when a deeper, freshly-created component under it is.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_leaves_a_preexisting_ancestor_directory_untouched() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let base = std::env::temp_dir().join(format!("{}-ancestor", guard.id()));
    fs::create_dir(&base).unwrap_or_else(|e| panic!("mkdir {}: {e}", base.display()));
    fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
    let _rm = RmDirAll(base.clone());
    let base_meta_before = fs::metadata(&base).expect("stat base before");

    let cwd = base.join("work");
    let mut spec = mk(guard.id());
    spec.user = User::Name("nobody".to_string());
    spec.cwd = Some(cwd.clone());

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

// Obligation 4/2: reading an unreadable foreign unit must not abort list() ============================================

/// `list` runs unelevated by design; a foreign unit shipped non-world-readable (units carrying
/// `LoadCredential=` commonly are 0600) must be silently skipped, not treated as an error that takes
/// down the whole listing.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn list_skips_a_foreign_unit_unreadable_to_the_caller() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());
    fs::set_permissions(unit_path(guard.id()), fs::Permissions::from_mode(0o600)).expect("chmod 0600");

    let output = Command::new("runuser")
        .args(["-u", "nobody", "--", env!("CARGO_BIN_EXE_goetia"), "daemon", "list"])
        .output()
        .expect("spawn runuser -u nobody");
    assert!(
        output.status.success(),
        "unelevated `daemon list` must still succeed with an unreadable foreign unit present:\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(guard.id()),
        "an unreadable foreign unit must not appear in the listing:\n{stdout}"
    );
}
