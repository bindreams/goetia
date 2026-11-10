//! A security identifier as bytes: how one is built, checked and shown. Plain data, so it works on
//! every host except [`sid_display`], which asks Windows; looking an account up by it is
//! Windows-only, in `manager::identity`.

/// A security identifier in its binary form, as `LookupAccountNameW` and `GetAce` return it.
pub(crate) type Sid = Vec<u8>;

/// The binary SID with `authority` as its identifier authority and `subauthorities` after it.
pub(crate) fn sid_from_parts(authority: u8, subauthorities: &[u32]) -> Sid {
    let mut sid = vec![1, subauthorities.len() as u8, 0, 0, 0, 0, 0, authority];
    for sub in subauthorities {
        sid.extend_from_slice(&sub.to_le_bytes());
    }
    sid
}

/// Whether `sid` holds a whole SID: revision 1, at most 15 sub-authorities, and as many bytes as
/// the sub-authority count says. What a Windows call needs to hold before it reads the bytes as a
/// `PSID`, since it would read past a short slice to find out.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_whole_sid(sid: &[u8]) -> bool {
    let (Some(&1), Some(&count)) = (sid.first(), sid.get(1)) else {
        return false;
    };
    let count = usize::from(count);
    count <= 15 && sid.len() == 8 + 4 * count
}

/// What [`sid_display`] says of bytes that are not a whole SID.
#[cfg(windows)]
pub(crate) const INVALID: &str = "an invalid SID";

/// What [`sid_display`] says of a whole SID that `ConvertSidToStringSidW` refused.
#[cfg(windows)]
pub(crate) const UNSHOWABLE: &str = "a SID that could not be shown";

/// `sid` as `ConvertSidToStringSidW` writes it, `S-1-5-32-544` for Administrators; [`INVALID`] for
/// bytes that are not a whole SID. What an error names an account by when `LookupAccountSidW` has
/// no name for it, and what the integration tests compare against.
#[cfg(windows)]
pub(crate) fn sid_display(sid: &[u8]) -> String {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::PSID;

    use super::wide::units_until_nul;

    if !is_whole_sid(sid) {
        return INVALID.to_string();
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `is_whole_sid` has checked that `sid` holds a whole SID's bytes; `text` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid.as_ptr() as PSID, &mut text) } == 0 {
        return UNSHOWABLE.to_string();
    }
    // SAFETY: on success `text` is a NUL-terminated string that is live until freed below, which
    // is once, after the last read.
    unsafe {
        let shown = String::from_utf16_lossy(units_until_nul(text));
        LocalFree(text.cast());
        shown
    }
}

// Well-known SIDs =====================================================================================================

pub(super) fn system() -> Sid {
    sid_from_parts(5, &[18])
}

pub(crate) fn administrators() -> Sid {
    sid_from_parts(5, &[32, 544])
}

pub(super) fn trusted_installer() -> Sid {
    sid_from_parts(5, &[80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464])
}

pub(super) fn everyone() -> Sid {
    sid_from_parts(1, &[0])
}

#[cfg(test)]
#[path = "sid_tests.rs"]
mod sid_tests;
