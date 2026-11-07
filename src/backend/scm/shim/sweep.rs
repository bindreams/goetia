//! The sweep of shim files nothing references: which files, what references a file, and the
//! deletion.
//!
//! A shim file is referenced by a service whose `ImagePath` names it and by a process that runs it
//! as its image. Neither is enough alone: SCM records a service `STOPPED` before its host exits, and
//! a host that was updated to a new file keeps running the old one until it restarts. So a file is
//! swept only when no registered service and no running process names it, and a failed delete is
//! reported rather than read as "in use".
//!
//! [`unreferenced`] is pure over paths and builds on every host; the rest is Windows-only.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::embedded::shim_file;

/// The files of `files` that are shim files (see [`shim_file`]) and that none of `referenced`
/// names, in the order given. Paths are compared without regard to case, and are expected
/// `normalise`d. A referenced path with no folder is a file name alone, which names every file of
/// that name: how a process whose image could not be read is known.
pub(crate) fn unreferenced(files: &[PathBuf], referenced: &[PathBuf]) -> Vec<PathBuf> {
    let named_by = |file: &Path, reference: &Path| {
        if is_bare(reference) {
            name_key(file) == name_key(reference)
        } else {
            same_path(file, reference)
        }
    };
    files
        .iter()
        .filter(|file| {
            let is_shim = file
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(shim_file)
                .is_some();
            is_shim && !referenced.iter().any(|reference| named_by(file, reference))
        })
        .cloned()
        .collect()
}

/// Every path SCM may start for the command line `image` (an `ImagePath` with its environment
/// variables expanded): a quoted one is the quoted part; an unquoted one may end before any space,
/// since SCM tries each, or run to the end. All of them are named, so a service registered either
/// way references the file it starts. More paths than SCM starts only keep more files.
pub(crate) fn host_candidates(image: &str) -> Vec<String> {
    let image = image.trim();
    if let Some(quoted) = image.strip_prefix('"') {
        let path = quoted.split('"').next().unwrap_or_default();
        return if path.is_empty() {
            vec![]
        } else {
            vec![path.to_string()]
        };
    }
    let mut candidates: Vec<String> = Vec::new();
    let ends = image
        .char_indices()
        .filter(|&(_, c)| c == ' ' || c == '\t')
        .map(|(at, _)| at)
        .chain(std::iter::once(image.len()));
    for end in ends {
        let prefix = image[..end].trim_end();
        if !prefix.is_empty() && candidates.last().is_none_or(|last| last != prefix) {
            candidates.push(prefix.to_string());
        }
    }
    candidates
}

/// Whether `a` and `b` are one path, compared without regard to case.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Whether `path` is a file name with no folder.
fn is_bare(path: &Path) -> bool {
    path.parent().is_none_or(|folder| folder.as_os_str().is_empty())
}

fn name_key(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(windows)]
pub(crate) use system::{normalise, referenced_hosts, registered_hosts, sweep_folder};

#[cfg(windows)]
mod system {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

    use super::super::acl::{display_path, lexical};
    use super::super::embedded::default_dir;
    use super::super::processes::running_images;
    use super::{host_candidates, same_path, unreferenced};
    use crate::backend::scm::manager::registry;
    use crate::error::{Error, Reason, Result};
    use crate::manager::Installed;

    /// `path` as one spelling of the file it names, so that two spellings compare equal: the final
    /// path name of a file that exists (long names for 8.3 aliases, links followed, the `\\?\` prefix
    /// dropped, the case it is stored in), and `.` and `..` applied lexically for any other path.
    pub(crate) fn normalise(path: &Path) -> PathBuf {
        match fs::canonicalize(path) {
            Ok(final_path) => display_path(&final_path),
            Err(_) => lexical(path),
        }
    }

    // What references a file ------------------------------------------------------------------------------------------

    /// Every executable SCM may start for any service's `ImagePath` (see [`host_candidates`]), with
    /// its environment variables expanded, [`normalise`]d. A
    /// service marked for deletion references nothing (its record outlives it until a handle
    /// closes), and one with no `ImagePath` has nothing to start. A service whose entry could not be
    /// read, or an enumeration that did not finish, is an error: what the unread entries reference is
    /// unknown, and it must not be taken for nothing.
    pub(crate) fn registered_hosts() -> Result<Vec<PathBuf>> {
        let mut hosts = Vec::new();
        let mut failed = Vec::new();
        let unfinished = registry::scan_services(|name| match registry::read_registered_image(&name) {
            Ok(Some(image)) => hosts.extend(
                host_candidates(&expand_environment(&image))
                    .iter()
                    .map(|candidate| normalise(Path::new(candidate))),
            ),
            Ok(None) => {}
            Err(e) => failed.push(e),
        });
        match Error::both_all(failed.into_iter().chain(unfinished.map(why_unfinished))) {
            None => Ok(hosts),
            Some(e) => Err(Error::Step {
                what: "read which services run which host".to_string(),
                source: Box::new(e),
            }),
        }
    }

    /// `text` with each `%NAME%` replaced by the variable's value, as the system expands a
    /// `REG_EXPAND_SZ`; a name that is not set stays as written.
    pub(super) fn expand_environment(text: &str) -> String {
        let source: Vec<u16> = std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `source` is NUL-terminated and `buf` is `buf.len()` writable units.
            let needed =
                unsafe { ExpandEnvironmentStringsW(source.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
            if needed == 0 {
                // Nothing could be expanded: the text as it was names what it names.
                return text.to_string();
            }
            if needed <= buf.len() {
                // `needed` counts the NUL.
                return OsString::from_wide(&buf[..needed - 1]).to_string_lossy().into_owned();
            }
            buf.resize(needed, 0);
        }
    }

    /// The error that stands for an enumeration that stopped, which `scan_services` reports as the
    /// `list` entry it leaves behind.
    fn why_unfinished(entry: Installed) -> Error {
        match entry {
            Installed::Undetermined {
                reason: Reason::Around { error, .. } | Reason::Failed(error),
                ..
            } => Error::Kept(error),
            Installed::Undetermined { reason, .. } => {
                Error::os("enumerate the services", io::Error::other(reason.to_string()))
            }
            _ => Error::os(
                "enumerate the services",
                io::Error::other("the enumeration did not finish"),
            ),
        }
    }

    /// Every file a registered service or a running process names: [`registered_hosts`] and
    /// [`running_images`].
    pub(crate) fn referenced_hosts() -> Result<Vec<PathBuf>> {
        let mut referenced = registered_hosts()?;
        referenced.extend(running_images()?);
        Ok(referenced)
    }

    // The deletion ----------------------------------------------------------------------------------------------------

    /// Deletes the shim files in `dir`, and what a repair or a crash left beside them, that no path
    /// of `referenced` names, and removes `dir` when that leaves it empty and it is the default
    /// folder. Every file is tried: the failures, each `Os { what: "delete", path }`, come back
    /// together after the last, in the order the files were tried. A `dir` that does not exist has
    /// nothing to sweep.
    ///
    /// The one mechanism behind a provider's `sweep` and the sweep of the folder an uninstalled
    /// service's host was in.
    pub(crate) fn sweep_folder(dir: &Path, referenced: &[PathBuf]) -> Result<()> {
        let swept = sweep_files(dir, referenced);
        let removed = remove_if_empty_default(dir, || Ok(same_path(&normalise(&default_dir()?), &normalise(dir))));
        match Error::both_all(swept.err().into_iter().chain(removed.err())) {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Removes `dir` if it is empty and `is_default` says it is the default folder, which is asked
    /// only of a folder that is empty: where the default folder is matters for nothing else.
    pub(super) fn remove_if_empty_default(dir: &Path, is_default: impl FnOnce() -> Result<bool>) -> Result<()> {
        match fs::read_dir(dir) {
            Ok(mut listing) => {
                if listing.next().is_some() {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::os_at("read", dir, e)),
        }
        if !is_default()? {
            return Ok(());
        }
        match fs::remove_dir(dir) {
            Err(e) if !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => {
                Err(Error::os_at("delete", dir, e))
            }
            _ => Ok(()),
        }
    }

    /// The deletion half of [`sweep_folder`].
    fn sweep_files(dir: &Path, referenced: &[PathBuf]) -> Result<()> {
        let mut failures = Vec::new();
        let mut entries = Vec::new();
        match fs::read_dir(dir) {
            Ok(listing) => {
                for entry in listing {
                    match entry {
                        // A name that is no shim file's is dropped by `unreferenced`; a folder that a
                        // repair set aside whole is a shim file's name too.
                        Ok(entry) => entries.push((entry.file_name(), entry.file_type().is_ok_and(|t| t.is_dir()))),
                        Err(e) => failures.push(Error::os_at("read an entry of", dir, e)),
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::os_at("read", dir, e)),
        }
        entries.sort();

        let base = normalise(dir);
        let normalised: Vec<PathBuf> = entries.iter().map(|(name, _)| base.join(name)).collect();
        let doomed = unreferenced(&normalised, referenced);
        for ((name, is_dir), normalised) in entries.iter().zip(&normalised) {
            if !doomed.contains(normalised) {
                continue;
            }
            let path = dir.join(name);
            let removed = if *is_dir {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match removed {
                Err(e) if e.kind() != io::ErrorKind::NotFound => failures.push(Error::os_at("delete", path, e)),
                _ => {}
            }
        }
        Error::both_all(failures).map_or(Ok(()), Err)
    }
}

#[cfg(test)]
#[path = "sweep_tests.rs"]
mod sweep_tests;
