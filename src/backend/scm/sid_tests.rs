//! `sid_text` over plain bytes, so these run on every host; `sid_display` calls Windows.

use super::*;

#[skuld::test]
fn a_sid_is_written_in_the_standard_string_form() {
    assert_eq!(sid_text(&system()).unwrap(), "S-1-5-18");
    assert_eq!(sid_text(&administrators()).unwrap(), "S-1-5-32-544");
    assert_eq!(sid_text(&everyone()).unwrap(), "S-1-1-0");
    assert_eq!(
        sid_text(&trusted_installer()).unwrap(),
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    );
}

#[skuld::test]
fn bytes_that_are_not_a_sid_are_not_text_and_not_panicked_on() {
    assert_eq!(sid_text(&[]), None);
    assert_eq!(sid_text(&[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]), None);
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::PSID;

    use super::*;

    /// What `ConvertSidToStringSidW` writes for `sid`; a pure conversion, which reads no OS state.
    fn converted(sid: &[u8]) -> String {
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is a whole SID (the callers pass `sid_from_parts`); `text` is a valid out-pointer.
        assert_ne!(unsafe { ConvertSidToStringSidW(sid.as_ptr() as PSID, &mut text) }, 0);
        let mut len = 0;
        // SAFETY: `text` is NUL-terminated and live until freed below.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: the `len` units before the NUL were just read; `text` is freed once.
        unsafe {
            let shown = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            shown
        }
    }

    #[skuld::test]
    fn a_large_identifier_authority_is_shown_as_windows_writes_it() {
        // 2^32: the first authority that is not written in decimal.
        let large = [1, 1, 0, 1, 0, 0, 0, 0, 7, 0, 0, 0];
        assert_eq!(sid_display(&large), converted(&large));
        assert_eq!(sid_display(&administrators()), converted(&administrators()));
    }

    #[skuld::test]
    fn bytes_that_are_not_a_sid_are_shown_as_such() {
        assert_eq!(sid_display(&[]), "an invalid SID");
        assert_eq!(sid_display(&[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]), "an invalid SID");
    }
}
