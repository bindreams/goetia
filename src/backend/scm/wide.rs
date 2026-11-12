//! Passing text to the wide Win32 calls, and reading what they hand back.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;

/// Text that holds a NUL, which a wide Win32 call would take for the end of the string and read
/// only up to.
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

/// `s` as the NUL-terminated UTF-16 string the wide Win32 calls take, or the position of the NUL
/// that would have ended it early. For text goetia sends: it is refused, never cut. Text that
/// Windows hands back is read as Windows reads it, with [`wide_null_cut`].
pub fn wide_null(s: impl AsRef<OsStr>) -> Result<Vec<u16>, InteriorNul> {
    let mut units = Vec::new();
    for unit in s.as_ref().encode_wide() {
        if unit == 0 {
            return Err(InteriorNul { index: units.len() });
        }
        units.push(unit);
    }
    units.push(0);
    Ok(units)
}

/// The text of `s` before its first NUL, as the NUL-terminated UTF-16 string a wide Win32 call
/// reads it as: for text that came from Windows, which treats it as a C string and so ends it at
/// the first NUL.
pub fn wide_null_cut(s: impl AsRef<OsStr>) -> Vec<u16> {
    let mut units: Vec<u16> = s.as_ref().encode_wide().take_while(|&unit| unit != 0).collect();
    units.push(0);
    units
}

/// `s` as a NUL-terminated UTF-16 string, evaluated at compile time. `N` is `s.len() + 1`, so a
/// constant spells it `const W: [u16; S.len() + 1] = wide_literal(S);`.
///
/// `s` must be ASCII and hold no NUL: used in a constant, a literal that does not fails the build,
/// so no run can meet it.
///
/// # Panics
/// When `s` is not ASCII, holds a NUL, or `N` is not `s.len() + 1`.
pub const fn wide_literal<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    assert!(bytes.len() + 1 == N, "N is not the length of the literal plus one");
    let mut units = [0u16; N];
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii(), "the literal is not ASCII");
        assert!(bytes[i] != 0, "the literal holds a NUL");
        units[i] = bytes[i] as u16;
        i += 1;
    }
    units
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
