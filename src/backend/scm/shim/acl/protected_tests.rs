//! `is_as_wanted` over stated tables, so these run on every host. The Windows-only tests build
//! descriptors from the SDDL strings and read them back, touching no file.

use std::path::PathBuf;

use super::*;
use crate::backend::scm::shim::acl::acl_tests::{allow, users};
use crate::backend::scm::shim::acl::{ObjectSecurity, administrators, everyone, system};

const FULL: u32 = 0x1F_01FF;
const READ_EXECUTE: u32 = 0x12_00A9;
const OBJECT_INHERIT: u8 = 0x01;
const CONTAINER_INHERIT: u8 = 0x02;

/// What the stock shim file has, with `path` as the label an existing file is read under.
fn stock_file(path: &str) -> ObjectSecurity {
    ObjectSecurity {
        path: PathBuf::from(path),
        owner: administrators(),
        protected: true,
        directory: false,
        dacl: Some(vec![
            allow(system(), FULL, 0),
            allow(administrators(), FULL, 0),
            allow(everyone(), READ_EXECUTE, 0),
        ]),
    }
}

#[skuld::test]
fn security_equal_but_for_the_path_and_the_order_of_entries_is_as_wanted() {
    let wanted = stock_file("");
    let mut actual = stock_file(r"C:\Program Files\Goetia\goetia-shim-00.exe");
    actual.dacl.as_mut().unwrap().reverse();

    assert!(is_as_wanted(&actual, &wanted));
}

#[skuld::test]
fn another_owner_is_not_as_wanted() {
    let mut actual = stock_file("");
    actual.owner = users();

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[skuld::test]
fn a_dacl_that_inherits_is_not_as_wanted() {
    let mut actual = stock_file("");
    actual.protected = false;

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[skuld::test]
fn an_extra_entry_is_not_as_wanted() {
    let mut actual = stock_file("");
    actual.dacl.as_mut().unwrap().push(allow(users(), 0x1, 0));

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[skuld::test]
fn a_missing_entry_is_not_as_wanted() {
    let mut actual = stock_file("");
    actual.dacl.as_mut().unwrap().pop();

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[skuld::test]
fn a_wider_mask_or_other_flags_are_not_as_wanted() {
    let mut wider = stock_file("");
    wider.dacl.as_mut().unwrap()[2].mask = FULL;
    let mut flagged = stock_file("");
    flagged.dacl.as_mut().unwrap()[0].flags = OBJECT_INHERIT | CONTAINER_INHERIT;

    assert!(!is_as_wanted(&wider, &stock_file("")));
    assert!(!is_as_wanted(&flagged, &stock_file("")));
}

#[skuld::test]
fn a_deny_entry_is_not_an_allow_entry() {
    let mut actual = stock_file("");
    actual.dacl.as_mut().unwrap()[2].allow = false;

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[skuld::test]
fn a_null_dacl_is_not_as_wanted() {
    let mut actual = stock_file("");
    actual.dacl = None;

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[cfg(windows)]
mod windows {
    use super::*;

    #[skuld::test]
    fn the_file_descriptor_is_the_spec_s_permissions() {
        let wanted = Descriptor::for_file().unwrap().wanted().unwrap();

        assert_eq!(wanted.owner, administrators());
        assert!(wanted.protected);
        assert_eq!(wanted.dacl, stock_file("").dacl);
    }

    #[skuld::test]
    fn the_folder_descriptor_is_the_same_and_inherited_by_what_is_made_in_it() {
        let wanted = Descriptor::for_folder().unwrap().wanted().unwrap();
        let inherit = OBJECT_INHERIT | CONTAINER_INHERIT;

        assert_eq!(wanted.owner, administrators());
        assert!(wanted.protected);
        assert_eq!(
            wanted.dacl,
            Some(vec![
                allow(system(), FULL, inherit),
                allow(administrators(), FULL, inherit),
                allow(everyone(), READ_EXECUTE, inherit),
            ])
        );
    }
}
