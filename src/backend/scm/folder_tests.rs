use super::*;

#[skuld::test]
fn locate_error_names_the_folder() {
    // The facility of a Win32 error, and `E_FAIL`.
    for hresult in [0x8007_0057_u32 as i32, 0x8000_4005_u32 as i32] {
        match locate_error("%ProgramFiles%", hresult) {
            Error::Os { what, path, source } => {
                assert_eq!(what, "locate %ProgramFiles%");
                assert_eq!(path, None);
                assert!(!source.to_string().is_empty());
            }
            other => panic!("expected an OS error, got {other:?}"),
        }
    }
}
