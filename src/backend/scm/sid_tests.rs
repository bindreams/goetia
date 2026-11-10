//! `sid_text` and `sid_string` over plain bytes, so these run on every host.

use super::*;

#[skuld::test]
fn a_sid_is_written_in_the_standard_string_form() {
    assert_eq!(sid_string(&system()), "S-1-5-18");
    assert_eq!(sid_string(&administrators()), "S-1-5-32-544");
    assert_eq!(sid_string(&everyone()), "S-1-1-0");
    assert_eq!(
        sid_string(&trusted_installer()),
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    );
}

#[skuld::test]
fn an_identifier_authority_of_2_to_the_32_or_more_is_written_as_unpadded_uppercase_hex() {
    let first_hex = [1, 1, 0, 1, 0, 0, 0, 0, 7, 0, 0, 0];
    assert_eq!(sid_string(&first_hex), "S-1-0x100000000-7");
    let all_six_bytes = [1, 1, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 7, 0, 0, 0];
    assert_eq!(sid_string(&all_six_bytes), "S-1-0xABCDEF123456-7");
}

#[skuld::test]
fn bytes_that_are_not_a_sid_are_written_as_such_not_panicked_on() {
    assert_eq!(sid_string(&[]), "an invalid SID");
    assert_eq!(sid_string(&[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]), "an invalid SID");
}
