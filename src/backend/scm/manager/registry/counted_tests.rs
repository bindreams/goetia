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

#[skuld::test]
fn a_child_is_opened_as_the_key_it_is_even_when_it_is_a_link() {
    let name = counted(&"Sub".encode_utf16().collect::<Vec<u16>>());
    let parent = 0x1234 as HANDLE;

    let attributes = child_attributes(parent, &name);

    assert_ne!(
        attributes.Attributes & OBJ_OPENLINK,
        0,
        "a link would be followed to its target"
    );
    assert_ne!(attributes.Attributes & OBJ_CASE_INSENSITIVE, 0);
    assert_eq!(attributes.RootDirectory, parent);
    assert!(std::ptr::eq(attributes.ObjectName, &name));
}

/// `KEY_BASIC_INFORMATION`: an 8-byte time, a 4-byte title index, a 4-byte name length in bytes,
/// then the name.
fn basic_information(name: &[u16]) -> Vec<u8> {
    let mut bytes = vec![0u8; 8];
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(((name.len() * 2) as u32).to_le_bytes());
    bytes.extend(name.iter().flat_map(|unit| unit.to_le_bytes()));
    // The buffer is larger than what the call wrote.
    bytes.extend([0xFF; 8]);
    bytes
}

#[skuld::test]
fn a_key_name_read_back_keeps_a_nul_and_what_follows_it() {
    let name: Vec<u16> = "a\0b".encode_utf16().collect();

    assert_eq!(basic_name(&basic_information(&name)), name);
}
