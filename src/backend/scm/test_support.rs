//! Byte layouts of registry string values, shared by the SCM unit tests.

/// The bytes goetia stores for a string value: UTF-16LE and one closing NUL.
pub(crate) fn sz(text: &str) -> Vec<u8> {
    units_bytes(&text.encode_utf16().chain([0]).collect::<Vec<_>>())
}

/// The bytes of a `REG_MULTI_SZ` holding one string: UTF-16LE, its NUL, and the closing NUL.
pub(crate) fn multi_sz(text: &str) -> Vec<u8> {
    units_bytes(&text.encode_utf16().chain([0, 0]).collect::<Vec<_>>())
}

/// `units` as the UTF-16LE bytes a registry value holds.
pub(crate) fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}
