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
use crate::backend::scm::parameters::RegistryReadOutcome;
use crate::error::{Error, Result};

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
/// since SCM tries each, or run to the end. A path with no extension is also named with `.exe`, as
/// `CreateProcess` appends it. All of them are named, so a service registered either way references
/// the file it starts. More paths than SCM starts only keep more files.
pub(crate) fn host_candidates(image: &str) -> Vec<String> {
    let image = image.trim();
    let mut paths: Vec<String> = Vec::new();
    if let Some(quoted) = image.strip_prefix('"') {
        paths.push(quoted.split('"').next().unwrap_or_default().to_string());
    } else {
        let ends = image
            .char_indices()
            .filter(|&(_, c)| c == ' ' || c == '\t')
            .map(|(at, _)| at)
            .chain(std::iter::once(image.len()));
        paths.extend(ends.map(|end| image[..end].trim_end().to_string()));
    }
    let mut candidates: Vec<String> = Vec::new();
    for path in paths.into_iter().filter(|path| !path.is_empty()) {
        let without_extension = Path::new(&path).extension().is_none();
        for candidate in [Some(path.clone()), without_extension.then(|| format!("{path}.exe"))]
            .into_iter()
            .flatten()
        {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// What a registered `ImagePath` references: the [`host_candidates`] of its `expanded` form and, when
/// it has a `%VAR%` in it, of the text as registered; and, for such a registration, every shim file
/// name among them on its own (a bare file name, which [`unreferenced`] reads as naming every file of
/// that name). This process expands a variable with its own environment, SCM with the system's, and
/// an expansion can fail, so the folder is not trusted to be the one SCM starts from; a shim file's
/// name is a hash of its content, so keeping every file of that name keeps nothing it should not.
pub(crate) fn image_references(raw: &str, expanded: &str) -> Vec<String> {
    let mut references = host_candidates(expanded);
    if raw.contains('%') {
        references.extend(host_candidates(raw));
        let names: Vec<String> = references
            .iter()
            .filter_map(|path| path.rsplit(['\\', '/']).next())
            .filter(|name| shim_file(name).is_some())
            .map(str::to_string)
            .collect();
        references.extend(names);
    }
    let mut unique = Vec::new();
    for reference in references {
        if !unique.contains(&reference) {
            unique.push(reference);
        }
    }
    unique
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

/// The `ImagePath` of the service `name`, from `from_registry` (the read of its key: `Read(None)`
/// for a service that starts nothing, being marked for deletion or having no `ImagePath` that is a
/// string) or from `through_scm`, which is asked when the read failed or found the service gone: a
/// key that denies `KEY_READ` to Administrators still answers SCM, which reads it as the service
/// control manager, and a registry that says gone and a registry link to a missing key (a read
/// failure) both leave SCM's word to settle. Only SCM's answer makes a service gone. An error only
/// when SCM failed too; it names the service, since one unreadable service must not fail every
/// uninstall without saying which.
pub(crate) fn image_of(
    name: &str,
    from_registry: Result<RegistryReadOutcome<Option<String>>>,
    through_scm: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<String>> {
    let registry = match from_registry {
        Ok(RegistryReadOutcome::Read(image)) => return Ok(image),
        Ok(RegistryReadOutcome::ServiceGone) => None,
        Err(registry) => Some(registry),
    };
    through_scm().map_err(|scm| {
        let scm = Error::Step {
            what: "SCM".to_string(),
            source: Box::new(scm),
        };
        Error::ServiceImageUnreadable {
            name: name.to_string(),
            source: Box::new(match registry {
                Some(registry) => Error::Both {
                    first: Box::new(Error::Step {
                        what: "its registry key".to_string(),
                        source: Box::new(registry),
                    }),
                    then: Box::new(scm),
                },
                None => scm,
            }),
        }
    })
}

/// A sweep that went wrong: the failures it met folded into one error, in the order they were met,
/// and the paths it tried to delete and could not, in the same order. A sweep tries every file, so
/// both are complete after the last; the paths are data, not read back out of the error's text.
#[derive(Debug)]
pub struct SweepFailure {
    /// Boxed, as `Error::Both` boxes its parts: with the paths beside it, a bare `Error` would make
    /// every `Result` of a sweep large.
    pub error: Box<Error>,
    pub not_deleted: Vec<PathBuf>,
}

impl SweepFailure {
    /// Every failure of `failures` as one, in the order given; `None` for none.
    pub(crate) fn all(failures: impl IntoIterator<Item = SweepFailure>) -> Option<Self> {
        let mut errors = Vec::new();
        let mut not_deleted = Vec::new();
        for failure in failures {
            errors.push(*failure.error);
            not_deleted.extend(failure.not_deleted);
        }
        Error::both_all(errors).map(|error| Self {
            error: Box::new(error),
            not_deleted,
        })
    }
}

/// A failure that deleted nothing it could name: a read, a lookup.
impl From<Error> for SweepFailure {
    fn from(error: Error) -> Self {
        Self {
            error: Box::new(error),
            not_deleted: Vec::new(),
        }
    }
}

/// The folder, besides the provider's, that an uninstall sweeps after a service whose host was in
/// `host_folder`: that folder when it is the default folder and not the provider's, else none.
/// The provider's own folder is swept by the provider. A folder that is neither is not goetia's
/// to sweep: a consumer's installer may place a file with a shim file's name in its own folder.
///
/// `default_folder` is asked only of a folder that is not the provider's, since where the default
/// folder is matters for nothing else. Paths are expected `normalise`d.
pub(crate) fn second_folder<'a>(
    host_folder: &'a Path,
    provider_folder: Option<&Path>,
    default_folder: impl FnOnce() -> Result<PathBuf>,
) -> Result<Option<&'a Path>> {
    if provider_folder.is_some_and(|provider| same_path(provider, host_folder)) {
        return Ok(None);
    }
    Ok(same_path(&default_folder()?, host_folder).then_some(host_folder))
}

/// What an uninstall knows of the folder its shim provider sweeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderFolder {
    /// The manager has no provider, or its provider sweeps no folder: nothing is retried.
    None,
    /// It has one, and its `folder()` failed, so the folder is not known: nothing is retried. The
    /// stock providers fail their sweep on the same cause.
    Unknown,
    At(PathBuf),
}

impl ProviderFolder {
    /// The folder, when it is known.
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::At(folder) => Some(folder),
            Self::None | Self::Unknown => None,
        }
    }
}

/// What an uninstall says when it deleted the service `id` but could not clean up: which shim files
/// remain, and what removes them. `files` are those the failure names (none when it failed before
/// it tried to delete any, and then any file the service used may remain, in `second_folder`, the
/// folder besides the provider's that the uninstall sweeps: see [`second_folder`]).
///
/// The retry, `uninstall` of an id that is no longer installed, sweeps the provider's folder alone:
/// a file in another folder is removed by the next install or uninstall of a manager whose provider
/// uses that folder, and with no provider, or one that sweeps no folder, nothing is retried at all.
/// For a provider whose folder is [`Unknown`](ProviderFolder::Unknown) nothing is promised either:
/// its sweep fails for the same cause.
pub(crate) fn left_behind_text(
    id: impl std::fmt::Display,
    provider: &ProviderFolder,
    second_folder: Option<&Path>,
    files: &[PathBuf],
) -> String {
    let provider_folder = provider.path();
    let in_providers = |folder: &Path| provider_folder.is_some_and(|provider| same_path(provider, folder));
    let retry = format!("Re-run `goetia daemon uninstall {id}` to retry");
    let elsewhere = "the next install or uninstall of a manager whose provider uses that folder removes";
    let list = |paths: &[&Path]| {
        paths
            .iter()
            .map(|path| format!("`{}`", path.display()))
            .collect::<Vec<_>>()
            .join(", ")
    };

    // Why nothing is retried: the retry runs the provider's sweep, which has no folder to sweep, or
    // whose folder is not known.
    let no_retry = match provider {
        ProviderFolder::None => Some("no shim provider of this program sweeps a folder"),
        ProviderFolder::Unknown => Some("the folder this program's shim provider uses is not known"),
        ProviderFolder::At(_) => None,
    };
    if files.is_empty() {
        let mut said = format!("`{id}` was deleted, but the shim files it used were not cleaned up.");
        if no_retry.is_none() {
            said.push_str(&format!(
                " {retry} what is in the folder this program's shim provider uses."
            ));
        }
        let tail = match (no_retry, second_folder.filter(|folder| !in_providers(folder))) {
            (None, None) => None,
            (None, Some(folder)) => Some(format!(
                " What is in `{}` is not retried by that command: {elsewhere} it.",
                folder.display()
            )),
            (Some(why), None) => Some(format!(" Nothing is retried, since {why}.")),
            (Some(why), Some(folder)) => Some(format!(
                " What is in `{}` is not retried, since {why}: {elsewhere} it.",
                folder.display()
            )),
        };
        said.extend(tail);
        return said;
    }
    let all: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let mut said = format!("`{id}` was deleted, but {} could not be removed.", list(&all));
    let (inside, outside): (Vec<&Path>, Vec<&Path>) = all
        .into_iter()
        .partition(|file| in_providers(file) || file.parent().is_some_and(in_providers));
    if !inside.is_empty() {
        said.push_str(&format!(" {retry} {}.", list(&inside)));
    }
    if !outside.is_empty() {
        let (is, them) = if outside.len() == 1 {
            ("is", "it")
        } else {
            ("are", "them")
        };
        let why = match no_retry {
            Some(why) => format!("{} {is} not retried, since {why}", list(&outside)),
            None => format!(
                "{} {is} not in the folder this program's shim provider uses, so that command does not retry {them}",
                list(&outside)
            ),
        };
        said.push_str(&format!(" {why}: {elsewhere} {them}."));
    }
    said
}

#[cfg(windows)]
pub(crate) use system::{normalise, referenced_hosts, sweep_folder};

#[cfg(windows)]
mod system {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

    use super::super::acl::{FolderCheck, display_path, lexical};
    use super::super::processes::running_images;
    use windows_service::service_manager::ServiceManagerAccess;

    use super::{SweepFailure, image_of, image_references, same_path, unreferenced};
    use crate::backend::scm::manager::{open_scm, read_image_through_scm, registry};
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

    /// Every executable SCM may start for any service's `ImagePath` (see
    /// [`host_candidates`](super::host_candidates)), with its environment variables expanded,
    /// [`normalise`]d. A service marked for deletion references nothing (its record outlives it until
    /// a handle closes), and one with no `ImagePath` has nothing to start. A service whose entry could
    /// not be read through its registry key or through SCM (see [`image_of`]), or an enumeration
    /// that did not finish, is an error: what the unread entries reference is unknown, and it must
    /// not be taken for nothing.
    fn registered_hosts() -> Result<Vec<PathBuf>> {
        let mut hosts = Vec::new();
        let mut failed = Vec::new();
        // Opened by the first service whose key cannot be read, and kept for the rest of the scan:
        // most scans never need it.
        let mut scm = None;
        let unfinished = registry::scan_services(|name, services| {
            let through_scm = || {
                let scm = match &mut scm {
                    Some(scm) => scm,
                    empty => empty.insert(open_scm(ServiceManagerAccess::CONNECT)?),
                };
                read_image_through_scm(scm, &name)
            };
            match image_of(&name, registry::read_registered_image(services, &name), through_scm) {
                Ok(Some(image)) => hosts.extend(image_references(&image, &expand_environment(&image)).iter().map(
                    // A bare file name stands for every file of that name: it is not a path to resolve.
                    |reference| match reference.contains(['\\', '/']) {
                        true => normalise(Path::new(reference)),
                        false => PathBuf::from(reference),
                    },
                )),
                Ok(None) => {}
                Err(e) => failed.push(e),
            }
        });
        scan_outcome(hosts, failed, unfinished, "read which services run which host")
    }

    /// What a pass over the services found, or, when any read failed or the enumeration did not
    /// finish, every such failure under `what`.
    fn scan_outcome<T>(found: T, failed: Vec<Error>, unfinished: Option<Installed>, what: &str) -> Result<T> {
        match Error::both_all(failed.into_iter().chain(unfinished.map(why_unfinished))) {
            None => Ok(found),
            Some(e) => Err(Error::Step {
                what: what.to_string(),
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
    pub(super) fn why_unfinished(entry: Installed) -> Error {
        match entry {
            Installed::Undetermined {
                reason: Reason::Around { error, .. } | Reason::Failed(error),
                ..
            } => Error::Kept(error),
            // The exact text: the report escapes it for a person, and `--json` reports it as it is.
            Installed::Undetermined { reason, .. } => Error::os(
                "enumerate the services",
                io::Error::other(reason.unescaped().to_string()),
            ),
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
    /// folder. Every file is tried: the failures come back together after the last, in the order
    /// the files were tried, each failed delete an `Os { what: "delete", path }` and its path in
    /// [`SweepFailure::not_deleted`].
    ///
    /// Nothing is deleted in a folder `folders` refuses: the refusal is the failure. A `dir` that
    /// does not exist (and only that: any other failure to read its metadata is a failure) has
    /// nothing to sweep, and is not checked, since the check judges the first ancestor that exists,
    /// which a folder made by someone else meanwhile is not.
    ///
    /// The one mechanism behind a provider's `sweep` and the sweep of the default folder an
    /// uninstalled service's host was in (see [`second_folder`](super::second_folder)).
    /// `default_folder` says where the default folder is, and is asked only of a folder left empty;
    /// the second-folder sweep passes the answer it already has.
    pub(crate) fn sweep_folder(
        dir: &Path,
        referenced: &[PathBuf],
        folders: &FolderCheck,
        default_folder: impl FnOnce() -> Result<PathBuf>,
    ) -> std::result::Result<(), SweepFailure> {
        match fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::os_at("read", dir, e).into()),
        }
        folders.check(dir)?;
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::os_at("read", dir, e).into()),
        };
        let (swept, listed) = sweep_files(dir, listing, referenced);
        let removed = if listed {
            remove_if_empty_default(dir, || Ok(same_path(&normalise(&default_folder()?), &normalise(dir))))
        } else {
            Ok(())
        };
        SweepFailure::all(swept.err().into_iter().chain(removed.err())).map_or(Ok(()), Err)
    }

    /// Removes `dir` if it is empty and `is_default` says it is the default folder, which is asked
    /// only of a folder that is empty: where the default folder is matters for nothing else.
    ///
    /// A folder that cannot be listed, or whose first entry cannot be read, is not known to be empty:
    /// it stays, and the failure is the removal's. A folder whose listing already failed on an entry is
    /// not passed here: `sweep_folder` has reported it. A folder that is not there has nothing to
    /// remove.
    pub(super) fn remove_if_empty_default(
        dir: &Path,
        is_default: impl FnOnce() -> Result<bool>,
    ) -> std::result::Result<(), SweepFailure> {
        match fs::read_dir(dir) {
            Ok(mut listing) => match listing.next() {
                Some(Ok(_)) => return Ok(()),
                Some(Err(e)) => return Err(Error::os_at("read an entry of", dir, e).into()),
                None => {}
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::os_at("read", dir, e).into()),
        }
        if !is_default()? {
            return Ok(());
        }
        match fs::remove_dir(dir) {
            Err(e) if !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => {
                Err(delete_failed(dir, e))
            }
            _ => Ok(()),
        }
    }

    /// The deletion half of [`sweep_folder`], and whether every entry of the listing was read.
    fn sweep_files(
        dir: &Path,
        listing: fs::ReadDir,
        referenced: &[PathBuf],
    ) -> (std::result::Result<(), SweepFailure>, bool) {
        let mut failures = Vec::new();
        let mut entries = Vec::new();
        let mut listed = true;
        for entry in listing {
            match entry {
                // A name that is no shim file's is dropped by `unreferenced`; a folder that a
                // repair set aside whole is a shim file's name too.
                Ok(entry) => entries.push((entry.file_name(), entry.file_type().is_ok_and(|t| t.is_dir()))),
                Err(e) => {
                    listed = false;
                    failures.push(Error::os_at("read an entry of", dir, e).into());
                }
            }
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
                Err(e) if e.kind() != io::ErrorKind::NotFound => failures.push(delete_failed(path, e)),
                _ => {}
            }
        }
        (SweepFailure::all(failures).map_or(Ok(()), Err), listed)
    }

    fn delete_failed(path: impl Into<PathBuf>, source: io::Error) -> SweepFailure {
        let path = path.into();
        SweepFailure {
            error: Box::new(Error::os_at("delete", &path, source)),
            not_deleted: vec![path],
        }
    }
}

#[cfg(test)]
#[path = "sweep_tests.rs"]
mod sweep_tests;
