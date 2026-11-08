//! The stock shim's provisioning, against the real file system and SCM: `EmbeddedStockShim` places
//! the shim this crate builds, and `Scm::install` repairs, refuses or accepts what it finds.
//!
//! Everything here writes under `%ProgramFiles%` or sets an owner (Administrators), which needs an
//! elevated token, and runs on CI only. The tests that use the default folder own
//! `%ProgramFiles%\Goetia` for their length: each empties it first and removes it after, and
//! `UNIT_DIR_EXCLUSIVE` keeps them from running beside each other.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{Budget, ServiceManager as _, State};
use goetia::shim::{
    CheckPermissions, EmbeddedStockShim, FolderCheck, HostCommand, ShimProvider, StandaloneShim, SweepFailure,
};
use goetia::spec::{DaemonFields, DaemonSpec, User};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GENERIC_ALL, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, SYNCHRONIZE};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, MUTEX_ALL_ACCESS, OpenMutexW, ReleaseMutex, WaitForSingleObject,
};

use crate::common::{
    LogDir, ShimGuard, ShimProcess, UNIT_DIR_EXCLUSIVE, embedded_manager, embedded_shim, fixture_command,
    fixture_command_with_exe, other_shim_bytes, shim_bytes, simple_spec,
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

/// Grants `Users` a right on a path that the permission check refuses, and takes the grant back on
/// drop.
struct Doctored(PathBuf);

impl Doctored {
    const USERS: &'static str = "*S-1-5-32-545";

    /// The right to change `folder`'s permissions.
    fn new(folder: &Path) -> Self {
        Self::grant(folder, "WDAC")
    }

    /// `rights` (as `icacls` spells them) on `path`.
    fn grant(path: &Path, rights: &str) -> Self {
        let doctored = Self(path.to_path_buf());
        let path = path.to_string_lossy();
        support::cmd::run("icacls", &[&path, "/grant", &format!("{}:({rights})", Self::USERS)]).expect_ok();
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

/// `%ProgramFiles%\Goetia` made by goetia (installing a service of its own into it), and then given
/// `Users` the right to change its permissions. The folder is goetia's, owned by Administrators, so
/// the folder check reaches the grant: a folder the test made itself is owned by the runner's
/// account, which the check refuses as an owner before it reads any grant.
///
/// Fields drop in order: the grant is taken back, the service uninstalled (which sweeps the folder
/// away), and the folder removed if anything is left.
struct DoctoredFolder {
    folder: PathBuf,
    _grant: Doctored,
    _service: ShimGuard,
    _dir: DirGuard,
}

impl DoctoredFolder {
    fn new(logs: &LogDir) -> Self {
        let (folder, dir) = default_folder();
        let id = support::random_test_id();
        let service = ShimGuard::new(&id);
        let spec = simple_spec(&id, fixture_command("report", &["1"]), logs);
        embedded_manager(None, CheckPermissions::Enable)
            .install(&spec, false)
            .expect("install, which makes the default folder");
        let grant = Doctored::new(&folder);
        Self {
            folder,
            _grant: grant,
            _service: service,
            _dir: dir,
        }
    }

    /// What the folder holds, by path.
    fn contents(&self) -> Vec<PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(&self.folder)
            .unwrap_or_else(|e| panic!("read {}: {e}", self.folder.display()))
            .flatten()
            .map(|entry| entry.path())
            .collect();
        found.sort();
        found
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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn provide_repairs_a_file_that_denies_reading() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = shim_in(&folder);
    // Administrators, which this test runs as, may not read the file's data: opening it is refused.
    let path = file.to_string_lossy();
    support::cmd::run("icacls", &[&path, "/deny", "*S-1-5-32-544:(RD)"]).expect_ok();
    assert!(std::fs::File::open(&file).is_err(), "the denial must refuse an open");

    let again = mgr.install(&spec, false).expect("install over a file nobody can read");

    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
    let aside = stale_files(&folder);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(
        security_of(&aside[0]).contains("(D;"),
        "the file set aside is the one with the denial: {}",
        security_of(&aside[0])
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn provide_replaces_a_directory_at_the_shim_s_name() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    mgr.install(&spec, false).expect("install");
    let file = shim_in(&folder);
    std::fs::remove_file(&file).expect("delete the shim");
    std::fs::create_dir(&file).expect("put a folder at its name");
    std::fs::write(file.join("inside"), b"x").expect("fill it");

    let again = mgr
        .install(&spec, false)
        .expect("install over a folder at the shim's name");

    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert_eq!(std::fs::read(&file).unwrap(), shim_bytes());
    assert_stock_file(&file);
    let aside = stale_files(&folder);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(aside[0].join("inside").is_file(), "the folder was set aside whole");
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
    let logs = LogDir::new();
    let doctored = DoctoredFolder::new(&logs);
    let before = doctored.contents();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    match mgr.install(&spec, false) {
        Err(Error::ShimFolderUnsafe { path, account, right }) => {
            assert_eq!(path, doctored.folder);
            assert_eq!(right, "change permissions of");
            assert!(account.ends_with("Users"), "{account}");
        }
        other => panic!("a folder Users can change the permissions of must be refused, got {other:?}"),
    }

    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "nothing may be registered:\n{query}");
    assert_eq!(doctored.contents(), before, "nothing may be written to the folder");
}

/// A host file `Users` can write to is refused though its folder is goetia's: whoever can write to
/// it controls what runs as `LocalSystem`.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_refuses_a_host_file_users_can_write_to() {
    let logs = LogDir::new();
    let (folder, _dir) = default_folder();
    let first_id = support::random_test_id();
    let _first = ShimGuard::new(&first_id);
    let first = simple_spec(&first_id, fixture_command("report", &["1"]), &logs);
    embedded_manager(None, CheckPermissions::Enable)
        .install(&first, false)
        .expect("install, which makes the default folder and the file");
    let host = shim_in(&folder);
    let _grant = Doctored::grant(&host, "W");
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    let standalone = |check| Scm::with_shim(StandaloneShim::new(host.clone(), vec![]), check);

    match standalone(CheckPermissions::Enable).install(&spec, false) {
        Err(Error::ShimFolderUnsafe { path, account, right }) => {
            assert_eq!(path, host);
            assert_eq!(right, "write to");
            assert!(account.ends_with("Users"), "{account}");
        }
        other => panic!("a host file Users can write to must be refused, got {other:?}"),
    }
    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "nothing may be registered:\n{query}");
    standalone(CheckPermissions::Disable)
        .install(&spec, false)
        .expect("with the check off the same host is accepted");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_checks_the_folder_before_the_marker() {
    let logs = LogDir::new();
    let doctored = DoctoredFolder::new(&logs);
    let folder = doctored.folder.clone();
    let host = folder.join("host.exe");
    std::fs::write(&host, b"not a goetia host").expect("write a host with no marker");
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
    let logs = LogDir::new();
    let doctored = DoctoredFolder::new(&logs);
    let folder = doctored.folder.clone();
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
            let spec = DaemonSpec::new(DaemonFields {
                user: user(id),
                ..simple_spec(id, fixture_command_with_exe(exe, "report", &[&port.to_string()]), &logs).into_fields()
            })
            .unwrap();
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

// What `begin_install` checks of a provider ===========================================================================

/// A provider that records the calls it gets and answers with the commands it was given.
///
/// The same provider is `Recording` in `src/backend/scm/manager_tests.rs`, which this crate cannot
/// share (that one is `cfg(test)` inside the library, and this crate reaches the library only through
/// its public API): keep the two identical in behaviour.
struct Recording {
    calls: Arc<Mutex<Vec<&'static str>>>,
    preview: HostCommand,
    provide: HostCommand,
}

impl Recording {
    fn new(preview: HostCommand, provide: HostCommand) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::default();
        let provider = Self {
            calls: Arc::clone(&calls),
            preview,
            provide,
        };
        (provider, calls)
    }
}

impl ShimProvider for Recording {
    fn preview(&self) -> goetia::error::Result<HostCommand> {
        self.calls.lock().unwrap().push("preview");
        Ok(self.preview.clone())
    }

    fn provide(&self, _folders: &FolderCheck) -> goetia::error::Result<HostCommand> {
        self.calls.lock().unwrap().push("provide");
        Ok(self.provide.clone())
    }

    fn sweep(&self, _referenced: &[PathBuf]) -> Result<(), SweepFailure> {
        self.calls.lock().unwrap().push("sweep");
        Ok(())
    }
}

fn command(exe: impl Into<PathBuf>) -> HostCommand {
    HostCommand {
        exe: exe.into(),
        args: vec![],
    }
}

/// The install renders from the `preview`, so the registration names that host. A provider
/// whose `provide` places another (the sibling shim upgraded while the install waited on the lock,
/// or a `preview` and `provide` that disagree) provides a file the registration does not name:
/// refused as a mismatch, with nothing registered. This install fails before anything is
/// registered, so nothing needs a guard but the temp dir.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_refuses_a_host_other_than_the_one_rendered() {
    let dir = DirGuard::new(tempfile::tempdir().expect("tempdir").keep());
    let logs = LogDir::new();
    let spec = simple_spec(&support::random_test_id(), fixture_command("report", &["1"]), &logs);
    let (rendered, provided) = (dir.path().join("rendered.exe"), dir.path().join("provided.exe"));
    let (provider, calls) = Recording::new(command(&rendered), command(&provided));
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    match mgr.install(&spec, false) {
        Err(Error::ShimHostMismatch {
            rendered: said,
            provided: gave,
        }) => assert_eq!((*said, *gave), (command(rendered), command(provided))),
        other => panic!("a host other than the one the registration names must be refused, got {other:?}"),
    }
    assert_eq!(*calls.lock().unwrap(), ["preview", "sweep", "provide"]);
    assert!(
        matches!(mgr.status(&spec.id), Err(Error::NotInstalled { .. })),
        "a refused install registered the service"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_refuses_a_host_with_no_marker() {
    let dir = DirGuard::new(tempfile::tempdir().expect("tempdir").keep());
    let logs = LogDir::new();
    let spec = simple_spec(&support::random_test_id(), fixture_command("report", &["1"]), &logs);
    let exe = dir.path().join("host.exe");
    std::fs::write(&exe, b"not a goetia host").unwrap();
    let (provider, calls) = Recording::new(command(&exe), command(&exe));
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    match mgr.install(&spec, false) {
        Err(Error::ShimMarkerMissing { path }) => assert_eq!(path, exe),
        other => panic!("a file without the marker is no host, got {other:?}"),
    }
    // Previewed once for the render, before anything is written; then swept and provided, and what
    // was provided is checked against the render.
    assert_eq!(*calls.lock().unwrap(), ["preview", "sweep", "provide"]);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_refuses_a_host_named_by_a_relative_path() {
    let logs = LogDir::new();
    let spec = simple_spec(&support::random_test_id(), fixture_command("report", &["1"]), &logs);
    let (provider, _) = Recording::new(command(r"bin\host.exe"), command(r"bin\host.exe"));
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    match mgr.install(&spec, false) {
        Err(Error::ShimPathRelative { path }) => assert_eq!(path, PathBuf::from(r"bin\host.exe")),
        other => panic!("a relative host must be refused, got {other:?}"),
    }
}

// The install lock ====================================================================================================

/// What an install running on its own thread reports to the test.
enum Event {
    /// The provider is inside `provide`, so the install holds the lock.
    Provided,
    Installed(goetia::error::Result<Outcome>),
}

/// The stock shim of this crate through `StandaloneShim`, held in `provide` until the test lets it
/// go.
struct Gated {
    inner: StandaloneShim,
    events: Mutex<mpsc::Sender<Event>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl ShimProvider for Gated {
    fn preview(&self) -> goetia::error::Result<HostCommand> {
        self.inner.preview()
    }

    fn provide(&self, folders: &FolderCheck) -> goetia::error::Result<HostCommand> {
        self.events
            .lock()
            .unwrap()
            .send(Event::Provided)
            .expect("the test is listening");
        // A test that ends early drops its sender, which lets the provider go as a send does.
        let _ = self.release.lock().unwrap().recv();
        self.inner.provide(folders)
    }

    fn sweep(&self, referenced: &[PathBuf]) -> Result<(), SweepFailure> {
        self.inner.sweep(referenced)
    }
}

/// The lock's name.
const LOCK_NAME: &str = r"Global\goetia-shim";

/// The lock, opened to read its permissions and to wait on it.
fn open_lock() -> OwnedHandle {
    let name = support::wide_null(LOCK_NAME);
    // SAFETY: `name` is NUL-terminated; the handle is checked, then owned.
    let raw = unsafe { OpenMutexW(SYNCHRONIZE | READ_CONTROL, 0, name.as_ptr()) };
    assert!(
        !raw.is_null(),
        "open the install lock: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a valid handle `OpenMutexW` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// Runs an install of a `type: simple` service on its own thread, which stops inside the
/// provider's `provide`, where the lock is held, and calls `probe` with the lock opened then. The
/// install is then let go and must succeed. Returns the lock, opened. A probe that panics lets the
/// install go too (the closure owns `release`, which is dropped as it unwinds, before the scope
/// waits for the install thread), so the test fails instead of hanging.
fn probing_the_lock_during_an_install(probe: impl FnOnce(&OwnedHandle)) -> OwnedHandle {
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let (events, happened) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let provider = Gated {
        inner: StandaloneShim::new(PathBuf::from(env!("CARGO_BIN_EXE_goetia-shim")), vec![]),
        events: Mutex::new(events.clone()),
        release: Mutex::new(released),
    };
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    std::thread::scope(|scope| {
        let release = release;
        scope.spawn(|| {
            let installed = mgr.install(&spec, false);
            let _ = events.send(Event::Installed(installed));
        });
        match happened.recv().expect("the install reports") {
            Event::Provided => {}
            Event::Installed(result) => panic!("the install ended before `provide`: {result:?}"),
        }
        let lock = open_lock();
        probe(&lock);
        release.send(()).expect("the provider is waiting");
        match happened.recv().expect("the install reports") {
            Event::Installed(result) => {
                result.unwrap_or_else(|e| panic!("install: {}", e.report()));
            }
            Event::Provided => panic!("`provide` ran twice"),
        }
        lock
    })
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn lock_is_exclusive() {
    let lock = probing_the_lock_during_an_install(|lock| {
        // A zero-timeout wait reads whether the mutex is held and waits for nothing.
        // SAFETY: `lock` is a live handle with `SYNCHRONIZE`.
        let held = unsafe { WaitForSingleObject(lock.as_raw_handle(), 0) };
        assert_eq!(held, WAIT_TIMEOUT, "an install in `provide` must hold the lock");
    });

    // The install has ended, so the lock is free. This thread takes it, and gives it back.
    // SAFETY: as above.
    let free = unsafe { WaitForSingleObject(lock.as_raw_handle(), 0) };
    assert_eq!(free, WAIT_OBJECT_0, "a finished install must release the lock");
    // SAFETY: taken by this thread just above.
    assert_ne!(unsafe { ReleaseMutex(lock.as_raw_handle()) }, 0);
}

/// `ACCESS_ALLOWED_ACE_TYPE`, which `windows-sys` does not export.
const ACCESS_ALLOWED_ACE_TYPE_VALUE: u8 = 0;

/// The DACL of the kernel object `handle` (open with `READ_CONTROL`), as what it means: whether it
/// is protected, and each entry as its trustee's SID text, its mask and its type. The kernel stores
/// a generic right as the object's specific mask, so a text comparison would test the spelling.
fn dacl_of(handle: HANDLE) -> (bool, Vec<(String, u32, u8)>) {
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `handle` is open with `READ_CONTROL`; the out-pointers are valid lvalues, and the
    // others are not asked for. The descriptor owns `dacl`'s memory until it is freed below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, 0, "read the permissions of the lock: error {status}");
    assert!(
        !dacl.is_null(),
        "the lock has a NULL DACL, which grants everyone every right"
    );
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor` is live; the out-pointers are valid lvalues.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    let mut entries = Vec::new();
    // SAFETY: `dacl` points into the live `descriptor`, so its header is readable.
    let count = unsafe { (*dacl).AceCount };
    for index in 0..u32::from(count) {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `index` is below the entry count; `ace` is a valid out-pointer.
        assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
        // SAFETY: `GetAce` returned an entry inside `dacl`; every type read here begins with the
        // header, the mask and the trustee's SID.
        let (kind, mask, sid) = unsafe {
            let body = ace as *const ACCESS_ALLOWED_ACE;
            (
                (*(ace as *const ACE_HEADER)).AceType,
                (*body).Mask,
                std::ptr::addr_of!((*body).SidStart) as PSID,
            )
        };
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is a valid SID inside the entry; `text` is a valid out-pointer.
        assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
        let mut len = 0;
        // SAFETY: `text` is a NUL-terminated string, live until it is freed below.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: the `len` units before the NUL were just read; `text` is freed once.
        let sid_text = unsafe {
            let sid_text = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            sid_text
        };
        entries.push((sid_text, mask, kind));
    }
    // SAFETY: allocated by `GetSecurityInfo`, freed once, after the last read.
    unsafe { LocalFree(descriptor) };
    (control & SE_DACL_PROTECTED != 0, entries)
}

/// The lock is for SYSTEM and Administrators alone, each with every right the mutex has, in a DACL
/// that inherits nothing.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn lock_is_for_system_and_administrators_only() {
    probing_the_lock_during_an_install(|lock| {
        let (protected, entries) = dacl_of(lock.as_raw_handle());

        assert!(protected, "the lock's DACL must not inherit: {entries:?}");
        let mut trustees: Vec<&str> = entries.iter().map(|(sid, ..)| sid.as_str()).collect();
        trustees.sort();
        assert_eq!(
            trustees,
            ["S-1-5-18", "S-1-5-32-544"],
            "SYSTEM and Administrators alone: {entries:?}"
        );
        for (sid, mask, kind) in &entries {
            assert_eq!(*kind, ACCESS_ALLOWED_ACE_TYPE_VALUE, "{sid} is allowed, not denied");
            // The kernel stores a generic right as the object's own mask: all of the mutex's rights.
            let full = mask & GENERIC_ALL != 0 || mask & MUTEX_ALL_ACCESS == MUTEX_ALL_ACCESS;
            assert!(
                full,
                "{sid} has {mask:#x}, not every right of the mutex ({MUTEX_ALL_ACCESS:#x})"
            );
        }
    });
}

/// The lock's name, made now as the user the test runs as, owned by that user, with a DACL of
/// `dacl` (SDDL ACEs). It closes when the handle drops, which is when the name goes.
fn lock_made_by_this_user(dacl: &str) -> OwnedHandle {
    let whoami = support::cmd::run("whoami", &["/user", "/fo", "csv", "/nh"]).expect_ok();
    let sid = whoami
        .stdout
        .trim()
        .rsplit('"')
        .nth(1)
        .unwrap_or_else(|| panic!("no SID in {whoami}"))
        .to_string();
    let sddl = support::wide_null(&format!("O:{sid}D:{dacl}"));
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; `descriptor` is a valid out-pointer, freed below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        converted,
        0,
        "build the descriptor: {}",
        std::io::Error::last_os_error()
    );
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name = support::wide_null(LOCK_NAME);
    // SAFETY: `attributes` points at `descriptor`, live for the call; `name` is NUL-terminated.
    let raw = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
    // SAFETY: read straight after the call; `descriptor` is freed once, after its last use.
    let (existed, created) = unsafe {
        let last = GetLastError();
        let created = std::io::Error::last_os_error();
        LocalFree(descriptor);
        (last == ERROR_ALREADY_EXISTS, created)
    };
    assert!(!raw.is_null(), "create the lock as {sid}: {created}");
    assert!(!existed, "`{LOCK_NAME}` existed before this test made it");
    // SAFETY: a valid handle `CreateMutexW` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// The error an install ends with when the lock this user made with `dacl` is there.
fn error_of_refused_lock(dacl: &str) -> Error {
    let _foreign = lock_made_by_this_user(dacl);

    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    mgr.install(&spec, false)
        .expect_err("the lock someone else made is refused")
}

/// A mutex of the lock's name that an account other than SYSTEM, Administrators and TrustedInstaller
/// owns (here, the user the test runs as) is refused before any wait, naming that account.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_lock_someone_else_created_first_is_refused() {
    match error_of_refused_lock("(A;;GA;;;WD)") {
        Error::ShimLockForeign { account } => assert!(!account.is_empty(), "the refusal names the owner"),
        other => panic!("expected the foreign lock to be refused: {other:?}"),
    }
}

/// A lock whose DACL admits SYSTEM alone denies an elevated caller the open: the refusal says access
/// was denied to a token with Administrators enabled, not that the owner is untrusted, and the owner
/// is read through `READ_CONTROL`, which an owner always has.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_lock_that_denies_administrators_is_refused_as_inaccessible() {
    match error_of_refused_lock("(A;;GA;;;SY)") {
        Error::ShimLockDenied { owner } => assert!(
            owner.as_deref().is_some_and(|owner| !owner.is_empty()),
            "the refusal names the owner: {owner:?}"
        ),
        other => panic!("expected the lock that denies Administrators to be refused: {other:?}"),
    }
}

// The sweep at install ================================================================================================

/// What a shim file of another build is named: a valid name nothing is registered under.
fn stray_shim_in(folder: &Path) -> PathBuf {
    let stray = folder.join(format!("goetia-shim-{}.exe", "ab".repeat(32)));
    std::fs::create_dir_all(folder).expect("create the folder");
    std::fs::write(&stray, b"left behind").expect("write the stray file");
    stray
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_sweeps_what_nothing_references() {
    let (_root, folder) = temp_shim_folder();
    let stray = stray_shim_in(&folder);
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    mgr.install(&spec, false).expect("install");

    assert!(!stray.exists(), "a shim file nothing references is swept");
    assert!(shim_in(&folder).is_file());
}

/// Service B runs a file other than the one the next install provides, as one installed by another
/// goetia version does: the install keeps it, and the two services run side by side.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn other_versions_shim_is_kept() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let (b_id, a_id) = (support::random_test_id(), support::random_test_id());
    let (_b_guard, _a_guard) = (ShimGuard::new(&b_id), ShimGuard::new(&a_id));
    let (b_started, a_started) = (ConnectBack::listen(), ConnectBack::listen());
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let b_spec = simple_spec(
        &b_id,
        fixture_command("report", &[&b_started.port().to_string()]),
        &logs,
    );
    mgr.install(&b_spec, false).expect("install B");
    // B's file is another valid name for the same host: what another version's build is.
    let theirs = folder.join(format!("goetia-shim-{}.exe", "cd".repeat(32)));
    std::fs::copy(shim_in(&folder), &theirs).expect("copy the shim to a second name");
    support::cmd::run(
        "sc.exe",
        &["config", &b_id, "binPath=", &format!("\"{}\"", theirs.display())],
    )
    .expect_ok();

    let a_spec = simple_spec(
        &a_id,
        fixture_command("report", &[&a_started.port().to_string()]),
        &logs,
    );
    mgr.install(&a_spec, false).expect("install A");

    assert!(theirs.is_file(), "the file a registered service runs is not swept");
    mgr.start(&b_spec.id, Budget::DEFAULT).expect("start B");
    mgr.start(&a_spec.id, Budget::DEFAULT).expect("start A");
    b_started.accept("B to report in from the other file");
    a_started.accept("A to report in from the file the install provided");
    // B's host is not the one this goetia provides, so installing it again changes it back.
    let again = mgr.install(&b_spec, false).expect("install B again");
    assert!(matches!(again, Outcome::Update { .. }), "{again:?}");
}

// Uninstall ===========================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_leaves_nothing() {
    let (folder, _folder_guard) = default_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let started = ConnectBack::listen();
    let mgr = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in under the shim in Program Files");

    // The shim is running its own file, which cannot be deleted while it does.
    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(
        !folder.exists(),
        "no shim file and no default folder is left: {:?}",
        std::fs::read_dir(&folder).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
    assert!(matches!(mgr.status(&spec.id), Err(Error::NotInstalled { .. })));
}

/// A re-provided service still runs the file it started from. Another service's uninstall leaves
/// that file alone, and its own waits for its process and then takes both files.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_keeps_a_running_old_shim() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let (a_id, b_id) = (support::random_test_id(), support::random_test_id());
    let (_a_guard, _b_guard) = (ShimGuard::new(&a_id), ShimGuard::new(&b_id));
    let started = ConnectBack::listen();
    let first = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let second = Scm::with_shim(
        EmbeddedStockShim::new(Some(other_shim_bytes()), Some(folder.clone())).with_check(CheckPermissions::Disable),
        CheckPermissions::Disable,
    );
    let a_spec = simple_spec(&a_id, fixture_command("report", &[&started.port().to_string()]), &logs);
    first.install(&a_spec, false).expect("install A on the first file");
    let old = shim_in(&folder);
    first.start(&a_spec.id, Budget::DEFAULT).expect("start A");
    started.accept("A to report in");
    let running = ShimProcess::attach(&a_id);

    // A moves to the second file and keeps running the first.
    let moved = second.install(&a_spec, false).expect("install A on the second file");
    assert!(matches!(moved, Outcome::Update { .. }), "{moved:?}");
    let new = EmbeddedStockShim::new(Some(other_shim_bytes()), Some(folder.clone()))
        .preview()
        .expect("preview")
        .exe;
    assert!(old.is_file() && new.is_file(), "both files exist: {old:?} {new:?}");
    let b_spec = simple_spec(&b_id, fixture_command("report", &["1"]), &logs);
    second.install(&b_spec, false).expect("install B on the second file");

    second.uninstall(&b_spec.id).expect("uninstall B");

    assert!(old.is_file(), "the file A's process runs is kept");
    assert!(new.is_file(), "the file A is registered with is kept");
    second.uninstall(&a_spec.id).expect("uninstall A");
    assert!(running.has_exited(), "uninstall returns once A's process is gone");
    assert!(!old.exists() && !new.exists(), "nothing references either file now");
}

/// Two services on one file: uninstalling one returns without waiting for the other, whose process
/// runs the file, and leaves the file and the process alone.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_of_one_of_two_on_a_file_returns() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let (a_id, b_id) = (support::random_test_id(), support::random_test_id());
    let (_a_guard, _b_guard) = (ShimGuard::new(&a_id), ShimGuard::new(&b_id));
    let started = ConnectBack::listen();
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let a_spec = simple_spec(&a_id, fixture_command("report", &["1"]), &logs);
    let b_spec = simple_spec(&b_id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&a_spec, false).expect("install A");
    mgr.install(&b_spec, false).expect("install B");
    mgr.start(&b_spec.id, Budget::DEFAULT).expect("start B");
    started.accept("B to report in");
    let running = ShimProcess::attach(&b_id);

    mgr.uninstall(&a_spec.id)
        .expect("uninstall A, which does not wait for B");

    assert!(shim_in(&folder).is_file(), "B still runs the file");
    assert!(!running.has_exited(), "B keeps running");
    assert_eq!(mgr.status(&b_spec.id).expect("status").state, State::Running);
}

/// `stop` returns once the service's process has exited, not when SCM first reports `STOPPED`, which
/// the shim does before it exits.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn stop_returns_after_the_services_process_has_exited() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let started = ConnectBack::listen();
    let mgr = embedded_manager(Some(folder), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the service to report in");
    let running = ShimProcess::attach(&id);

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");

    assert!(
        running.has_exited(),
        "`stop` returned while the shim's process was still running"
    );
}

/// `stop` and then `uninstall`: the process is already gone, so the uninstall finds the file free and
/// removes it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_right_after_stop_leaves_no_file() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let started = ConnectBack::listen();
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the service to report in");
    let file = shim_in(&folder);
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");

    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(
        !file.exists(),
        "nothing runs the file, so the uninstall's sweep removes it"
    );
}

/// `stop` that does not wait, then `uninstall` at once: the service may report `STOPPED` with no pid
/// while its process is still exiting, and then the uninstall holds no process and does not wait for
/// it. It succeeds whether or not the file is still in use; a file the process still ran is the next
/// sweep's, once the process has exited.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_right_after_an_unwaited_stop_succeeds_and_the_next_sweep_takes_the_file() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let started = ConnectBack::listen();
    let mgr = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the service to report in");
    let running = ShimProcess::attach(&id);
    let file = shim_in(&folder);
    mgr.stop(&spec.id, Budget::Immediate).expect("request the stop");

    mgr.uninstall(&spec.id).expect("uninstall");

    running.wait_for_exit();
    let retry = mgr.uninstall(&spec.id);
    assert!(matches!(retry, Err(Error::NotInstalled { .. })), "{retry:?}");
    assert!(!file.exists(), "the sweep after the process exited removes it");
}

/// A service B that was updated to another file and not restarted still runs the first. Uninstalling
/// A, the last service registered with the first file, does not wait for B's process and leaves it
/// alone; the file is skipped without an error, as B's image references it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_does_not_wait_for_an_updated_service_still_on_the_old_file() {
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let (a_id, b_id) = (support::random_test_id(), support::random_test_id());
    let (_a_guard, _b_guard) = (ShimGuard::new(&a_id), ShimGuard::new(&b_id));
    let started = ConnectBack::listen();
    let first = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let second = Scm::with_shim(
        EmbeddedStockShim::new(Some(other_shim_bytes()), Some(folder.clone())).with_check(CheckPermissions::Disable),
        CheckPermissions::Disable,
    );
    let a_spec = simple_spec(&a_id, fixture_command("report", &["1"]), &logs);
    let b_spec = simple_spec(&b_id, fixture_command("report", &[&started.port().to_string()]), &logs);
    first.install(&a_spec, false).expect("install A on the first file");
    first.install(&b_spec, false).expect("install B on the first file");
    let old = shim_in(&folder);
    first.start(&b_spec.id, Budget::DEFAULT).expect("start B");
    started.accept("B to report in");
    let running = ShimProcess::attach(&b_id);
    let moved = second.install(&b_spec, false).expect("install B on the second file");
    assert!(matches!(moved, Outcome::Update { .. }), "{moved:?}");

    second
        .uninstall(&a_spec.id)
        .expect("uninstall A, which does not wait for B");

    assert!(old.is_file(), "B's process still runs the first file");
    assert!(!running.has_exited(), "B keeps running");
}

/// A service registered in the default folder, by a manager whose provider uses another: its uninstall
/// sweeps the default folder too.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_sweeps_the_default_folder_when_it_is_not_the_providers() {
    let (default, _default_guard) = default_folder();
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let theirs = embedded_manager(None, CheckPermissions::Enable);
    let ours = embedded_manager(Some(folder), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    theirs.install(&spec, false).expect("install in the default folder");
    assert!(default.is_dir());

    ours.uninstall(&spec.id).expect("uninstall");

    assert!(!default.exists(), "the default folder is swept, and removed once empty");
}

/// A service registered in a folder that is neither the default nor the provider's, such as a
/// consumer installer's own: its uninstall leaves the folder alone.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_leaves_a_folder_that_is_neither_the_default_nor_the_providers() {
    let (_default, _default_guard) = default_folder();
    let (_root, folder) = temp_shim_folder();
    let logs = LogDir::new();
    let id = support::random_test_id();
    let _guard = ShimGuard::new(&id);
    let theirs = embedded_manager(Some(folder.clone()), CheckPermissions::Disable);
    let ours = embedded_manager(None, CheckPermissions::Enable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    theirs.install(&spec, false).expect("install under the other folder");

    ours.uninstall(&spec.id).expect("uninstall");

    assert!(
        shim_in(&folder).is_file(),
        "a file in a folder goetia does not own is not swept"
    );
}

/// `uninstall` of an id that is not installed is the retry of a sweep that failed.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_retry_sweeps() {
    let (_root, folder) = temp_shim_folder();
    let stray = stray_shim_in(&folder);
    let mgr = embedded_manager(Some(folder), CheckPermissions::Disable);
    let id = goetia::spec::Id::try_from(support::random_test_id().as_str()).expect("a test id is an Id");

    let result = mgr.uninstall(&id);

    assert!(matches!(result, Err(Error::NotInstalled { .. })), "{result:?}");
    assert!(!stray.exists(), "the retry sweeps");
}
