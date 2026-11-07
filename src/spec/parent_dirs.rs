//! systemd's `..` rule for a working directory and a log path.
//!
//! systemd will not load a path that is not normalized (a `..` in `WorkingDirectory=`; on 255 an
//! `append:` log is dropped). Collapsing a `..` lexically is exact only when the component it
//! consumes is not a symlink: `link/..` is the parent of what `link` leads to, not the directory
//! holding `link`. So [`collapse`] removes a `..` and the component before it when that component is
//! not a symlink, and refuses naming the symlink otherwise. Nothing else in the path is touched: a
//! symlink the `..` does not consume keeps its spelling, so a service follows a repointed one.
//!
//! The consumed component is looked at with `symlink_metadata`, once per distinct path in one call.
//! A component that does not exist is not a symlink. Where a `..` came from decides whose defect a
//! refusal is: the manifest directory's (no field) or the field's own.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::Id;
use super::backend::ShapedSpec;
use super::field::Field;
use super::raw::RawSpec;
use super::resolve::{Refused, invalid, invalid_from, is_under_manifest_dir};
use crate::error::Error;

/// `spec`'s `cwd` and `logs` with every `..` collapsed, or why one cannot be. `authored` is the
/// manifest's text of the fields, which says whether one was joined to `manifest_dir`.
pub(super) fn collapse(spec: &mut ShapedSpec, authored: &RawSpec, manifest_dir: &Path) -> Result<(), Refused> {
    let mut symlinks = Symlinks::default();
    let fields = [
        (Field::Cwd, "cwd", authored.cwd.as_deref(), &mut spec.cwd),
        (Field::Logs, "logs", authored.logs.as_deref(), &mut spec.logs),
    ];
    for (field, name, text, slot) in fields {
        let (Some(text), Some(path)) = (text, slot.as_deref()) else {
            continue;
        };
        if !path.components().any(|c| c == Component::ParentDir) {
            continue;
        }
        debug_assert!(path.is_absolute(), "a resolved `{name}` is absolute");
        // The leading components of `path` that are the manifest directory's.
        let from_dir = if is_under_manifest_dir(Path::new(text)) && path.starts_with(manifest_dir) {
            manifest_dir.components().count()
        } else {
            0
        };
        match collapsed(path, &mut symlinks) {
            Ok(collapsed) => *slot = Some(collapsed),
            Err(stuck) => {
                let in_dir = stuck.at < from_dir;
                let err = refusal(&spec.id, name, text, path, stuck, in_dir);
                return Err(if in_dir {
                    Refused::in_manifest_dir(err)
                } else {
                    Refused::at(field, err)
                });
            }
        }
    }
    Ok(())
}

/// A `..` that could not be collapsed.
struct Stuck {
    /// Its index among the components of the path.
    at: usize,
    /// The path up to the component it consumes.
    consumed: PathBuf,
    /// `None`: the component is a symlink. `Some`: it could not be inspected.
    unreadable: Option<io::Error>,
}

/// `path` with each `..` and the component it consumes removed.
fn collapsed(path: &Path, symlinks: &mut Symlinks) -> Result<PathBuf, Stuck> {
    let mut kept: Vec<Component> = Vec::new();
    for (at, component) in path.components().enumerate() {
        if component != Component::ParentDir {
            kept.push(component);
            continue;
        }
        // A root is its own parent.
        if let Some(Component::Normal(_)) = kept.last() {
            let consumed: PathBuf = kept.iter().collect();
            match symlinks.is_symlink(&consumed) {
                Ok(false) => {
                    kept.pop();
                }
                Ok(true) => {
                    return Err(Stuck {
                        at,
                        consumed,
                        unreadable: None,
                    });
                }
                Err(source) => {
                    return Err(Stuck {
                        at,
                        consumed,
                        unreadable: Some(source),
                    });
                }
            }
        }
    }
    Ok(kept.iter().collect())
}

fn refusal(id: &Id, name: &str, text: &str, path: &Path, stuck: Stuck, from_dir: bool) -> Error {
    let consumed = stuck.consumed.display();
    let head = format!(
        "field `{name}` is `{text}`, which resolves to `{}`, and systemd refuses a `..` in it (it requires a \
         normalized path)",
        path.display()
    );
    match stuck.unreadable {
        None => {
            let fix = if from_dir { "`-f`" } else { "the path" };
            invalid(
                id,
                &format!(
                    "{head}; the `..` follows `{consumed}`, a symlink, so removing it here would name another \
                     directory than the kernel reaches. Write {fix} without `..`"
                ),
            )
        }
        Some(source) => invalid_from(
            id,
            &format!(
                "{head}; whether `{consumed}`, which the `..` follows, is a symlink could not be read, so the `..` \
                 cannot be removed"
            ),
            source,
        ),
    }
}

/// Whether a path is a symlink, read once per path.
#[derive(Default)]
struct Symlinks(BTreeMap<PathBuf, bool>);

impl Symlinks {
    fn is_symlink(&mut self, path: &Path) -> io::Result<bool> {
        if let Some(known) = self.0.get(path) {
            return Ok(*known);
        }
        let is_symlink = match fs::symlink_metadata(path) {
            Ok(meta) => meta.is_symlink(),
            // Nothing there to lead elsewhere.
            Err(source) if source.kind() == io::ErrorKind::NotFound => false,
            Err(source) => return Err(source),
        };
        self.0.insert(path.to_path_buf(), is_symlink);
        Ok(is_symlink)
    }
}
