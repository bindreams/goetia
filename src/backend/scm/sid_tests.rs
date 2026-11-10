//! `is_whole_sid` and the well-known SIDs' bytes, so these run on every host; `sid_display` asks
//! Windows.

use super::*;

#[skuld::test]
fn the_well_known_sids_are_these_bytes() {
    assert_eq!(system(), [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]);
    assert_eq!(administrators(), [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0]);
    assert_eq!(everyone(), [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    assert_eq!(
        trusted_installer(),
        [
            1, 6, 0, 0, 0, 0, 0, 5, // revision, six sub-authorities, authority 5
            80, 0, 0, 0, //
            181, 137, 251, 56, //
            25, 132, 194, 203, //
            92, 108, 35, 109, //
            87, 0, 119, 110, //
            192, 2, 100, 135,
        ]
    );
}

#[skuld::test]
fn a_whole_sid_is_told_from_bytes_that_are_not_one() {
    for sid in [system(), administrators(), everyone(), trusted_installer()] {
        assert!(is_whole_sid(&sid), "{sid:?}");
    }
    let mut sixteen = sid_from_parts(5, &[0; 15]);
    assert!(is_whole_sid(&sixteen));
    sixteen[1] = 16;
    sixteen.extend_from_slice(&[0; 4]);
    let mut other_revision = system();
    other_revision[0] = 2;
    let mut one_byte_short = administrators();
    one_byte_short.pop();
    let mut one_byte_long = administrators();
    one_byte_long.push(0);
    for bytes in [
        &[][..],
        &[1],
        &sixteen,
        &other_revision,
        &one_byte_short,
        &one_byte_long,
    ] {
        assert!(!is_whole_sid(bytes), "{bytes:?}");
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Security::PSID;

    use super::*;

    #[skuld::test]
    fn the_well_known_sids_are_shown_as_windows_writes_them() {
        assert_eq!(try_sid_display(&system()).unwrap(), "S-1-5-18");
        assert_eq!(try_sid_display(&administrators()).unwrap(), "S-1-5-32-544");
        assert_eq!(try_sid_display(&everyone()).unwrap(), "S-1-1-0");
        assert_eq!(
            try_sid_display(&trusted_installer()).unwrap(),
            "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        );
    }

    /// 2^32 is the first authority that Windows writes in hex, measured with `ConvertSidToStringSidW`.
    #[skuld::test]
    fn a_large_identifier_authority_is_shown_in_hex() {
        assert_eq!(
            try_sid_display(&[1, 1, 0, 1, 0, 0, 0, 0, 7, 0, 0, 0]).unwrap(),
            "S-1-0x100000000-7"
        );
    }

    /// 2^32 - 1 is the last authority that Windows writes in decimal.
    #[skuld::test]
    fn the_largest_decimal_identifier_authority_is_shown_in_decimal() {
        assert_eq!(
            try_sid_display(&[1, 1, 0, 0, 255, 255, 255, 255, 7, 0, 0, 0]).unwrap(),
            "S-1-4294967295-7"
        );
    }

    /// `None` for a whole SID would mean `ConvertSidToStringSidW` failing for want of memory, which
    /// a test cannot make happen; the bytes that are not a SID are the contract's other side.
    #[skuld::test]
    fn bytes_that_are_not_a_sid_have_no_display_and_a_description() {
        for bytes in [&[][..], &[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]] {
            assert_eq!(try_sid_display(bytes), None);
            assert_eq!(sid_display(bytes), INVALID);
        }
    }

    #[skuld::test]
    fn a_sid_is_copied_from_where_it_stands_and_a_null_or_invalid_one_is_not() {
        let mut sid = administrators();
        // SAFETY: `sid` is a whole SID, live for the call.
        assert_eq!(unsafe { copy_sid(sid.as_mut_ptr() as PSID) }, Some(administrators()));
        // SAFETY: a null pointer is allowed.
        assert_eq!(unsafe { copy_sid(std::ptr::null_mut()) }, None);
        sid[0] = 2;
        // SAFETY: `sid` is as long as its header says, live for the call.
        assert_eq!(unsafe { copy_sid(sid.as_mut_ptr() as PSID) }, None);
    }
}
