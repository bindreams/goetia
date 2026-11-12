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
/// that would have ended it early.
pub fn wide_null(s: impl AsRef<OsStr>) -> Result<Vec<u16>, InteriorNul> {
    let mut units: Vec<u16> = s.as_ref().encode_wide().collect();
    if let Some(index) = units.iter().position(|&unit| unit == 0) {
        return Err(InteriorNul { index });
    }
    units.push(0);
    Ok(units)
}

/// [`wide_null`] for a literal or constant, which the code that wrote it can see holds no NUL.
///
/// # Panics
/// When `s` holds a NUL: a literal that does is a defect that the first use shows.
pub fn wide_literal(s: &'static str) -> Vec<u16> {
    wide_null(s).unwrap_or_else(|e| panic!("the literal {s:?} holds a NUL: {e}"))
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
