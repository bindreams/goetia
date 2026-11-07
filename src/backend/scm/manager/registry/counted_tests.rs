use super::*;

use std::io::ErrorKind;

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32 as i32;

#[skuld::test]
fn a_counted_name_keeps_every_unit_including_a_nul_and_what_follows_it() {
    let name: Vec<u16> = "Marker\0x".encode_utf16().collect();

    let counted = counted(&name);

    assert_eq!(usize::from(counted.Length), name.len() * 2);
    assert!(usize::from(counted.MaximumLength) >= usize::from(counted.Length));
    // SAFETY: `counted` points at `name`, which is alive, for `Length` bytes.
    let seen = unsafe { std::slice::from_raw_parts(counted.Buffer, name.len()) };
    assert_eq!(seen, name);
}

#[skuld::test]
fn a_status_reads_as_the_os_error_it_maps_to() {
    assert_eq!(os_error(STATUS_OBJECT_NAME_NOT_FOUND).kind(), ErrorKind::NotFound);
    assert_eq!(os_error(STATUS_ACCESS_DENIED).kind(), ErrorKind::PermissionDenied);
}
