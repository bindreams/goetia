//! Fixtures shared by the SCM unit tests: byte layouts of registry string values, and a service's blob.

/// A deadline that never passes: a wait under it never expires.
pub(crate) fn unbounded() -> crate::manager::budget::Deadline {
    crate::manager::budget::Budget::Unbounded.start()
}

/// An already-spent deadline: the only kind under which a fake reports an expiry.
pub(crate) fn spent() -> crate::manager::budget::Deadline {
    crate::manager::budget::Budget::Immediate.start()
}

/// The bytes goetia stores for a string value: UTF-16LE and one closing NUL.
pub(crate) fn sz(text: &str) -> Vec<u8> {
    units_bytes(&text.encode_utf16().chain([0]).collect::<Vec<_>>())
}

/// `units` as the UTF-16LE bytes a registry value holds.
pub(crate) fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// The hash in the file name of a shim host, as a provider places one: SHA-256 of `b"abc"`.
pub(crate) const SHIM_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A service's blob of `protocol`, as `install` writes it for the daemon `frpc`.
#[cfg(windows)]
pub(crate) fn blob_of(protocol: crate::spec::Protocol) -> crate::blob::Blob {
    use crate::spec::{DaemonSpec, ServiceFields, ServiceSpec};
    use crate::testing::base_fields;

    crate::blob::Blob {
        schema: crate::blob::SCHEMA,
        version: "0.1.0".to_string(),
        spec: DaemonSpec::from(
            ServiceSpec::new(ServiceFields {
                protocol,
                ..base_fields("frpc", vec![crate::spec::test_support::program("frpc")])
            })
            .unwrap(),
        ),
    }
}

/// The task `nightly`, which runs `schedule` (`None`: nothing starts it).
#[cfg(windows)]
pub(crate) fn task_spec(schedule: Option<&str>) -> crate::spec::DaemonSpec {
    crate::testing::task("nightly", vec![crate::spec::test_support::program("job")], schedule)
}

/// The task `nightly` as `user` runs it.
#[cfg(windows)]
pub(crate) fn task_spec_as(user: crate::spec::User, schedule: Option<&str>) -> crate::spec::DaemonSpec {
    let crate::spec::DaemonSpec::Task(task) = task_spec(schedule) else {
        unreachable!("`task_spec` builds a task")
    };
    let mut fields = task.into_fields();
    fields.common.user = user;
    crate::spec::TaskSpec::new(fields)
        .map(crate::spec::DaemonSpec::from)
        .expect("a task as another user")
}

/// The shim host a task runs.
#[cfg(windows)]
pub(crate) fn task_host() -> crate::shim::HostCommand {
    crate::shim::HostCommand {
        exe: crate::spec::test_support::abs("Program Files/Goetia/goetia-shim.exe"),
        args: vec!["--service-host".to_owned()],
    }
}

/// The token of every registration [`registered_task`] makes.
#[cfg(windows)]
pub(crate) const TASK_TOKEN: &str = "feedfeedfeedfeedfeedfeedfeedfeed";

#[cfg(windows)]
pub(crate) fn task_token() -> crate::backend::scm::task_xml::Token {
    crate::backend::scm::task_xml::Token::parse(TASK_TOKEN).expect("a fixture token is 32 lower-case hex digits")
}

/// What Task Scheduler holds for `spec` run through [`task_host`]: the XML goetia registers with the
/// token [`TASK_TOKEN`], its triggers or none, and the blob as its `Data`.
#[cfg(windows)]
pub(crate) fn registered_task(
    spec: &crate::spec::DaemonSpec,
    armed: bool,
) -> crate::backend::scm::manager::task::api::Registered {
    registered_task_with(spec, armed, &task_token())
}

/// [`registered_task`] under the registration `token`.
#[cfg(windows)]
pub(crate) fn registered_task_with(
    spec: &crate::spec::DaemonSpec,
    armed: bool,
    token: &crate::backend::scm::task_xml::Token,
) -> crate::backend::scm::manager::task::api::Registered {
    let definition = crate::backend::scm::task_xml::TaskXml::new(spec, task_host()).unwrap();
    crate::backend::scm::manager::task::api::Registered {
        xml: definition.to_xml(armed, token),
        data: crate::blob::metadata_text(spec),
        enabled: Some(true),
    }
}

/// Makes `descriptor`'s DACL, protected, the DACL of the existing file or folder `path`, not following a
/// link at it: a fixture's permissions.
#[cfg(windows)]
pub(crate) fn set_dacl(
    descriptor: &crate::backend::scm::shim::acl::Descriptor,
    path: &std::path::Path,
) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL, WRITE_DAC,
    };

    let object = std::fs::OpenOptions::new()
        .access_mode(WRITE_DAC | READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    descriptor.set_dacl_of(&object)
}

/// The files `names` of the records directory `dir`, whose delete is refused for no handle and no pending
/// delete, to every account and an elevated token too: they carry SYSTEM's entry alone, and the directory grants
/// no right to delete entries. Dropping restores the directory, so that the test's own can be removed.
#[cfg(windows)]
pub(crate) struct Undeletable<'a>(&'a std::path::Path);

#[cfg(windows)]
impl<'a> Undeletable<'a> {
    pub(crate) fn new(dir: &'a std::path::Path, names: &[&str]) -> Self {
        use crate::backend::scm::shim::acl::Descriptor;

        let system_only = Descriptor::from_sddl("D:P(A;;FA;;;SY)").unwrap();
        for name in names {
            set_dacl(&system_only, &dir.join(name)).unwrap();
        }
        // Read, list and add a file, which `Dir::open` asks for; not `DELETE_CHILD`.
        let listable = Descriptor::from_sddl("D:P(A;;0x1200a3;;;WD)").unwrap();
        set_dacl(&listable, dir).unwrap();
        Undeletable(dir)
    }
}

#[cfg(windows)]
impl Drop for Undeletable<'_> {
    fn drop(&mut self) {
        let restored =
            crate::backend::scm::shim::acl::Descriptor::for_runs(&[crate::elevation::test_support::process_user_sid()])
                .map(|wanted| set_dacl(&wanted, self.0));
        if !matches!(restored, Ok(Ok(()))) {
            eprintln!(
                "warning: could not restore the permissions of {}: {restored:?}",
                self.0.display()
            );
        }
    }
}

/// An account no test runs as.
#[cfg(windows)]
pub(crate) const A_STRANGER: &str = "S-1-5-21-1-2-3-1000";

/// Makes the permissions of the existing file `path` one entry, full access for the account `sid` (SDDL
/// text), and nothing else, administrators included. The file's owner may still change them.
#[cfg(windows)]
pub(crate) fn permit_only(path: &std::path::Path, sid: &str) {
    set_entries(path, &[("FA", sid)]);
}

/// [`permit_only`] for an account no test runs as, with the account `sid` left only the rights to read the
/// attributes and the permissions and to change them: it may not append, which is what a log's account loses
/// when an install takes its entry out, and a test may still put the permissions back.
#[cfg(windows)]
pub(crate) fn withdraw_append(path: &std::path::Path, sid: &str) {
    // Read the attributes and the permissions, change the permissions, and wait on the handle (every open asks
    // for the last).
    set_entries(path, &[("FA", A_STRANGER), ("0x160080", sid)]);
}

/// Makes the permissions of the existing file `path` `entries`, each rights in SDDL and the account's SID,
/// set on a handle opened as an install opens a log to set them.
#[cfg(windows)]
fn set_entries(path: &std::path::Path, entries: &[(&str, &str)]) {
    let sddl: String = entries
        .iter()
        .map(|(rights, sid)| format!("(A;;{rights};;;{sid})"))
        .collect();
    let descriptor = crate::backend::scm::shim::acl::Descriptor::from_sddl(&format!("D:P{sddl}")).unwrap();
    let (file, made) = descriptor
        .open_always_for_dacl(path, false)
        .unwrap_or_else(|e| panic!("open {} to set {sddl}: {e}", path.display()));
    assert!(!made, "{} was not there", path.display());
    descriptor.set_dacl_of(&file).unwrap();
}

/// Makes a directory with a file in it at `path`. Where a file is expected, deleting it as a file fails
/// for a reason that is no handle and no pending delete, whoever runs it. The tempdir the tests make it
/// in removes it. A task's records directory names its account without the right to add a subdirectory,
/// so the directory `path` is in is opened to everyone first.
#[cfg(windows)]
pub(crate) fn directory_with_a_file(path: &std::path::Path) {
    let everyone = crate::backend::scm::shim::acl::Descriptor::from_sddl("D:P(A;;FA;;;WD)").unwrap();
    set_dacl(&everyone, path.parent().expect("a path in a directory")).unwrap();
    std::fs::create_dir(path).unwrap();
    std::fs::write(path.join("inside"), "").unwrap();
}

/// A directory at `path` that cannot be removed while the returned handle is open: a file in it is held
/// without sharing delete, so removing its tree is a sharing violation.
#[cfg(windows)]
pub(crate) fn directory_held_open(path: &std::path::Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    directory_with_a_file(path);
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path.join("inside"))
        .unwrap()
}

/// A file at `path` whose delete is pending: its holder closed it while the returned handle was open on
/// it, so it is deleted when that handle is dropped. Windows answers an open or a delete of it with
/// access denied.
#[cfg(windows)]
pub(crate) fn delete_pending(path: &std::path::Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_DELETE_ON_CLOSE;

    let holder = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
        .unwrap();
    let reader = std::fs::OpenOptions::new().read(true).open(path).unwrap();
    drop(holder);
    reader
}

/// Gives the existing empty directory `dir` the reparse tag `tag` with `payload`, as the account that may
/// add files to it can: it is the same directory, so a handle held on it before is on a
/// reparse point now.
#[cfg(windows)]
pub(crate) fn set_reparse_point(dir: &std::path::Path, tag: u32, payload: &[u8]) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    // The tag, the length of the data, and a reserved field.
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&tag.to_le_bytes());
    buffer.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(payload);
    // `FILE_ADD_FILE`, which is what the account has on its directory, and `SYNCHRONIZE`.
    let file = std::fs::OpenOptions::new()
        .access_mode(0x2 | 0x10_0000)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
        .unwrap();
    let mut returned = 0u32;
    // SAFETY: `buffer` is readable for the length given; `file` is a live handle; no output is asked for.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "set the reparse point: {}", std::io::Error::last_os_error());
}

/// `path` by its 8.3 names where the volume keeps them, and unchanged where it does not.
#[cfg(windows)]
pub(crate) fn short(path: &std::path::Path) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide = crate::backend::scm::wide::wide_null(path).unwrap();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `buf` is `buf.len()` writable units.
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    assert!(
        0 < len && len < buf.len(),
        "GetShortPathNameW: {}",
        std::io::Error::last_os_error()
    );
    std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..len]))
}

/// Makes the existing empty directory `dir` a mount point (a junction) to the existing `target`, in place:
/// see [`set_reparse_point`]. `mklink /J` cannot, since it makes the directory.
#[cfg(windows)]
pub(crate) fn make_mount_point(dir: &std::path::Path, target: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt as _;

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    let target = target.canonicalize().unwrap();
    // `canonicalize` gives `\?\C:\...`; the substitute name is `\??\C:\...` and the print name `C:\...`.
    let print: Vec<u16> = target.as_os_str().encode_wide().skip(4).collect();
    let substitute: Vec<u16> = r"\??\".encode_utf16().chain(print.iter().copied()).collect();
    let bytes = |units: &[u16]| units.iter().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>();
    let (substitute, print) = (bytes(&substitute), bytes(&print));
    let mut payload = Vec::new();
    // The offsets and lengths of the two names in the buffer that follows, each closed by a NUL unit.
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&u16::try_from(substitute.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(&u16::try_from(substitute.len() + 2).unwrap().to_le_bytes());
    payload.extend_from_slice(&u16::try_from(print.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(&substitute);
    payload.extend_from_slice(&[0, 0]);
    payload.extend_from_slice(&print);
    payload.extend_from_slice(&[0, 0]);
    set_reparse_point(dir, IO_REPARSE_TAG_MOUNT_POINT, &payload);
}

/// The rights the account `sid` has on the file `path` from its permissions alone, as
/// `GetEffectiveRightsFromAclW` says: what an open for exactly those rights would be granted. The owner's implicit
/// rights are not counted, which is why a test of them reads the owner.
#[cfg(windows)]
pub(crate) fn effective_rights(path: &std::path::Path, sid: &mut [u8]) -> u32 {
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        BuildTrusteeWithSidW, GetEffectiveRightsFromAclW, GetNamedSecurityInfoW, SE_FILE_OBJECT, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let wide = crate::backend::scm::wide::wide_null(path).unwrap();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; the out-pointers are locals; the descriptor is freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, ERROR_SUCCESS, "read the permissions of {}", path.display());
    // SAFETY: a zeroed `TRUSTEE_W` is filled by the call below; `sid` outlives the use of the trustee.
    let mut trustee: TRUSTEE_W = unsafe { std::mem::zeroed() };
    // SAFETY: `trustee` is a valid out-pointer and `sid` a whole SID.
    unsafe { BuildTrusteeWithSidW(&mut trustee, sid.as_mut_ptr().cast()) };
    let mut rights = 0u32;
    // SAFETY: `dacl` points into `descriptor`, which is live; `trustee` and `rights` are locals.
    let status = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) };
    // SAFETY: `descriptor` was allocated by `GetNamedSecurityInfoW`, and is freed once.
    unsafe { LocalFree(descriptor) };
    assert_eq!(status, ERROR_SUCCESS, "the rights of the account on {}", path.display());
    rights
}

/// What happened to the names in one directory, in the order Windows reported it, from the moment of
/// [`DirWatch::new`]: a test of the order of two changes reads it after the fact, with no timing in it.
#[cfg(windows)]
pub(crate) struct DirWatch {
    dir: std::fs::File,
    event: std::os::windows::io::OwnedHandle,
    /// `u64`s, so the buffer is aligned for `FILE_NOTIFY_INFORMATION`.
    buffer: Box<[u64; 2048]>,
    overlapped: Box<windows_sys::Win32::System::IO::OVERLAPPED>,
}

/// One change of a name in a watched directory.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameChange {
    Added(String),
    Removed(String),
    /// A rename to this name.
    RenamedTo(String),
    /// A rename from this name.
    RenamedFrom(String),
}

#[cfg(windows)]
impl DirWatch {
    /// Starts watching the names in `dir`: every change from now on is kept for [`DirWatch::until_added`].
    pub(crate) fn new(dir: &std::path::Path) -> DirWatch {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        use windows_sys::Win32::System::Threading::CreateEventW;

        let dir = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
            .open(dir)
            .unwrap();
        // SAFETY: an unnamed manual-reset event with default security; a null handle is the failure.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null(), "CreateEventW: {}", std::io::Error::last_os_error());
        // SAFETY: `event` is a handle this call owns.
        let event = unsafe { OwnedHandle::from_raw_handle(event.cast()) };
        let mut watch = DirWatch {
            dir,
            event,
            buffer: Box::new([0; 2048]),
            // SAFETY: an all-zero `OVERLAPPED` is the documented initial state.
            overlapped: Box::new(unsafe { std::mem::zeroed() }),
        };
        watch.issue();
        watch
    }

    /// Asks for the next batch of changes. The buffer and the `OVERLAPPED` are boxed, so they stay where the
    /// request left them until it completes or is cancelled ([`Drop`]).
    fn issue(&mut self) {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_NOTIFY_CHANGE_FILE_NAME, ReadDirectoryChangesW};

        // SAFETY: an all-zero `OVERLAPPED` is the documented initial state.
        *self.overlapped = unsafe { std::mem::zeroed() };
        self.overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
        // SAFETY: the handle is live and overlapped; the buffer and the `OVERLAPPED` outlive the request.
        let ok = unsafe {
            ReadDirectoryChangesW(
                self.dir.as_raw_handle() as HANDLE,
                self.buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&*self.buffer) as u32,
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME,
                std::ptr::null_mut(),
                &mut *self.overlapped,
                None,
            )
        };
        assert!(ok != 0, "ReadDirectoryChangesW: {}", std::io::Error::last_os_error());
    }

    /// Every change since the watch began, in order, up to and including the addition of the name `last`.
    /// The test makes `last` itself once the changes it wants are made, so the wait is for a change that
    /// is bound to come. Windows keeps what happens between two requests for the next one.
    pub(crate) fn until_added(&mut self, last: &str) -> Vec<NameChange> {
        let last = NameChange::Added(last.to_owned());
        self.until(|changes| changes.contains(&last))
    }

    /// Every change since the watch began, in order, up to the batch that makes `done` true of all of them. The
    /// wait is for a change the test knows is bound to come.
    pub(crate) fn until(&mut self, done: impl Fn(&[NameChange]) -> bool) -> Vec<NameChange> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ACTION_ADDED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
        };
        use windows_sys::Win32::System::IO::GetOverlappedResult;

        let mut changes = Vec::new();
        loop {
            let mut returned = 0u32;
            // The kernel writes to the `OVERLAPPED`, so it is passed as a mutable pointer, not a shared reference.
            let overlapped: *mut windows_sys::Win32::System::IO::OVERLAPPED = &raw mut *self.overlapped;
            // SAFETY: the handle and the `OVERLAPPED` of a request in flight; `1` waits for it.
            let ok = unsafe { GetOverlappedResult(self.dir.as_raw_handle() as HANDLE, overlapped, &mut returned, 1) };
            assert!(
                ok != 0,
                "the watch of the directory failed: {}",
                std::io::Error::last_os_error()
            );
            assert!(returned > 0, "the watch's buffer overflowed: changes were lost");
            let bytes: Vec<u8> = self
                .buffer
                .iter()
                .flat_map(|word| word.to_ne_bytes())
                .take(returned as usize)
                .collect();
            self.issue();
            let mut at = 0usize;
            loop {
                let word = |offset: usize| u32::from_ne_bytes(bytes[at + offset..at + offset + 4].try_into().unwrap());
                let (next, action, length) = (word(0) as usize, word(4), word(8) as usize);
                let name: Vec<u16> = bytes[at + 12..at + 12 + length]
                    .chunks_exact(2)
                    .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                    .collect();
                let name = String::from_utf16(&name).unwrap();
                changes.push(match action {
                    FILE_ACTION_ADDED => NameChange::Added(name),
                    FILE_ACTION_REMOVED => NameChange::Removed(name),
                    FILE_ACTION_RENAMED_NEW_NAME => NameChange::RenamedTo(name),
                    FILE_ACTION_RENAMED_OLD_NAME => NameChange::RenamedFrom(name),
                    other => panic!("an action no one asked for: {other}"),
                });
                if next == 0 {
                    break;
                }
                at += next;
            }
            if done(&changes) {
                return changes;
            }
        }
    }
}

#[cfg(windows)]
impl Drop for DirWatch {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult};

        // The request in flight is cancelled and waited out, since the kernel still writes to the buffer.
        let handle = self.dir.as_raw_handle() as HANDLE;
        let mut returned = 0u32;
        // The kernel writes to the `OVERLAPPED`, so it is passed as a mutable pointer, not a shared reference.
        let overlapped: *mut windows_sys::Win32::System::IO::OVERLAPPED = &raw mut *self.overlapped;
        // SAFETY: the handle and `OVERLAPPED` are those of the request in flight.
        unsafe {
            CancelIoEx(handle, overlapped);
            GetOverlappedResult(handle, overlapped, &mut returned, 1);
        }
    }
}

/// Makes the account `sid` (SDDL text) the owner of the existing file `path`, as `icacls /setowner` does, which
/// takes the privilege an administrator needs for it. The test is elevated.
#[cfg(windows)]
pub(crate) fn set_owner(path: &std::path::Path, sid: &str) {
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .args(["/setowner", &format!("*{sid}")])
        .output()
        .expect("run icacls.exe");
    assert!(
        output.status.success(),
        "icacls /setowner *{sid} on {}: {}{}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
