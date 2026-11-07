//! The providers that place goetia's own stock shim: [`EmbeddedStockShim`] from bytes the program
//! carries, and the crate-private `SiblingStockShim` from `goetia-shim.exe` beside the running
//! executable.
//!
//! The shim is a file named for the SHA-256 of its bytes, `goetia-shim-<hex>.exe`, in a folder only
//! Administrators can write to. Several goetia versions can share the folder, each with its own
//! file. `provide` creates the file with the permissions of [`Descriptor`], written under a
//! temporary name and then renamed into place, or verifies the file already there (owner,
//! permissions, bytes against its name) and, when any is wrong, renames it aside and writes a fresh
//! one. Both temporary and aside names are `goetia-shim-<hex>.<random>.stale.exe`, so that what a
//! crash leaves is what a sweep finds.
//!
//! [`shim_file`] builds on every host; the providers are Windows-only.
#![cfg_attr(not(windows), allow(dead_code))]

/// What a file name in a shim folder says about the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShimFile {
    /// `goetia-shim-<hex>.exe`: a shim in place.
    Current,
    /// `goetia-shim-<hex>.<random>.stale.exe`: a shim renamed aside by a repair, or half written.
    StaleAside,
}

/// Which kind of shim file `name` is, matched without regard to case, or `None` for any other name:
/// the hash is exactly 64 hexadecimal digits and the random part is not empty.
pub(crate) fn shim_file(name: &str) -> Option<ShimFile> {
    let lower = name.to_ascii_lowercase();
    let rest = lower.strip_prefix("goetia-shim-")?;
    let (hex, rest) = rest.split_at_checked(64)?;
    if !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    if rest == ".exe" {
        return Some(ShimFile::Current);
    }
    let random = rest.strip_prefix('.')?.strip_suffix(".stale.exe")?;
    (!random.is_empty()).then_some(ShimFile::StaleAside)
}

#[cfg(windows)]
pub use stock::EmbeddedStockShim;
#[cfg(windows)]
pub(crate) use stock::SiblingStockShim;
#[cfg(windows)]
pub(in crate::backend::scm) use stock::default_dir;

#[cfg(windows)]
mod stock {
    use std::ffi::OsString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write as _};
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use sha2::{Digest as _, Sha256};
    use tempfile::Builder;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath};

    use super::super::acl::{
        Descriptor, FolderCheck, ObjectSecurity, is_as_wanted, is_unreadable, read_object, require_acls_under,
    };
    use super::super::marker::read_retrying;
    use super::super::provider::ShimProvider;
    use super::super::sweep::{SweepFailure, sweep_folder};
    use crate::error::{Error, Result};
    use crate::shim::HostCommand;

    /// The end of every name a half-written or replaced shim carries; `goetia-shim-<hex>.` begins it.
    pub(super) const STALE_SUFFIX: &str = ".stale.exe";

    /// The folder under `%ProgramFiles%` that [`EmbeddedStockShim`] and `SiblingStockShim` use by default.
    const DEFAULT_FOLDER: &str = "Goetia";

    /// The stock shim from bytes the program carries: `goetia::embed_shim!()` in a program built with
    /// the stock shim, `None` in one built without.
    pub struct EmbeddedStockShim {
        bytes: Option<&'static [u8]>,
        /// `bytes` with their SHA-256, made by the first use: the bytes are `'static`, and every
        /// `preview` and `provide` of one install needs the digest.
        pub(super) image: OnceLock<Image<'static>>,
        dir: Option<PathBuf>,
    }

    impl EmbeddedStockShim {
        /// `dir` is where the file goes (an absolute path; a relative one is refused when used), or
        /// `None` for `%ProgramFiles%\Goetia`, which is looked up only when a `type: simple`
        /// install or its preview needs it.
        pub fn new(bytes: Option<&'static [u8]>, dir: Option<PathBuf>) -> Self {
            Self {
                bytes,
                image: OnceLock::new(),
                dir,
            }
        }

        fn image(&self) -> Result<&Image<'static>> {
            let bytes = self.bytes.ok_or(Error::ShimNotEmbedded)?;
            Ok(self.image.get_or_init(|| Image::new(bytes)))
        }

        fn dir(&self) -> Result<PathBuf> {
            self.dir.clone().map_or_else(default_dir, Ok)
        }
    }

    impl ShimProvider for EmbeddedStockShim {
        /// `<dir>\goetia-shim-<sha256 of the bytes>.exe`, with no arguments. Without bytes it is
        /// [`Error::ShimNotEmbedded`]; with a relative `dir`, [`Error::ShimPathRelative`].
        fn preview(&self) -> Result<HostCommand> {
            let image = self.image()?;
            preview_in(&self.dir()?, image)
        }

        /// Checks `dir` with `folders`, creates what is missing of it, and leaves the file in place
        /// and verified; see the module documentation. A volume that keeps no ACLs is refused
        /// whatever `folders` says, since there the file's owner and permissions cannot be set or
        /// verified. Runs under the install lock, as every `provide` does: two calls at once can
        /// each set the other's fresh file aside.
        fn provide(&self, folders: &FolderCheck) -> Result<HostCommand> {
            let image = self.image()?;
            provide_in(&self.dir()?, image, folders)
        }

        /// Deletes the shim files in `dir`, and what a repair or a crash left beside them, that no
        /// path of `referenced` names, and removes `dir` when that leaves it empty and it is the
        /// default folder; see [`sweep_folder`].
        fn sweep(&self, referenced: &[PathBuf]) -> std::result::Result<(), SweepFailure> {
            sweep_folder(absolute(&self.dir()?)?, referenced, default_dir)
        }
    }

    /// The stock shim from `goetia-shim.exe` beside the running executable, into the default folder.
    /// The file is read and hashed on every call, since it can change between two.
    pub(crate) struct SiblingStockShim;

    impl SiblingStockShim {
        pub(crate) fn new() -> Self {
            Self
        }

        /// The sibling's bytes, or [`Error::SiblingShimMissing`] naming where it should be.
        fn read(&self) -> Result<Vec<u8>> {
            let exe = std::env::current_exe().map_err(|e| Error::os("locate the running executable", e))?;
            let sibling = exe.with_file_name("goetia-shim.exe");
            fs::read(&sibling).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => Error::SiblingShimMissing { path: sibling },
                _ => Error::os_at("read", sibling, e),
            })
        }
    }

    impl ShimProvider for SiblingStockShim {
        fn preview(&self) -> Result<HostCommand> {
            let bytes = self.read()?;
            preview_in(&default_dir()?, &Image::new(&bytes))
        }

        fn provide(&self, folders: &FolderCheck) -> Result<HostCommand> {
            let bytes = self.read()?;
            provide_in(&default_dir()?, &Image::new(&bytes), folders)
        }

        /// As [`EmbeddedStockShim`]'s, over the default folder, and whether or not the sibling is there.
        fn sweep(&self, referenced: &[PathBuf]) -> std::result::Result<(), SweepFailure> {
            let dir = default_dir()?;
            sweep_folder(&dir, referenced, || Ok(dir.clone()))
        }
    }

    // The stock shim's bytes ==========================================================================================

    /// The stock shim's bytes and their SHA-256, which is the file's name and what it must hold.
    pub(super) struct Image<'a> {
        bytes: &'a [u8],
        digest: [u8; 32],
    }

    impl<'a> Image<'a> {
        pub(super) fn new(bytes: &'a [u8]) -> Self {
            Self {
                bytes,
                digest: Sha256::digest(bytes).into(),
            }
        }

        pub(super) fn digest(&self) -> [u8; 32] {
            self.digest
        }

        /// The digest as 64 lowercase hexadecimal digits.
        pub(super) fn hex(&self) -> String {
            self.digest.iter().map(|byte| format!("{byte:02x}")).collect()
        }

        fn file_name(&self) -> String {
            format!("goetia-shim-{}.exe", self.hex())
        }

        /// What every temporary and aside name of this shim begins with.
        pub(super) fn stale_prefix(&self) -> String {
            format!("goetia-shim-{}.", self.hex())
        }
    }

    /// The SHA-256 of what `reader` yields, over one pass through a fixed buffer.
    pub(super) fn hash_reader(mut reader: impl Read) -> io::Result<[u8; 32]> {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match read_retrying(&mut reader, &mut buf)? {
                0 => return Ok(hasher.finalize().into()),
                n => hasher.update(&buf[..n]),
            }
        }
    }

    // Where it goes ===================================================================================================

    /// `%ProgramFiles%\Goetia`.
    pub(in crate::backend::scm) fn default_dir() -> Result<PathBuf> {
        let mut path: *mut u16 = std::ptr::null_mut();
        // SAFETY: the folder ID is a static; `KF_FLAG_DEFAULT` and no token ask for the machine's
        // folder; `path` is a valid out-pointer.
        let hresult = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, std::ptr::null_mut(), &mut path) };
        let found = if hresult >= 0 {
            let mut len = 0;
            // SAFETY: on success `path` is a NUL-terminated string that is live until freed below,
            // and every unit read is before its NUL.
            while unsafe { *path.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: the `len` units before the NUL were just read.
            Ok(PathBuf::from(OsString::from_wide(unsafe {
                std::slice::from_raw_parts(path, len)
            })))
        } else {
            Err(locate_error(hresult))
        };
        // SAFETY: `path` is null or what `SHGetKnownFolderPath` allocated, which the caller frees
        // with `CoTaskMemFree` whether or not the call succeeded.
        unsafe { CoTaskMemFree(path.cast()) };
        found.map(|program_files| program_files.join(DEFAULT_FOLDER))
    }

    /// The error for `SHGetKnownFolderPath` failing with `hresult`. A Win32 error wrapped as an
    /// `HRESULT` is read as that error.
    pub(super) fn locate_error(hresult: i32) -> Error {
        const FACILITY_WIN32: u32 = 0x8007_0000;
        let code = hresult as u32;
        let source = if code & 0xFFFF_0000 == FACILITY_WIN32 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(format!("HRESULT {code:#010x}"))
        };
        Error::os("locate %ProgramFiles%", source)
    }

    /// `dir`, if it is an absolute path.
    fn absolute(dir: &Path) -> Result<&Path> {
        if dir.is_absolute() {
            Ok(dir)
        } else {
            Err(Error::ShimPathRelative {
                path: dir.to_path_buf(),
            })
        }
    }

    fn preview_in(dir: &Path, image: &Image<'_>) -> Result<HostCommand> {
        Ok(HostCommand {
            exe: absolute(dir)?.join(image.file_name()),
            args: vec![],
        })
    }

    // Provisioning ====================================================================================================

    /// The core of every `provide`: the file for `image` in `dir`, checked, written or repaired.
    fn provide_in(dir: &Path, image: &Image<'_>, folders: &FolderCheck) -> Result<HostCommand> {
        let command = preview_in(dir, image)?;
        folders.check(dir)?;
        require_acls_under(dir)?;
        create_missing(dir)?;
        let file = Descriptor::for_file()?;
        match inspect(&command.exe, image, &file.wanted()?)? {
            Existing::Good => {}
            Existing::Absent => write_fresh(dir, &command.exe, image, &file)?,
            Existing::Bad => {
                rename_aside(dir, &command.exe, image)?;
                write_fresh(dir, &command.exe, image, &file)?;
            }
        }
        Ok(command)
    }

    /// Creates the folders of `dir` that do not exist, outermost first, with the permissions of a
    /// shim folder.
    fn create_missing(dir: &Path) -> Result<()> {
        let mut missing = Vec::new();
        for ancestor in dir.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(ancestor),
                Err(e) => return Err(Error::os_at("read the type of", ancestor, e)),
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let folder = Descriptor::for_folder()?;
        for path in missing.into_iter().rev() {
            folder.create_dir(path).map_err(|e| Error::os_at("create", path, e))?;
        }
        Ok(())
    }

    /// What `inspect` found at the file's path.
    pub(super) enum Existing {
        Absent,
        /// A regular file with the wanted owner and permissions and the bytes its name says.
        Good,
        /// Anything else: another kind of object, or a file that cannot be opened or whose
        /// permissions cannot be read as goetia reads them.
        Bad,
    }

    /// Whether the file at `path` is the shim. Only an answer from the system that says nothing
    /// about the file is an error; a file that refuses to be opened or read is not the shim, and a
    /// repair sets it aside.
    pub(super) fn inspect(path: &Path, image: &Image<'_>, wanted: &ObjectSecurity) -> Result<Existing> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Absent),
            Err(e) => return Err(Error::os_at("read the type of", path, e)),
        };
        if !metadata.file_type().is_file() {
            return Ok(Existing::Bad);
        }
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if refuses_to_open(&e) => return Ok(Existing::Bad),
            Err(e) => return Err(Error::os_at("open", path, e)),
        };
        let actual = match read_object(&file, path.to_path_buf()) {
            Ok(actual) => actual,
            Err(e) if is_unreadable(&e) => return Ok(Existing::Bad),
            Err(e) => return Err(e),
        };
        if !is_as_wanted(&actual, wanted) {
            return Ok(Existing::Bad);
        }
        let found = hash_reader(&file).map_err(|e| Error::os_at("read", path, e))?;
        Ok(if found == image.digest() {
            Existing::Good
        } else {
            Existing::Bad
        })
    }

    /// Whether `error` from opening a file that exists is the file's doing: its permissions refuse
    /// reading, or another process holds it without sharing.
    fn refuses_to_open(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
    }

    /// Renames `path` to a name no other file has, which `shim_file` calls
    /// [`StaleAside`](super::ShimFile::StaleAside). The name is reserved by creating a file under it,
    /// which the rename then replaces, so no existing file is.
    fn rename_aside(dir: &Path, path: &Path, image: &Image<'_>) -> Result<()> {
        let prefix = image.stale_prefix();
        let reserved = Builder::new()
            .prefix(&prefix)
            .suffix(STALE_SUFFIX)
            .make_in(dir, |reserved| {
                OpenOptions::new().write(true).create_new(true).open(reserved)
            })
            .map_err(|e| Error::os_at("create a file in", dir, e))?
            .into_temp_path();
        fs::rename(path, &reserved).map_err(|e| Error::os_at("rename aside", path, e))?;
        // The reserved name is the file's now: it stays.
        reserved.keep().map(|_| ()).map_err(|e| {
            let path = e.path.to_path_buf();
            Error::os_at("keep", path, e.error)
        })
    }

    /// Writes `image` to `path`, which does not exist, as a file of `wanted`'s permissions: under a
    /// temporary name in `dir`, then renamed into place, so that `path` never holds half a file. A
    /// file that appeared at `path` meanwhile is not replaced.
    fn write_fresh(dir: &Path, path: &Path, image: &Image<'_>, wanted: &Descriptor) -> Result<()> {
        let prefix = image.stale_prefix();
        let (mut file, temporary) = Builder::new()
            .prefix(&prefix)
            .suffix(STALE_SUFFIX)
            .make_in(dir, |temporary| wanted.create_file(temporary))
            .map_err(|e| Error::os_at("create a file in", dir, e))?
            .into_parts();
        let written = file.write_all(image.bytes).and_then(|()| file.flush());
        // Closed before the file is renamed into place, or deleted for a failed write: the handle
        // allows neither.
        drop(file);
        written.map_err(|e| Error::os_at("write", &temporary, e))?;
        temporary
            .persist_noclobber(path)
            .map_err(|e| Error::os_at("place", path, e.error))
    }
}

#[cfg(test)]
#[path = "embedded_tests.rs"]
mod embedded_tests;
