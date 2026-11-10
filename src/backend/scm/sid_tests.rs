//! `sid_text` and `sid_string` over plain bytes, so these run on every host.

use super::*;

#[skuld::test]
fn a_sid_is_written_in_the_standard_string_form() {
    assert_eq!(sid_string(&sid_from_parts(5, &[32, 544])), "S-1-5-32-544");
    assert_eq!(sid_string(&sid_from_parts(1, &[0])), "S-1-1-0");
    assert_eq!(
        sid_string(&sid_from_parts(
            5,
            &[80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464]
        )),
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    );
}

#[skuld::test]
fn bytes_that_are_not_a_sid_are_written_as_such_not_panicked_on() {
    assert_eq!(sid_string(&[]), "an invalid SID");
    assert_eq!(sid_string(&[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]), "an invalid SID");
}
