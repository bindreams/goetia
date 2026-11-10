//! Builders for this crate's unit tests.

use std::path::PathBuf;

use super::BackendKind;

/// `p` made absolute on the host running the test, since `Path::is_absolute` is platform-defined.
pub(crate) fn abs(p: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"C:\{p}"))
    } else {
        PathBuf::from(format!("/{p}"))
    }
}

/// The backends a resolve test may simulate on this host: those whose path style is the host's.
/// `resolve` reads the host's `std::path`, so another backend's rules would run on paths it never meets.
pub(crate) const HOST_BACKENDS: &[BackendKind] = if cfg!(windows) {
    &[BackendKind::Scm]
} else {
    &[BackendKind::Systemd, BackendKind::Launchd]
};

/// A message that lost a `\` line continuation shows as a long run of spaces.
pub(crate) fn assert_no_run_of_spaces(msg: &str) {
    assert!(!msg.contains("  "), "a run of spaces in {msg:?}");
}

/// An absolute program path `bin/<name>` on the host, as a `command[0]`.
pub(crate) fn program(name: &str) -> String {
    abs(&format!("bin/{name}")).to_string_lossy().into_owned()
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
