//! The shim permission check: refuses a folder, or a host file in one, that an account other than
//! SYSTEM, Administrators or TrustedInstaller could tamper with, since the shim runs as
//! `LocalSystem` and whoever can replace its file owns the machine.
//!
//! The check is a static scan of the owner and DACL of the folder (or of the file and its folder)
//! and of every ancestor up to the volume root, over the path as given and over its final path
//! (links resolved). [`verdict`]
//! judges plain data (SIDs as bytes, masks as `u32`) and is unit-tested on every host; reading the
//! data and [`FolderCheck`] are Windows-only.
//!
//! What is judged, per object, folders before files and each nearest the target first:
//! - an untrusted owner refuses (an owner can always change the DACL);
//! - a NULL DACL refuses (everyone has every right);
//! - an allow ACE that applies to the object, granting an untrusted account `WRITE_DAC`,
//!   `WRITE_OWNER`, `DELETE` or, on a directory, `FILE_DELETE_CHILD`, or, on a file,
//!   `FILE_WRITE_DATA` or `FILE_APPEND_DATA`, after generic rights are mapped, refuses. A file's
//!   extended attributes and attributes (`FILE_WRITE_EA`, `FILE_WRITE_ATTRIBUTES`) change neither
//!   what it runs nor who may change it, so they are accepted. Accounts that may only add files to
//!   a directory are accepted, and inherit-only ACEs grant nothing here;
//! - deny ACEs are not credited, wherever they stand.
#![cfg_attr(not(windows), allow(dead_code))]

mod protected;

use std::path::PathBuf;

#[cfg(windows)]
pub(crate) use protected::{Descriptor, is_as_wanted};
#[cfg(windows)]
pub(crate) use system::{
    display_path, is_unreadable, kernel_object_owner, normalise as lexical, read_object, require_acls_under,
};

#[cfg(windows)]
use std::path::Path;

#[cfg(windows)]
use crate::backend::scm::manager::identity;
#[cfg(windows)]
use crate::error::{Error, Result};

/// A security identifier in its binary form, as `LookupAccountNameW` and `GetAce` return it.
pub(crate) type Sid = Vec<u8>;

/// The owner and DACL of one filesystem object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectSecurity {
    pub(crate) path: PathBuf,
    pub(crate) owner: Sid,
    /// Whether the DACL ignores what its parent would have it inherit.
    pub(crate) protected: bool,
    /// Whether it is a directory (or a link to one), whose rights mean something other than a
    /// file's: see [`verdict`].
    pub(crate) directory: bool,
    /// `None` is a NULL DACL, which grants everyone every right.
    pub(crate) dacl: Option<Vec<Ace>>,
}

/// An access-control entry that grants or denies a trustee a mask of rights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ace {
    pub(crate) trustee: Sid,
    pub(crate) mask: u32,
    /// The ACE header's flags, of which only `INHERIT_ONLY_ACE` is read.
    pub(crate) flags: u8,
    pub(crate) allow: bool,
}

/// The first reason found to refuse a folder or a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unsafe {
    pub(crate) path: PathBuf,
    /// Who holds the right. A SID, not a name: naming it is a lookup, which `verdict` does not make.
    pub(crate) account: Sid,
    /// A verb phrase that `{account} can {right} it` completes.
    pub(crate) right: &'static str,
}

const INHERIT_ONLY_ACE: u8 = 0x08;

const DELETE: u32 = 0x1_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
const FILE_DELETE_CHILD: u32 = 0x40;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;

// `FILE_GENERIC_MAPPING`'s generic rights, and what each maps to on a file or directory.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_GENERIC_READ: u32 = 0x12_0089;
const FILE_GENERIC_WRITE: u32 = 0x12_0116;
const FILE_GENERIC_EXECUTE: u32 = 0x12_00A0;
const FILE_ALL_ACCESS: u32 = 0x1F_01FF;

/// The rights that let an account replace what is in a folder, in the order they are reported.
const DIRECTORY_TAMPERING: [(u32, &str); 4] = [
    (WRITE_DAC, "change permissions of"),
    (WRITE_OWNER, "take ownership of"),
    (DELETE, "delete or rename"),
    (FILE_DELETE_CHILD, "delete or rename entries in"),
];

/// The rights that let an account change a file's contents, or take it away or hand it over, in the
/// order they are reported.
const FILE_TAMPERING: [(u32, &str); 4] = [
    (FILE_WRITE_DATA | FILE_APPEND_DATA, "write to"),
    (WRITE_DAC, "change permissions of"),
    (WRITE_OWNER, "take ownership of"),
    (DELETE, "delete or rename"),
];

// SIDs ================================================================================================================

/// The binary SID with `authority` as its identifier authority and `subauthorities` after it.
pub(crate) fn sid_from_parts(authority: u8, subauthorities: &[u32]) -> Sid {
    let mut sid = vec![1, subauthorities.len() as u8, 0, 0, 0, 0, 0, authority];
    for sub in subauthorities {
        sid.extend_from_slice(&sub.to_le_bytes());
    }
    sid
}

fn system() -> Sid {
    sid_from_parts(5, &[18])
}

fn administrators() -> Sid {
    sid_from_parts(5, &[32, 544])
}

fn trusted_installer() -> Sid {
    sid_from_parts(5, &[80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464])
}

fn everyone() -> Sid {
    sid_from_parts(1, &[0])
}

/// `S-1-5-32-544`, or `None` when the bytes are not a SID.
pub(crate) fn sid_text(sid: &[u8]) -> Option<String> {
    let (&1, &count) = (sid.first()?, sid.get(1)?) else {
        return None;
    };
    let count = usize::from(count);
    if count > 15 || sid.len() != 8 + 4 * count {
        return None;
    }
    let authority = sid[2..8].iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    let mut text = if authority < 1 << 32 {
        format!("S-1-{authority}")
    } else {
        format!("S-1-{authority:#x}")
    };
    for &sub in sid[8..].as_chunks::<4>().0 {
        text.push('-');
        text.push_str(&u32::from_le_bytes(sub).to_string());
    }
    Some(text)
}

/// [`sid_text`], or a description of bytes that are not a SID. What an error names an account by
/// when `LookupAccountSidW` has no name for it.
pub(crate) fn sid_string(sid: &[u8]) -> String {
    sid_text(sid).unwrap_or_else(|| "an invalid SID".to_string())
}

// verdict =============================================================================================================

/// Judges `chain` (the target first, then outward to the volume root) by the rules in the module
/// documentation. Each object is judged by its own kind: a file by [`FILE_TAMPERING`], a directory
/// by [`DIRECTORY_TAMPERING`]. The folders are judged before any file, since a file inherits what
/// its folder grants and the folder is what to fix; each kind in the order of `chain`.
pub(crate) fn verdict(chain: &[ObjectSecurity]) -> std::result::Result<(), Unsafe> {
    let folders = chain.iter().filter(|object| object.directory);
    let files = chain.iter().filter(|object| !object.directory);
    for object in folders.chain(files) {
        let refuse = |account: &Sid, right: &'static str| Unsafe {
            path: object.path.clone(),
            account: account.clone(),
            right,
        };
        if !is_trusted(&object.owner) {
            return Err(refuse(&object.owner, "own"));
        }
        let Some(dacl) = &object.dacl else {
            return Err(refuse(&everyone(), "do anything to"));
        };
        let tampering = if object.directory {
            &DIRECTORY_TAMPERING
        } else {
            &FILE_TAMPERING
        };
        let applying = dacl
            .iter()
            .filter(|ace| ace.allow && ace.flags & INHERIT_ONLY_ACE == 0 && !is_trusted(&ace.trustee));
        for ace in applying {
            let mask = map_generic(ace.mask);
            if let Some((_, right)) = tampering.iter().find(|(bit, _)| mask & bit != 0) {
                return Err(refuse(&ace.trustee, right));
            }
        }
    }
    Ok(())
}

/// Whether `sid` is one of the accounts nobody else can be told from: SYSTEM, Administrators and
/// TrustedInstaller. What may own or change a shim folder, and own the install lock.
pub(crate) fn is_trusted(sid: &Sid) -> bool {
    [system(), administrators(), trusted_installer()].contains(sid)
}

/// `mask` with each generic right replaced by the file rights it stands for.
fn map_generic(mask: u32) -> u32 {
    let mut mapped = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
    for (generic, specific) in [
        (GENERIC_READ, FILE_GENERIC_READ),
        (GENERIC_WRITE, FILE_GENERIC_WRITE),
        (GENERIC_EXECUTE, FILE_GENERIC_EXECUTE),
        (GENERIC_ALL, FILE_ALL_ACCESS),
    ] {
        if mask & generic != 0 {
            mapped |= specific;
        }
    }
    mapped
}

// FolderCheck =========================================================================================================

/// Whether the manager checks the shim folder's permissions.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckPermissions {
    Enable,
    /// For a folder whose protection the caller guarantees some other way. Skips only this check.
    Disable,
}

/// Refuses a folder that an account other than SYSTEM, Administrators or TrustedInstaller could
/// tamper with. See the module documentation for what is judged.
#[cfg(windows)]
pub struct FolderCheck {
    mode: CheckPermissions,
}

#[cfg(windows)]
impl FolderCheck {
    pub(crate) fn new(mode: CheckPermissions) -> Self {
        Self { mode }
    }

    /// `Ok` under [`Disable`](CheckPermissions::Disable), without reading anything. Otherwise
    /// reads `path`, a folder or a host file, which need not exist (its first existing ancestor
    /// stands for it), and every ancestor, and refuses with [`Error::ShimFolderUnsafe`] naming the
    /// first object and account found to be unsafe. A file is refused too when an untrusted account
    /// can write to it, change its permissions, take it over or delete it; a link at `path` is read
    /// as itself, and so is what it leads to. An object that cannot be read is an error, never a
    /// pass.
    pub fn check(&self, path: &Path) -> Result<()> {
        if self.mode == CheckPermissions::Disable {
            return Ok(());
        }
        verdict(&system::read_chain(path)?).map_err(Unsafe::into_error)
    }
}

#[cfg(windows)]
impl Unsafe {
    /// The error for this refusal. An account with no name (a deleted one) is written as its SID.
    fn into_error(self) -> Error {
        Error::ShimFolderUnsafe {
            account: account_name(&self.account),
            path: self.path,
            right: self.right,
        }
    }
}

/// The name of the account `sid` is, or the SID's text when no account has it (a deleted one).
#[cfg(windows)]
pub(crate) fn account_name(sid: &Sid) -> String {
    identity::account_name_of_sid(sid).unwrap_or_else(|_| sid_string(sid))
}

// The filesystem ======================================================================================================

#[cfg(windows)]
mod system {
    use std::ffi::{OsString, c_void};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::path::{Component, Path, PathBuf};

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetLengthSid,
        GetSecurityDescriptorControl, IsValidSid, OWNER_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        GetVolumeInformationByHandleW, READ_CONTROL,
    };

    use super::{Ace, ObjectSecurity, Sid, Unsafe, everyone};
    use crate::error::{Error, Result};

    // The ACE types a file's DACL can hold: allow and deny, plain or conditional. A conditional ACE
    // has an expression after its SID, which is not read: it is treated as if it always applies.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
    const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;

    /// A volume flag: it keeps ACLs. Without it no access control exists to check.
    const FILE_PERSISTENT_ACLS: u32 = 0x8;

    /// The security of `folder` and of everything it is reached through: the target first, then
    /// outward to the volume root. Holds each component of `folder` as given, each link among
    /// them read as itself, the components of every such link's target and of that target's own
    /// links in turn, and each component of the final path, once. A `folder` that does not exist
    /// is represented by its first existing ancestor, since a provider creates it with its own
    /// permissions.
    pub(super) fn read_chain(folder: &Path) -> Result<Vec<ObjectSecurity>> {
        let given = std::path::absolute(folder).map_err(|e| Error::os_at("resolve", folder, e))?;
        let start = first_existing(&given)?;

        let mut walk = Walk::default();
        walk.add(&start)?;
        let final_path = std::fs::canonicalize(&start).map_err(|e| Error::os_at("resolve", &start, e))?;
        walk.add(&final_path)?;
        Ok(walk.chain)
    }

    /// `path` as it is shown and compared: a final path's `\\?\C:\` is `C:\` and its `\\?\UNC\` is
    /// `\\`, so it equals the path it was given as. Any other verbatim path (`\\?\Volume{..}\`) is
    /// kept, since without its prefix it would name something else. Only a label: open the path.
    pub(crate) fn display_path(path: &Path) -> PathBuf {
        const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
        const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let Some(rest) = wide.strip_prefix(&VERBATIM) else {
            return path.to_path_buf();
        };
        if let Some(share) = rest.strip_prefix(&UNC) {
            let mut unc = vec![b'\\' as u16, b'\\' as u16];
            unc.extend_from_slice(share);
            return PathBuf::from(OsString::from_wide(&unc));
        }
        let drive = match rest {
            [letter, colon, ..] => {
                *colon == b':' as u16 && u8::try_from(*letter).is_ok_and(|b| b.is_ascii_alphabetic())
            }
            _ => false,
        };
        if drive {
            PathBuf::from(OsString::from_wide(rest))
        } else {
            path.to_path_buf()
        }
    }

    /// The first of `path` and its ancestors that exists.
    fn first_existing(path: &Path) -> Result<PathBuf> {
        for candidate in path.ancestors() {
            match open(candidate) {
                Ok(_) => return Ok(candidate.to_path_buf()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::os_at("read the permissions of", candidate, e)),
            }
        }
        Err(Error::os_at(
            "find an existing ancestor of",
            path,
            io::Error::from(io::ErrorKind::NotFound),
        ))
    }

    /// Which object a handle is open on: the volume's serial number and the file's ID, which no
    /// spelling of a path changes.
    type ObjectId = (u64, [u8; 16]);

    /// The chain being read, and the objects in it.
    #[derive(Default)]
    struct Walk {
        chain: Vec<ObjectSecurity>,
        seen: Vec<ObjectId>,
    }

    impl Walk {
        /// Adds `path` and each of its ancestors, nearest first, unless the walk has the object
        /// already. A component that is a link also adds its target's chain, and a link already
        /// seen is not followed again, which ends a loop of links. Opens each component by the
        /// path it was given (a final path is verbatim, and without the prefix Win32 would
        /// rewrite some names); the path is only the object's label.
        fn add(&mut self, path: &Path) -> Result<()> {
            for component in path.ancestors() {
                let file = open(component).map_err(|e| Error::os_at("read the permissions of", component, e))?;
                let shown = display_path(component);
                // First: a volume without ACLs may not answer for identity either.
                require_acls(file.as_raw_handle() as HANDLE, &shown)?;
                let id = object_id(&file, component)?;
                if self.seen.contains(&id) {
                    continue;
                }
                self.seen.push(id);
                self.chain.push(read_object(&file, shown)?);
                let is_link = file
                    .metadata()
                    .map_err(|e| Error::os_at("read the type of", component, e))?
                    .file_type()
                    .is_symlink();
                if is_link {
                    let target = link_target(component)?;
                    self.add(&target)?;
                }
            }
            Ok(())
        }
    }

    /// The object `file` is open on. A volume or server that has no file ID (FAT, some SMB
    /// shares) is identified by the older volume serial and file index.
    fn object_id(file: &File, path: &Path) -> Result<ObjectId> {
        identity_of(file_id(file), || file_index(file)).map_err(|e| Error::os_at("identify", path, e))
    }

    /// `by_id`, or when it failed, `by_index`; an error only when both did.
    pub(super) fn identity_of(
        by_id: io::Result<ObjectId>,
        by_index: impl FnOnce() -> io::Result<ObjectId>,
    ) -> io::Result<ObjectId> {
        by_id.or_else(|_| by_index())
    }

    fn file_id(file: &File) -> io::Result<ObjectId> {
        let mut info = FILE_ID_INFO {
            VolumeSerialNumber: 0,
            FileId: Default::default(),
        };
        // SAFETY: `file` is open with `FILE_READ_ATTRIBUTES`; `info` is a `FILE_ID_INFO` and the
        // size passed is its own.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                std::ptr::addr_of_mut!(info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    /// The 64-bit file index, widened to an [`ObjectId`] with the volume's 32-bit serial.
    fn file_index(file: &File) -> io::Result<ObjectId> {
        // SAFETY: an all-zero `BY_HANDLE_FILE_INFORMATION` is a valid value of plain integers.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `file` is open with `FILE_READ_ATTRIBUTES`; `info` is a valid out-pointer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&index.to_le_bytes());
        Ok((u64::from(info.dwVolumeSerialNumber), id))
    }

    /// Where the link at `link` leads, as one hop, with `.` and `..` applied lexically. A relative
    /// target is resolved against the final path of the link's parent, since a link on the way to
    /// `link` can lead the parent elsewhere than its spelling suggests. A target with a drive and
    /// no root (`C:foo`) is refused: it means the current directory of that drive in the process
    /// that follows the link, which is not this one. A verbatim target is kept as it is, since
    /// the system does not interpret `..` in one.
    fn link_target(link: &Path) -> Result<PathBuf> {
        let target = std::fs::read_link(link).map_err(|e| Error::os_at("read the link", link, e))?;
        resolve_target(link, target, || {
            let parent = link.parent().unwrap_or(link);
            std::fs::canonicalize(parent).map_err(|e| Error::os_at("resolve", parent, e))
        })
    }

    /// [`link_target`] over the target as read; `parent_final` gives the final path of the link's
    /// parent, and is asked for only when the target is relative.
    pub(super) fn resolve_target(
        link: &Path,
        target: PathBuf,
        parent_final: impl FnOnce() -> Result<PathBuf>,
    ) -> Result<PathBuf> {
        if target.is_absolute() {
            return Ok(match target.components().next() {
                Some(Component::Prefix(prefix)) if prefix.kind().is_verbatim() => target,
                _ => normalise(&target),
            });
        }
        if !target.has_root() && matches!(target.components().next(), Some(Component::Prefix(_))) {
            return Err(Error::os_at(
                "follow the link",
                link,
                io::Error::other(format!(
                    "its target {} is relative to the current directory of a drive, which the process that follows it decides",
                    target.display()
                )),
            ));
        }
        Ok(normalise(&parent_final()?.join(target)))
    }

    /// `path` with its `.` and `..` components applied lexically.
    pub(crate) fn normalise(path: &Path) -> PathBuf {
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                other => resolved.push(other),
            }
        }
        resolved
    }

    /// Opens `path` for its security alone. A link is opened as itself, not followed.
    fn open(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    /// The owner and DACL of an object on a volume that keeps ACLs ([`require_acls`]).
    pub(crate) fn read_object(file: &File, path: PathBuf) -> Result<ObjectSecurity> {
        let handle = file.as_raw_handle() as HANDLE;

        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        // SAFETY: `handle` is open, with `READ_CONTROL`, for as long as `file` is borrowed; the
        // out-pointers are valid lvalues. On success `descriptor` owns the memory `owner` and
        // `dacl` point into, and is freed below.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(Error::os_at(
                "read the permissions of",
                path,
                io::Error::from_raw_os_error(status as i32),
            ));
        }
        let directory = file.metadata().map(|metadata| {
            std::os::windows::fs::MetadataExt::file_attributes(&metadata) & FILE_ATTRIBUTE_DIRECTORY != 0
        });
        // SAFETY: `owner` and `dacl` point into `descriptor`, which is live until the free below.
        let object = match directory {
            Ok(directory) => unsafe { describe(descriptor, owner, dacl, path, directory) },
            Err(e) => Err(Error::os_at("read the type of", path, e)),
        };
        // SAFETY: `descriptor` was allocated by `GetSecurityInfo` and is freed once, after the
        // last read of what points into it.
        unsafe {
            LocalFree(descriptor);
        }
        object
    }

    /// The owner of the kernel object `handle` (a mutex, say), which must be open with
    /// `READ_CONTROL`. `name` stands for the object in an error.
    pub(crate) fn kernel_object_owner(handle: HANDLE, name: &Path) -> Result<Sid> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        // SAFETY: `handle` is open with `READ_CONTROL` for the call; the out-pointers are valid
        // lvalues. On success `descriptor` owns the memory `owner` points into, freed below.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(Error::os_at(
                "read the owner of",
                name,
                io::Error::from_raw_os_error(status as i32),
            ));
        }
        // SAFETY: `owner` points into `descriptor`, which is live until the free below.
        let copied = unsafe { copy_sid(owner, name) };
        // SAFETY: `descriptor` was allocated by `GetSecurityInfo` and is freed once, after the last
        // read of what points into it.
        unsafe {
            LocalFree(descriptor);
        }
        copied
    }

    /// Copies the owner, the DACL and whether it is protected out of a security descriptor of a
    /// directory or, when `directory` is false, a file.
    ///
    /// # Safety
    /// `descriptor` is a live security descriptor, and `owner` and `dacl` point into it (the DACL
    /// null for a NULL DACL).
    pub(super) unsafe fn describe(
        descriptor: *mut c_void,
        owner: PSID,
        dacl: *mut ACL,
        path: PathBuf,
        directory: bool,
    ) -> Result<ObjectSecurity> {
        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: the caller's contract covers `descriptor`; the out-pointers are valid lvalues.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(Error::os_at(
                "read the permissions of",
                path,
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: the caller's contract covers `owner` and `dacl`.
        let (owner, dacl) = unsafe { parse(owner, dacl, &path)? };
        Ok(ObjectSecurity {
            path,
            owner,
            protected: control & SE_DACL_PROTECTED != 0,
            directory,
            dacl,
        })
    }

    /// Refuses a volume that keeps no ACLs, where every account can do anything.
    fn require_acls(handle: HANDLE, path: &Path) -> Result<()> {
        let mut flags: u32 = 0;
        // SAFETY: `handle` is open; the one out-pointer asked for is a valid `u32`, and the rest
        // are null with zero lengths.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(Error::os_at("read the volume of", path, io::Error::last_os_error()));
        }
        if flags & FILE_PERSISTENT_ACLS == 0 {
            return Err(Unsafe {
                path: path.to_path_buf(),
                account: everyone(),
                right: "bypass access control on",
            }
            .into_error());
        }
        Ok(())
    }

    /// What a read of an object's permissions fails with when the object holds something goetia
    /// cannot read (an entry of a type it does not know, an account that is no SID), as opposed to
    /// the system failing to answer. Carried as the error's source; see [`is_unreadable`].
    #[derive(Debug)]
    struct Unreadable(String);

    impl std::fmt::Display for Unreadable {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::error::Error for Unreadable {}

    fn unreadable(path: &Path, what: String) -> Error {
        Error::os_at("read the permissions of", path, Unreadable(what))
    }

    /// Whether `error` is a read of permissions that failed because the object holds something
    /// goetia cannot read, which says something about the object; any other failure says nothing.
    pub(crate) fn is_unreadable(error: &Error) -> bool {
        matches!(error, Error::Os { source, .. } if source.is::<Unreadable>())
    }

    /// Refuses with [`Unsafe`]'s error when the volume of `dir`, or of its first existing ancestor,
    /// keeps no ACLs: on it no owner or permissions can be set or read back.
    pub(crate) fn require_acls_under(dir: &Path) -> Result<()> {
        let given = std::path::absolute(dir).map_err(|e| Error::os_at("resolve", dir, e))?;
        let start = first_existing(&given)?;
        let file = open(&start).map_err(|e| Error::os_at("read the permissions of", &start, e))?;
        require_acls(file.as_raw_handle() as HANDLE, &display_path(&start))
    }

    /// Copies the owner and the entries out of a security descriptor.
    ///
    /// # Safety
    /// `owner` and `dacl` are what `GetSecurityInfo` returned, and the descriptor they point into
    /// is live.
    pub(super) unsafe fn parse(owner: PSID, dacl: *mut ACL, path: &Path) -> Result<(Sid, Option<Vec<Ace>>)> {
        // SAFETY: the caller's contract covers `owner`.
        let owner = unsafe { copy_sid(owner, path)? };
        if dacl.is_null() {
            return Ok((owner, None));
        }
        // SAFETY: `dacl` is non-null and live, so its header is readable.
        let count = unsafe { (*dacl).AceCount };
        let mut aces = Vec::with_capacity(usize::from(count));
        for index in 0..u32::from(count) {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `dacl` is live and `index` is below its entry count; `ace` is a valid out-pointer.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(Error::os_at(
                    "read the permissions of",
                    path,
                    io::Error::last_os_error(),
                ));
            }
            // SAFETY: `GetAce` returned a pointer to an entry inside `dacl`, whose every type read
            // here begins with the header, the mask and then the trustee's SID.
            let (header, mask, trustee) = unsafe {
                let body = ace as *const ACCESS_ALLOWED_ACE;
                (
                    *(ace as *const ACE_HEADER),
                    (*body).Mask,
                    std::ptr::addr_of!((*body).SidStart) as PSID,
                )
            };
            let allow = match header.AceType {
                ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => true,
                ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE => false,
                other => {
                    return Err(unreadable(
                        path,
                        format!("an entry of type {other}, which goetia cannot read"),
                    ));
                }
            };
            aces.push(Ace {
                // SAFETY: `trustee` points at the SID inside the entry just read.
                trustee: unsafe { copy_sid(trustee, path)? },
                mask,
                flags: header.AceFlags,
                allow,
            });
        }
        Ok((owner, Some(aces)))
    }

    /// # Safety
    /// `sid` is null or points at readable memory.
    unsafe fn copy_sid(sid: PSID, path: &Path) -> Result<Sid> {
        // SAFETY: `IsValidSid` checks the structure before `GetLengthSid` reads it; a null pointer
        // is rejected first.
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return Err(unreadable(path, "an account that is not a valid SID".to_string()));
        }
        // SAFETY: valid per the check above, so `GetLengthSid` bytes are readable from `sid`.
        let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize) };
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
#[path = "acl_tests.rs"]
pub(in crate::backend::scm::shim) mod acl_tests;
