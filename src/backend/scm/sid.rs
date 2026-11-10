//! A security identifier as bytes: how one is built, checked and shown. Plain data, so it works on
//! every host except [`sid_display`]; looking an account up by it is Windows-only, in
//! `manager::identity`.

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

/// `S-1-5-32-544`, or `None` when the bytes are not a whole SID: the check that a Windows call may
/// read the sub-authorities the bytes declare. For validation, and for the unit tests of the
/// well-known SIDs on every host; a Windows build shows a SID with [`sid_display`]. An identifier
/// authority of 2^32 or more is written here in decimal, which is not Windows' form.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn sid_text(sid: &[u8]) -> Option<String> {
    let (&1, &count) = (sid.first()?, sid.get(1)?) else {
        return None;
    };
    let count = usize::from(count);
    if count > 15 || sid.len() != 8 + 4 * count {
        return None;
    }
    let authority = sid[2..8].iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    let mut text = format!("S-1-{authority}");
    for &sub in sid[8..].as_chunks::<4>().0 {
        text.push('-');
        text.push_str(&u32::from_le_bytes(sub).to_string());
    }
    Some(text)
}

/// `sid` as `ConvertSidToStringSidW` writes it, or a description of bytes that are not a SID. What
/// an error names an account by when `LookupAccountSidW` has no name for it.
#[cfg(windows)]
pub(crate) fn sid_display(sid: &[u8]) -> String {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::PSID;

    const INVALID: &str = "an invalid SID";
    // `sid_text` checks that the bytes hold the sub-authorities they declare, which the conversion
    // would otherwise read past a short slice to find out.
    if sid_text(sid).is_none() {
        return INVALID.to_string();
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid_text` has checked that `sid` holds a whole SID's bytes; `text` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid.as_ptr() as PSID, &mut text) } == 0 {
        return INVALID.to_string();
    }
    let mut len = 0;
    // SAFETY: `text` is a NUL-terminated string, live until it is freed below.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the NUL were just read; `text` was allocated by the conversion
    // and is freed once.
    unsafe {
        let shown = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
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
