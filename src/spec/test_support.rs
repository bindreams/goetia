//! Builders for this crate's unit tests.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{DaemonFields, Id, Kind, Restart, User};

/// A plain root, `restart: never`, `type: managed` daemon named `id`; panics on an invalid `Id`.
/// Mirrors `tests/support/daemon_spec.rs` (integration tests cannot reach `#[cfg(test)]`).
pub(crate) fn plain_fields(id: &str, command: Vec<String>) -> DaemonFields {
    DaemonFields {
        id: Id::try_from(id).unwrap_or_else(|e| panic!("`{id}` is not a valid Id: {e}")),
        name: id.to_string(),
        command,
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Managed,
    }
}

/// An absolute path that is not valid UTF-8, for the rule that refuses one.
#[cfg(windows)]
pub(crate) fn non_utf8_absolute_path() -> PathBuf {
    // A lone UTF-16 surrogate, which `OsString` holds on Windows.
    use std::os::windows::ffi::OsStringExt as _;
    PathBuf::from(std::ffi::OsString::from_wide(&[
        u16::from(b'C'),
        u16::from(b':'),
        u16::from(b'\\'),
        0xD800,
    ]))
}

/// An absolute path that is not valid UTF-8, for the rule that refuses one.
#[cfg(unix)]
pub(crate) fn non_utf8_absolute_path() -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xFF, 0xFE]))
}
