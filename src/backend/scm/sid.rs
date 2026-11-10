//! A security identifier as bytes: how one is built, checked, copied and shown. Plain data, so it
//! works on every host except [`try_sid_display`] and [`copy_sid`], which call Windows; looking an
//! account up by it is Windows-only, in `manager::identity`.

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
const INVALID: &str = "an invalid SID";

/// What [`sid_display`] says of a whole SID that `ConvertSidToStringSidW` refused.
#[cfg(windows)]
const UNSHOWABLE: &str = "a SID that could not be shown";

/// The bytes of the SID at `psid`, or `None` when it is null or not a valid SID.
///
/// # Safety
/// `psid` is null or points at a SID structure whole enough to read: its header and the
/// sub-authorities the header counts.
#[cfg(windows)]
pub unsafe fn copy_sid(psid: windows_sys::Win32::Security::PSID) -> Option<Sid> {
    use windows_sys::Win32::Security::{GetLengthSid, IsValidSid};

    // SAFETY: `IsValidSid` reads the structure the caller vouches for; a null pointer is rejected first.
    if psid.is_null() || unsafe { IsValidSid(psid) } == 0 {
        return None;
    }
    // SAFETY: valid per the check above, so `GetLengthSid` bytes are readable from `psid`.
    let bytes = unsafe { std::slice::from_raw_parts(psid as *const u8, GetLengthSid(psid) as usize) };
    Some(bytes.to_vec())
}

/// `sid` as `ConvertSidToStringSidW` writes it, `S-1-5-32-544` for Administrators; `None` for bytes
/// that are not a whole SID or that Windows refuses to show.
#[cfg(windows)]
pub(crate) fn try_sid_display(sid: &[u8]) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::PSID;

    use super::wide::units_until_nul;

    if !is_whole_sid(sid) {
        return None;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `is_whole_sid` has checked that `sid` holds a whole SID's bytes; `text` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid.as_ptr() as PSID, &mut text) } == 0 {
        return None;
    }
    // SAFETY: on success `text` is a NUL-terminated string that is live until freed below, which
    // is once, after the last read.
    Some(unsafe {
        let shown = String::from_utf16_lossy(units_until_nul(text));
        LocalFree(text.cast());
        shown
    })
}

/// [`try_sid_display`], or a description of why there is none. What an error names an account by
/// when `LookupAccountSidW` has no name for it. A test that needs the SID goes through
/// `testing::shown_sid`, which never takes the description for one.
#[cfg(windows)]
pub(crate) fn sid_display(sid: &[u8]) -> String {
    try_sid_display(sid).unwrap_or_else(|| if is_whole_sid(sid) { UNSHOWABLE } else { INVALID }.to_string())
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
