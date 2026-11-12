//! `wide_null` is a pure function over UTF-16; these run on the host.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt as _;

use super::*;

#[skuld::test]
fn text_without_a_nul_is_its_units_and_one_terminator() {
    assert_eq!(wide_null("ab").unwrap(), [u16::from(b'a'), u16::from(b'b'), 0]);
    assert_eq!(wide_null("").unwrap(), [0]);
}

#[skuld::test]
fn text_that_is_not_unicode_keeps_its_units() {
    let lone_surrogate = OsString::from_wide(&[u16::from(b'a'), 0xD800]);

    assert_eq!(wide_null(lone_surrogate).unwrap(), [u16::from(b'a'), 0xD800, 0]);
}

#[skuld::test]
fn a_nul_inside_the_text_is_refused_with_where_it_is() {
    assert_eq!(wide_null("ab\0cd").unwrap_err(), InteriorNul { index: 2 });
    assert_eq!(wide_null("\0").unwrap_err(), InteriorNul { index: 0 });
    assert_eq!(wide_null("ab\0").unwrap_err(), InteriorNul { index: 2 });
}

#[skuld::test]
fn the_refusal_reads_as_invalid_input_and_says_where() {
    let e = std::io::Error::from(InteriorNul { index: 2 });

    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    assert!(e.to_string().contains("position 2"), "{e}");
}

#[skuld::test]
fn a_literal_is_its_units_and_one_terminator() {
    assert_eq!(wide_literal("ab"), [u16::from(b'a'), u16::from(b'b'), 0]);
}

#[skuld::test]
#[should_panic(expected = "NUL")]
fn a_literal_with_a_nul_panics_rather_than_truncating() {
    let _ = wide_literal("ab\0cd");
}
