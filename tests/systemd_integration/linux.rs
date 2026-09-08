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

/// The same drop-in, in a directory no unprivileged process can open. Mode `000` on a directory the
/// (root) test process owns is enough: `EACCES` applies to every uid but root's, and root is exactly
/// the uid the reader in these tests is not.
fn write_unreadable_dropin(id: &str) {
    write_dropin(id);
    fs::set_permissions(dropin_dir(id), fs::Permissions::from_mode(0o000)).expect("chmod 0000");
}

/// Seed one `*.conf` into an arbitrary drop-in directory and hand back the RAII removal of it. The
/// parent search root is left exactly as found: `/etc/systemd/system` is shared with every other
/// test running concurrently.
fn seed_dropin(dir: &Path) -> RmDropin {
    fs::create_dir_all(dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    fs::write(dir.join("50-x.conf"), "[Service]\nMemoryMax=8G\n").expect("write drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
    RmDropin {
        leaf: dir.to_path_buf(),
        root: None,
    }
}

/// The same, under one of the two `.control` roots — where `systemctl set-property UNIT
/// PROPERTY=VALUE` writes, and the two highest-precedence entries on the system unit search path,
/// both above `/etc/systemd/system`.
///
/// The root is removed along with the drop-in. Each test that seeds one takes a *different* root, so
/// no concurrently running test can be inside the one this removes — and the removal is
/// `remove_dir`, so a root systemd itself populated for some other unit fails to go and is left as
/// it was found.
fn seed_control_dropin(root: &str, id: &str) -> (PathBuf, RmDropin) {
    let leaf = PathBuf::from(root).join(format!("{id}.service.d"));
    let mut guard = seed_dropin(&leaf);
    guard.root = Some(PathBuf::from(root));
    (leaf, guard)
}

/// A manifest at a path an unprivileged process can actually reach: `tempfile`'s own directory is
/// 0700, which `runuser -u nobody` cannot traverse.
fn world_readable_manifest(id: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod the tempdir 0755");
    let path = dir.path().join("goetia.yaml");
    fs::write(
        &path,
        format!("daemons:\n  {id}:\n    command: [\"/bin/sleep\", \"infinity\"]\n"),
    )
    .expect("write manifest");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod the manifest 0644");
    (dir, path)
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

/// RAII removal of a seeded drop-in directory, and of the search root itself where the test created
/// that root — see [`seed_control_dropin`].
struct RmDropin {
    leaf: PathBuf,
    root: Option<PathBuf>,
}

impl Drop for RmDropin {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.leaf);
        if let Some(root) = &self.root {
            let _ = fs::remove_dir(root);
        }
        // No `daemon-reload` here: every test that seeds one of these also holds a `ServiceGuard`,
        // whose own cleanup reloads after this one has run.
    }
}

/// RAII removal of a single path, symlink included — `remove_file` unlinks a symlink rather than
/// following it, which is what the dangling `.wants` link the residue tests plant needs.
struct RmPath(PathBuf);

impl Drop for RmPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Drive `daemon uninstall` through the CLI rather than calling `Systemd::uninstall` directly: the
/// thing under test is the exit code a shell sees, and `IdVerbCall::absent_is_success` turns
/// `Error::NotInstalled` — and only that variant — into `0` and "nothing to do" on the way there.
fn uninstall_via_cli(id: &str) -> (i32, String, String) {
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

// Residual artifacts: absence is about the id, not about the fragment file — obligation 7 =============================

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

    assert_ne!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("not installed (nothing to do)"), "{out}");
    assert!(
        dropin_dir(guard.id()).exists(),
        "and the drop-in survives: goetia cannot tell its own leftover from an administrator's \
         override of a unit shipped in /usr/lib, so it removes neither"
    );
}

/// The same for the other artifact that outlives the fragment. `systemctl disable` is impossible
/// once the `[Install]` section is gone (obligation 6), so this link keeps the id enrolled at boot
/// — the very reason the exit-code table gives for `disable` returning `1` here.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_refuses_an_id_whose_fragment_is_gone_but_whose_enablement_link_remains() {
    let id = support::random_test_id();
    let link = wants_symlink(&id);
    fs::create_dir_all(link.parent().expect("the link has a parent")).expect("mkdir *.target.wants");
    std::os::unix::fs::symlink(unit_path(&id), &link).expect("plant a dangling .wants link");
    let _cleanup = RmPath(link.clone());

    let (code, out, err) = uninstall_via_cli(&id);

    assert_ne!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(!out.contains("not installed (nothing to do)"), "{out}");
    assert!(fs::symlink_metadata(&link).is_ok(), "and the link survives, unremoved");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_a_stray_enablement_link_with_no_fragment() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let link = wants_symlink(guard.id());
    fs::create_dir_all(link.parent().expect("the link has a parent")).expect("mkdir *.target.wants");
    std::os::unix::fs::symlink(unit_path(guard.id()), &link).expect("plant a dangling .wants link");
    let _cleanup = RmPath(link.clone());

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

// Obligation 7: a read that failed establishes no ownership ===========================================================

/// A drop-in directory the caller cannot open leaves goetia unable to say whether *anything* is
/// installed at the id. Reporting `unreadable` for it — "goetia owns the id but cannot report on
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

/// The same absence path `status` takes above, reached through `discover` instead: with no fragment,
/// `residue`'s drop-in scan is what would have answered "is anything at this id", and it did not
/// complete. `4`, not the `1` `diff`'s catch-all `Err` arm gives: it was asked a question and could
/// not determine the answer — the same reasoning that already puts `Outcome::RefuseUnreadable` at
/// `4` there.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn diff_reports_a_dropin_directory_it_cannot_read_as_indeterminate() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    write_unreadable_dropin(guard.id());
    let (_tempdir, manifest) = world_readable_manifest(guard.id());

    let output = run_unelevated(&[
        "daemon",
        "diff",
        "-f",
        manifest.to_str().expect("a UTF-8 temp path"),
        guard.id(),
    ]);
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
/// ability to report on the id, which is `Outcome::RefuseUnreadable`.
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
    fs::write(&blocker, "").expect("plant a regular file where the drop-in directory would be");
    let _cleanup = RmPath(blocker.clone());

    let outcome = mgr
        .install(&mk(guard.id()), false)
        .expect("a drop-in read that failed is not a failure to determine whether the id is installed");
    let Outcome::RefuseUnreadable { reason, .. } = &outcome else {
        panic!("goetia owns this id and cannot report on it, got {outcome:?}");
    };
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

    let output = run_unelevated(&[
        "daemon",
        "diff",
        "-f",
        manifest.to_str().expect("a UTF-8 temp path"),
        guard.id(),
    ]);
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
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
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

// Obligation 3: every drop-in systemd reads is drift ==================================================================

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
    assert_eq!(code, 0, "an empty id is `nothing to do`");

    // The `/run` root this time, so the two `.control` tests never share a directory to clean up.
    let (dir, _cleanup) = seed_control_dropin("/run/systemd/system.control", guard.id());

    let (code, out, err) = uninstall_via_cli(guard.id());
    let context = format!("stdout:\n{out}\nstderr:\n{err}");
    assert_ne!(
        code, 0,
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
    let _cleanup = seed_dropin(&dir);

    let outcome = mgr.preview_install(&mk(guard.id())).expect("preview");
    assert_eq!(
        outcome,
        Outcome::UpToDate,
        "{} is a sibling of this id's artifact, not part of it",
        dir.display()
    );

    // And the same id's *own* drop-in still is drift, so this is a boundary, not a hole — and one
    // `--force` does resolve, since `/etc/systemd/system/<id>.service.d` is goetia's to clear.
    let _own = seed_dropin(&dropin_dir(guard.id()));
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
    let _cleanup = RmDropin {
        leaf: dir.clone(),
        root: None,
    };
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
        Installed::Undetermined { name, .. } => name.as_deref() == Some(guard.id()),
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

    let output = run_unelevated(&["daemon", "list"]);
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

    let output = run_unelevated(&["daemon", "list"]);
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
    fs::copy(env!("CARGO_BIN_EXE_goetia"), &staged).expect("stage the binary somewhere traversable");
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).expect("chmod 0755");

    let mut argv = vec!["-u", "nobody", "--", staged.to_str().expect("utf-8 temp path")];
    argv.extend_from_slice(args);
    let output = Command::new("runuser")
        .args(&argv)
        .output()
        .expect("spawn runuser -u nobody");
    let _ = fs::remove_file(&staged);
    output
}
