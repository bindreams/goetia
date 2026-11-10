//! A security identifier as bytes: how one is built, and how it is written. Plain data, so it
//! works on every host; looking an account up by it is Windows-only, in `manager::identity`.

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

/// `S-1-5-32-544`, or `None` when the bytes are not a SID.
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
    let mut text = if authority < 1 << 32 {
        format!("S-1-{authority}")
    } else {
        format!("S-1-{authority:#x}")
    };
    for &sub in sid[8..].as_chunks::<4>().0 {
        text.push('-');
        text.push_str(&u32::from_le_bytes(sub).to_string());
    }
    Some(text)
}

/// [`sid_text`], or a description of bytes that are not a SID. What an error names an account by
/// when `LookupAccountSidW` has no name for it.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn sid_string(sid: &[u8]) -> String {
    sid_text(sid).unwrap_or_else(|| "an invalid SID".to_string())
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
