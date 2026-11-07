//! The stock shim's provisioning, against the real file system and SCM: `EmbeddedStockShim` places
//! the shim this crate builds, and `Scm::install` repairs, refuses or accepts what it finds.
//!
//! Everything here writes under `%ProgramFiles%` or sets an owner (Administrators), which needs an
//! elevated token, and runs on CI only. The tests that use the default folder own
//! `%ProgramFiles%\Goetia` for their length: each empties it first and removes it after, and
//! `UNIT_DIR_EXCLUSIVE` keeps them from running beside each other.

use std::path::{Path, PathBuf};
use std::process::Command;

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{Budget, ServiceManager as _, State};
use goetia::shim::{CheckPermissions, ShimProvider as _, StandaloneShim};
use goetia::spec::{DaemonSpec, User};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

use crate::common::{
    LogDir, ShimGuard, UNIT_DIR_EXCLUSIVE, embedded_manager, embedded_shim, fixture_command, fixture_command_with_exe,
    shim_bytes, simple_spec,
};
use crate::install_helper::INSTALL_AS;
use crate::support::cleanup::{self, DirGuard};
use crate::support::{self, ConnectBack, ELEVATED, LocalUserGuard, WorldReadableExe};

// Folders and files ===================================================================================================

/// `%ProgramFiles%\Goetia`, emptied of an earlier run's leftovers, and removed when the guard drops.
/// Made before anything is written into it.
fn default_folder() -> (PathBuf, DirGuard) {
    let exe = embedded_shim(None).preview().expect("locate the default folder").exe;
    let folder = exe.parent().expect("the shim has a folder").to_path_buf();
    cleanup::remove_or_warn(&folder, true);
    let guard = DirGuard::new(folder.clone());
    (folder, guard)
}

/// A temp folder the test owns, and a folder two levels into it that `provide` must create.
fn temp_shim_folder() -> (DirGuard, PathBuf) {
    let root = DirGuard::new(tempfile::tempdir().expect("tempdir for the shim folder").keep());
    let folder = root.path().join("shims").join("here");
    (root, folder)
}

/// Where the stock shim goes in `folder`.
fn shim_in(folder: &Path) -> PathBuf {
    embedded_shim(Some(folder.to_path_buf()))
        .preview()
        .expect("preview")
        .exe
}

/// The shim files `folder` holds that a repair set aside or a crash left half written.
fn stale_files(folder: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(folder)
        .unwrap_or_else(|e| panic!("read {}: {e}", folder.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_lowercase();
            name.starts_with("goetia-shim-") && name.ends_with(".stale.exe")
        })
        .collect()
}

/// Grants `Users` the right to change `folder`'s permissions, which the folder check refuses, and
/// takes the grant back on drop.
struct Doctored(PathBuf);

impl Doctored {
    const USERS: &'static str = "*S-1-5-32-545";

    fn new(folder: &Path) -> Self {
        let doctored = Self(folder.to_path_buf());
        let path = folder.to_string_lossy();
        support::cmd::run("icacls", &[&path, "/grant", &format!("{}:(WDAC)", Self::USERS)]).expect_ok();
        doctored
    }
}

impl Drop for Doctored {
    fn drop(&mut self) {
        let path = self.0.to_string_lossy();
        cleanup::warn_left_behind(support::cmd::run_cleanup(
            "icacls",
            &[&[&path, "/remove:g", Self::USERS]],
            &[],
        ));
    }
}

// Reading permissions =================================================================================================

const FILE_SDDL: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;WD)";
const FOLDER_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;WD)";

/// The owner and DACL of `descriptor` as SDDL.
fn sddl_of(descriptor: PSECURITY_DESCRIPTOR) -> String {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `descriptor` is live, and `text` is a valid out-pointer.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "write a descriptor as SDDL: {}", std::io::Error::last_os_error());
    let mut len = 0;
    // SAFETY: `text` is a NUL-terminated string that stays live until it is freed below.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the NUL were just read; `text` was allocated by the call
    // above and is freed once.
    unsafe {
        let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        sddl
    }
}

/// The owner and DACL of the file or folder `path`, as SDDL.
fn security_of(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt as _;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; the out-pointer for the descriptor is a valid lvalue and
    // the others are not asked for.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, 0, "read the permissions of {}: error {status}", path.display());
    let sddl = sddl_of(descriptor);
    // SAFETY: allocated by `GetNamedSecurityInfoW`, freed once, after the last read.
    unsafe { LocalFree(descriptor) };
    sddl
}

/// `sddl` as the system writes it, so that two spellings of one descriptor are equal.
fn normalised(sddl: &str) -> String {
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; the descriptor out-pointer is valid and the size is not asked for.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "read `{sddl}` as SDDL: {}", std::io::Error::last_os_error());
    let normal = sddl_of(descriptor);
    // SAFETY: allocated by the call above, freed once, after the last read.
    unsafe { LocalFree(descriptor) };
    normal
}

/// Fails unless `path` is owned by Administrators, has a protected DACL, and has exactly the entries
/// of `expected`.
#[track_caller]
fn assert_security(path: &Path, expected: &str) {
    let split = |sddl: &str| {
        let (owner, dacl) = sddl.split_once("D:").unwrap_or_else(|| panic!("no DACL in `{sddl}`"));
        let (flags, entries) = dacl.split_at(dacl.find('(').unwrap_or(dacl.len()));
        (owner.to_string(), flags.to_string(), entries.to_string())
    };
    let actual = security_of(path);
    let (owner, flags, entries) = split(&actual);
    let (want_owner, _, want_entries) = split(&normalised(expected));
    assert_eq!(owner, want_owner, "owner of {}: `{actual}`", path.display());
    assert!(
        flags.contains('P'),
        "the DACL of {} is not protected: `{actual}`",
        path.display()
    );
    assert_eq!(entries, want_entries, "entries of {}: `{actual}`", path.display());
}

#[track_caller]
fn assert_stock_file(path: &Path) {
    assert_security(path, FILE_SDDL);
}

#[track_caller]
fn assert_stock_folder(path: &Path) {
    assert_security(path, FOLDER_SDDL);
}

// Provisioning into a folder the test owns ============================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn provide_creates_the_file_with_the_protected_dacl() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    mgr.install(&spec, false).expect("install");

    let file = shim_in(&folder);
    assert_eq!(std::fs::read(&file).expect("read the placed shim"), shim_bytes());
    assert_stock_file(&file);
    // The folders `provide` made, both of them.
    assert_stock_folder(&folder);
    assert_stock_folder(folder.parent().unwrap());
    assert!(stale_files(&folder).is_empty());
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn provide_is_idempotent() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = shim_in(&folder);
    let placed = std::fs::metadata(&file).unwrap().modified().unwrap();

    let again = mgr.install(&spec, false).expect("install again");

    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), placed);
    assert!(stale_files(&folder).is_empty());
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn provide_repairs_wrong_bytes() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = shim_in(&folder);
    std::fs::write(&file, b"not the shim").expect("overwrite the shim");

    mgr.install(&spec, false).expect("install over a wrong file");

    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
    let aside = stale_files(&folder);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(std::fs::read(&aside[0]).unwrap(), b"not the shim");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn provide_replaces_a_loose_dacl() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = shim_in(&folder);
    let path = file.to_string_lossy();
    support::cmd::run("icacls", &[&path, "/grant", "*S-1-5-32-545:(F)"]).expect_ok();

    mgr.install(&spec, false).expect("install over a loose file");

    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
    let aside = stale_files(&folder);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(
        security_of(&aside[0]).contains(";;;BU)"),
        "the file set aside is the loose one: {}",
        security_of(&aside[0])
    );
}

// Provisioning into %ProgramFiles%\Goetia =============================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_provisions_under_program_files() {
    let (folder, _folder_guard) = default_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let started = ConnectBack::listen();
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);

    mgr.install(&spec, false).expect("install into the default folder");

    let file = embedded_shim(None).preview().unwrap().exe;
    assert_eq!(file.parent(), Some(folder.as_path()));
    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in under the shim placed in Program Files");
    assert_eq!(mgr.status(&spec.id).expect("status").state, State::Running);
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_repairs_deleted_shim() {
    let (_folder, _folder_guard) = default_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = embedded_shim(None).preview().unwrap().exe;
    std::fs::remove_file(&file).expect("delete the shim");

    let again = mgr.install(&spec, false).expect("install again");

    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_repairs_tampered_shim() {
    let (folder, _folder_guard) = default_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = embedded_shim(None).preview().unwrap().exe;
    std::fs::write(&file, b"not the shim").expect("overwrite the shim");

    let again = mgr.install(&spec, false).expect("install again");

    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
    let aside = stale_files(&folder);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(std::fs::read(&aside[0]).unwrap(), b"not the shim");
}

// The folder check ====================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_refuses_doctored_folder() {
    let (folder, _folder_guard) = default_folder();
    std::fs::create_dir_all(&folder).expect("create the default folder");
    let _doctored = Doctored::new(&folder);
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    match mgr.install(&spec, false) {
        Err(Error::ShimFolderUnsafe { path, right, .. }) => {
            assert_eq!(path, folder);
            assert_eq!(right, "change permissions of");
        }
        other => panic!("a folder Users can change the permissions of must be refused, got {other:?}"),
    }

    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "nothing may be registered:\n{query}");
    assert!(
        std::fs::read_dir(&folder).unwrap().next().is_none(),
        "nothing may be written to the folder"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_checks_the_folder_before_the_marker() {
    let (folder, _folder_guard) = default_folder();
    std::fs::create_dir_all(&folder).expect("create the default folder");
    let _doctored = Doctored::new(&folder);
    let host = folder.join("host.exe");
    std::fs::write(&host, b"not a goetia host").expect("write a host with no marker");
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    let standalone = |check| Scm::with_shim(StandaloneShim::new(host.clone(), vec![]), check);

    // Both checks would refuse this host; the folder's wins.
    match standalone(CheckPermissions::Enable).install(&spec, false) {
        Err(Error::ShimFolderUnsafe { path, .. }) => assert_eq!(path, folder),
        other => panic!("the folder check must come first, got {other:?}"),
    }
    // Without it, the marker is what refuses: the folder check was what answered above.
    match standalone(CheckPermissions::Disable).install(&spec, false) {
        Err(Error::ShimMarkerMissing { path }) => assert_eq!(path, host),
        other => panic!("the marker check must refuse with the folder check off, got {other:?}"),
    }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn disable_skips_the_folder_check() {
    let (folder, _folder_guard) = default_folder();
    std::fs::create_dir_all(&folder).expect("create the default folder");
    let _doctored = Doctored::new(&folder);
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(None, CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    let outcome = mgr
        .install(&spec, false)
        .expect("install into a folder the check would refuse");

    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");
    assert_eq!(std::fs::read(shim_in(&folder)).unwrap(), shim_bytes());
}

// The accounts ========================================================================================================

/// Installs a `type: simple` service with `install`, which is given its id, the daemon's
/// executable, and the port the daemon reports in on; starts it, waits for the daemon's report,
/// and stops it. `label` names the account in a failure.
fn runs_as(label: &str, mgr: &Scm, install: impl FnOnce(&str, &str, u16)) {
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    // Held until the service has stopped: the daemon's account cannot run the build tree's binary.
    let exe = WorldReadableExe::new(&id);
    let started = ConnectBack::listen();
    install(&id, &exe.path_str(), started.port());

    let id = goetia::spec::Id::try_from(id.as_str()).expect("a test id is an Id");
    // Unbounded: the subject is whether the account can run the shim, and a first logon under a
    // new account builds a profile, which outruns any fixed number.
    mgr.start(&id, Budget::Unbounded)
        .unwrap_or_else(|e| panic!("{label}: start: {}", e.report()));
    started.accept(&format!("the daemon to report in as {label}"));
    assert_eq!(mgr.status(&id).expect("status").state, State::Running, "{label}");
    mgr.stop(&id, Budget::DEFAULT)
        .unwrap_or_else(|e| panic!("{label}: stop: {}", e.report()));
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn every_account_type_runs_the_shim() {
    let (_folder, _folder_guard) = default_folder();
    let logs = LogDir::world_writable();
    let mgr = embedded_manager(None, CheckPermissions::Enable);

    let as_user = |label: &str, user: fn(&str) -> User| {
        runs_as(label, &mgr, |id, exe, port| {
            let spec = DaemonSpec {
                user: user(id),
                ..simple_spec(id, fixture_command_with_exe(exe, "report", &[&port.to_string()]), &logs)
            };
            mgr.install(&spec, false)
                .unwrap_or_else(|e| panic!("{label}: install: {}", e.report()));
        });
    };
    as_user("LocalSystem", |_| User::Root);
    as_user("LocalService", |_| User::Name("LocalService".to_string()));
    as_user("NetworkService", |_| User::Name("NetworkService".to_string()));
    as_user("NT SERVICE\\<id>", |id| User::Name(format!(r"NT SERVICE\{id}")));

    // A local account needs a password in the installing process's environment: a child.
    // <=14 chars: longer ones make `net user ... /add` prompt about pre-Windows-2000 compatibility.
    let password = "Goetia!Test12";
    let account = format!("gt{:08x}", rand::random::<u32>());
    let _user = LocalUserGuard::create(&account, password);
    runs_as("a local user", &mgr, |id, exe, port| {
        let output = Command::new(support::current_exe_str())
            .arg(INSTALL_AS)
            .args([id, &account, exe])
            .arg(logs.path_for(id))
            .arg(port.to_string())
            .env("GOETIA_SERVICE_PASSWORD", password)
            .output()
            .expect("spawn the install helper");
        assert!(
            output.status.success(),
            "install as a local user failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    });
}
