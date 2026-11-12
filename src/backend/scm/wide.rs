//! Passing text to the wide Win32 calls, and reading what they hand back.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;

/// A NUL that a wide Win32 call would take for the end of the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the text holds a NUL at position {index}")]
pub struct InteriorNul {
    /// The UTF-16 position of the first NUL.
    pub index: usize,
}

impl From<InteriorNul> for std::io::Error {
    fn from(e: InteriorNul) -> Self {
        Self::new(std::io::ErrorKind::InvalidInput, e)
    }
}

/// `s` as the NUL-terminated UTF-16 string the wide Win32 calls take. For text goetia sends: an
/// interior NUL is refused, never cut. The registry layer holds the same rule for what it hands
/// `winreg` (`refuse_nul`). See `wide_null_cut` for text read from Windows.
pub fn wide_null(s: impl AsRef<OsStr>) -> Result<Vec<u16>, InteriorNul> {
    let s = s.as_ref();
    let mut units = Vec::with_capacity(s.len() + 1);
    for unit in s.encode_wide() {
        if unit == 0 {
            return Err(InteriorNul { index: units.len() });
        }
        units.push(unit);
    }
    units.push(0);
    Ok(units)
}

/// The text of `s` before its first NUL, NUL-terminated. For text read from Windows, which treats it
/// as a C string.
pub fn wide_null_cut(s: impl AsRef<OsStr>) -> Vec<u16> {
    let mut units: Vec<u16> = s.as_ref().encode_wide().take_while(|&unit| unit != 0).collect();
    units.push(0);
    units
}

/// `message` as NUL-terminated UTF-16 for the Event Log. A NUL would end the message early there, so
/// it is escaped as `\0`. Not `wide_null`: it refuses a NUL, and this path reports failures, so it
/// cannot fail itself.
pub fn wide_null_escaped(message: &str) -> Vec<u16> {
    message.replace('\0', "\\0").encode_utf16().chain([0]).collect()
}

/// The UTF-16 units of the NUL-terminated string at `ptr`, without the NUL.
///
/// # Safety
/// `ptr` points at a NUL-terminated string of `u16` that stays live and unchanged for `'a`.
pub unsafe fn units_until_nul<'a>(ptr: *const u16) -> &'a [u16] {
    let mut len = 0;
    // SAFETY: the caller guarantees a NUL ends the string, so every unit read is at or before it.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the NUL were just read.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

#[cfg(test)]
#[path = "wide_tests.rs"]
mod wide_tests;
