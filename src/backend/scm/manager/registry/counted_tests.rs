use super::*;

use std::io::ErrorKind;

use windows_sys::Win32::Foundation::ERROR_KEY_DELETED;
use winreg::enums::HKEY_LOCAL_MACHINE;

use crate::testing::groups::{Group, scm_group};
use crate::testing::scratch::ScratchKey;

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32 as i32;

#[skuld::test]
fn a_counted_name_keeps_every_unit_including_a_nul_and_what_follows_it() {
    let name = Name::new("Marker\0x");
    let units: Vec<u16> = "Marker\0x".encode_utf16().collect();

    let counted = counted(&name);

    assert_eq!(usize::from(counted.Length), units.len() * 2);
    assert!(usize::from(counted.MaximumLength) >= usize::from(counted.Length));
    // SAFETY: `counted` points at `name`, which is alive, for `Length` bytes.
    let seen = unsafe { std::slice::from_raw_parts(counted.Buffer, units.len()) };
    assert_eq!(seen, units);
}

/// A terminator would be a unit of the name, and open the wrong key.
#[skuld::test]
fn a_name_made_from_text_has_no_terminator() {
    let name = Name::new("Sub");

    assert_eq!(name.units(), "Sub".encode_utf16().collect::<Vec<u16>>());
    assert_eq!(usize::from(counted(&name).Length), "Sub".len() * 2);
}

#[skuld::test]
fn a_status_reads_as_the_os_error_it_maps_to() {
    assert_eq!(os_error(STATUS_OBJECT_NAME_NOT_FOUND).kind(), ErrorKind::NotFound);
    assert_eq!(os_error(STATUS_ACCESS_DENIED).kind(), ErrorKind::PermissionDenied);
}

#[skuld::test]
fn a_child_is_opened_as_the_key_it_is_even_when_it_is_a_link() {
    let name = counted(&Name::new("Sub"));
    let parent = 0x1234 as HANDLE;

    let attributes = child_attributes(parent, &name, LinkHandling::Itself);

    assert_ne!(
        attributes.Attributes & OBJ_OPENLINK,
        0,
        "a link would be followed to its target"
    );
    assert_ne!(attributes.Attributes & OBJ_CASE_INSENSITIVE, 0);
    assert_eq!(attributes.RootDirectory, parent);
    assert!(std::ptr::eq(attributes.ObjectName, &name));
}

/// `Followed` enters a link.
#[skuld::test]
fn a_child_is_opened_through_a_link_when_asked_to_follow_it() {
    let name = counted(&Name::new("Sub"));

    let attributes = child_attributes(0x1234 as HANDLE, &name, LinkHandling::Followed);

    assert_eq!(attributes.Attributes & OBJ_OPENLINK, 0, "the link would not be entered");
    assert_ne!(attributes.Attributes & OBJ_CASE_INSENSITIVE, 0);
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

const REG_FLAG_VOLATILE: u32 = 0x0001;

/// `KEY_FLAGS_INFORMATION`: the Wow64 flags, the key's flags, the control flags.
#[skuld::test]
fn a_key_is_a_link_when_its_own_flags_say_so_whatever_else_is_set() {
    assert!(link_flag([0, REG_FLAG_LINK, 0], 12).unwrap());
    assert!(link_flag([7, REG_FLAG_LINK | REG_FLAG_VOLATILE, 7], 12).unwrap());
    // The key's flags end at byte 8; the control flags after them are not read.
    assert!(link_flag([0, REG_FLAG_LINK, 0], 8).unwrap());
}

/// A link is told from its flag alone: a key that merely holds a value named `SymbolicLinkValue`
/// has no such flag, and a link that holds no such value still has it.
#[skuld::test]
fn a_key_without_the_link_flag_is_not_a_link() {
    assert!(!link_flag([0, 0, 0], 12).unwrap());
    assert!(!link_flag([REG_FLAG_LINK, REG_FLAG_VOLATILE, REG_FLAG_LINK], 12).unwrap());
}

/// A reply that does not reach the key's flags leaves the buffer as it was, zeroed: that is no
/// answer, and reading it as "not a link" would have `--force` write through a link.
#[skuld::test]
fn a_reply_too_short_for_the_key_flags_is_an_error_not_a_key_that_is_no_link() {
    for written in [0, 4, 7] {
        let e = link_flag([0, 0, 0], written).unwrap_err();
        assert!(e.to_string().contains(&format!("{written} bytes")), "{e}");
    }
}

// Against the real registry ===========================================================================================

/// Writes keys under `HKLM\SOFTWARE`. KEY_DELETED at the open of `name` itself says that the held
/// parent was marked deleted, which loses the delete's target: an error. A child that is not found
/// is done.
#[skuld::test]
fn a_child_of_a_parent_marked_deleted_is_an_error_and_a_child_not_found_is_done(#[fixture(scm_group)] _group: &Group) {
    let scratch = ScratchKey::new(&format!("counted-{}-parent", std::process::id()));
    let parent = scratch.create();

    delete_subkey(&parent, &Name::new("Missing")).expect("a child that is not there is done");

    // Another handle deletes the parent; this one holds it open, so it is marked deleted.
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .delete_subkey(scratch.path())
        .expect("delete the parent through another handle");
    let e = delete_subkey(&parent, &Name::new("Child")).expect_err("the parent is marked deleted");

    assert_eq!(e.raw_os_error(), Some(ERROR_KEY_DELETED as i32), "{e}");
}

/// Writes keys under `HKLM\SOFTWARE`. A key marked deleted while the walk holds it is done: its
/// listing fails with KEY_DELETED, and `gone_is_done` takes that. This covers the listing branch at
/// the top of `delete_tree`; the branch that opens a grandchild of a key deleted meanwhile needs an
/// interleaving between the walk's listing and its open, which the real registry cannot be made to
/// give without a hook in the walk.
#[skuld::test]
fn a_key_marked_deleted_while_the_walk_holds_it_is_done_at_its_listing(#[fixture(scm_group)] _group: &Group) {
    let scratch = ScratchKey::new(&format!("counted-{}-listing", std::process::id()));
    let parent = scratch.create();
    let (top, _) = parent.create_subkey("Top").expect("create Top");
    top.create_subkey("Leaf").expect("create Leaf");
    drop(top);
    let held = open_raw(
        parent.raw_handle(),
        &Name::new("Top"),
        CHILD_ACCESS,
        LinkHandling::Itself,
    )
    .expect("open Top");

    // Another handle deletes the tree under the walk's open handle: `Top` is marked deleted.
    parent
        .delete_subkey_all("Top")
        .expect("delete Top through another handle");

    delete_tree(held).expect("a key deleted under the walk is done");
}
