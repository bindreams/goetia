//! Reading what the wide Win32 calls hand back.

/// The UTF-16 units of the NUL-terminated string at `ptr`, without the NUL.
///
/// # Safety
/// `ptr` points at a NUL-terminated string of `u16` that stays live and unchanged for `'a`.
pub(crate) unsafe fn units_until_nul<'a>(ptr: *const u16) -> &'a [u16] {
    let mut len = 0;
    // SAFETY: the caller guarantees a NUL ends the string, so every unit read is at or before it.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the NUL were just read.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}
