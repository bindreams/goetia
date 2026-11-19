//! `is_as_wanted` over stated tables, so these run on every host. The Windows-only tests build
//! descriptors from the SDDL strings and read them back, touching no file.

use std::path::PathBuf;

use super::*;
use crate::backend::scm::shim::acl::ObjectSecurity;
use crate::backend::scm::shim::acl::acl_tests::{CONTAINER_INHERIT, FULL, OBJECT_INHERIT, READ_EXECUTE, allow, users};
use crate::backend::scm::sid::{administrators, everyone, system};

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
fn a_conditional_entry_is_not_a_plain_one() {
    let mut actual = stock_file("");
    actual.dacl.as_mut().unwrap()[2].conditional = true;

    assert!(!is_as_wanted(&actual, &stock_file("")));
    // Order does not matter between a conditional entry and a plain one of the same trustee.
    let mut wanted = stock_file("");
    let dacl = wanted.dacl.as_mut().unwrap();
    dacl.push(Ace {
        conditional: true,
        ..dacl[2].clone()
    });
    let mut reversed = wanted.clone();
    reversed.dacl.as_mut().unwrap().reverse();
    assert!(is_as_wanted(&reversed, &wanted));
}

#[skuld::test]
fn a_null_dacl_is_not_as_wanted() {
    let mut actual = stock_file("");
    actual.dacl = None;

    assert!(!is_as_wanted(&actual, &stock_file("")));
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};

    use super::super::system::created;
    use super::*;
    use crate::error::Error;
    use crate::testing::groups::{Group, scm_group};

    const INHERIT_ONLY: u8 = 0x08;

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

    #[skuld::test]
    fn the_goetia_descriptor_names_the_owner_and_leaves_the_dacl_to_inherit() {
        let wanted = Descriptor::for_goetia().unwrap().wanted().unwrap();

        assert_eq!(wanted.owner, administrators());
        assert_eq!(
            wanted.dacl, None,
            "no DACL is stated, so the folder inherits its parent's"
        );
    }

    #[skuld::test]
    fn the_runs_descriptor_names_the_owner_and_the_principal() {
        let me = users();
        let wanted = Descriptor::for_runs(std::slice::from_ref(&me)).unwrap().wanted().unwrap();
        let inherit = OBJECT_INHERIT | CONTAINER_INHERIT;
        let mut expected = vec![
            allow(system(), FULL, inherit),
            allow(administrators(), FULL, inherit),
            // The files made in it, and the directory itself without a subdirectory, with delete-child
            // and without `DELETE` (0x1_0000) on itself; neither can set attributes or extended
            // attributes (0x110).
            allow(me.clone(), 0x13_00AF, OBJECT_INHERIT | INHERIT_ONLY),
            allow(me, 0x12_00EB, 0),
        ];

        assert_eq!(wanted.owner, administrators());
        assert!(wanted.protected);
        let mut actual = wanted.dacl.unwrap();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }

    /// An account change names both accounts alike, each with the entries of the one-account descriptor.
    #[skuld::test]
    fn the_runs_descriptor_gives_each_of_its_principals_the_same_entries() {
        let (first, second) = (users(), crate::backend::scm::sid::sid_from_parts(5, &[19]));
        let wanted = Descriptor::for_runs(&[first.clone(), second.clone()])
            .unwrap()
            .wanted()
            .unwrap();
        let inherit = OBJECT_INHERIT | CONTAINER_INHERIT;
        let mut expected = vec![
            allow(system(), FULL, inherit),
            allow(administrators(), FULL, inherit),
            allow(first.clone(), 0x13_00AF, OBJECT_INHERIT | INHERIT_ONLY),
            allow(first, 0x12_00EB, 0),
            allow(second.clone(), 0x13_00AF, OBJECT_INHERIT | INHERIT_ONLY),
            allow(second, 0x12_00EB, 0),
        ];

        let mut actual = wanted.dacl.unwrap();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[skuld::test]
    fn the_logs_descriptor_is_protected_and_names_system_and_administrators_alone() {
        let wanted = Descriptor::for_logs().unwrap().wanted().unwrap();
        let inherit = OBJECT_INHERIT | CONTAINER_INHERIT;

        assert_eq!(wanted.owner, administrators());
        assert!(wanted.protected, "nothing is inherited from %ProgramData%");
        assert_eq!(
            wanted.dacl,
            Some(vec![
                allow(system(), FULL, inherit),
                allow(administrators(), FULL, inherit)
            ])
        );
    }

    #[skuld::test]
    fn the_default_log_descriptor_names_the_owner_and_the_account_s_append_alone() {
        let account = users();
        let wanted = Descriptor::for_default_log(std::slice::from_ref(&account))
            .unwrap()
            .wanted()
            .unwrap();

        assert_eq!(wanted.owner, administrators());
        assert!(wanted.protected);
        // Append data, read attributes and synchronize: it cannot read, write data, delete, or change
        // the file's permissions or owner.
        assert_eq!(
            wanted.dacl,
            Some(vec![
                allow(system(), FULL, 0),
                allow(administrators(), FULL, 0),
                allow(account, 0x10_0084, 0)
            ])
        );
    }

    #[skuld::test]
    fn the_default_log_descriptor_refuses_bytes_that_are_not_a_sid() {
        assert!(Descriptor::for_default_log(&[vec![1, 2, 3]]).is_err());
    }

    /// An account that is changing is named beside the old one, each for append alone.
    #[skuld::test]
    fn the_default_log_descriptor_names_each_account_it_is_given() {
        let (first, second) = (users(), everyone());
        let wanted = Descriptor::for_default_log(&[first.clone(), second.clone()])
            .unwrap()
            .wanted()
            .unwrap();

        assert_eq!(
            wanted.dacl,
            Some(vec![
                allow(system(), FULL, 0),
                allow(administrators(), FULL, 0),
                allow(first, 0x10_0084, 0),
                allow(second, 0x10_0084, 0)
            ])
        );
    }

    // What the runs descriptor grants is read back from a real directory by `runs_tests`.
    #[skuld::test]
    fn the_runs_descriptor_refuses_bytes_that_are_not_a_sid() {
        assert!(Descriptor::for_runs(&[vec![1, 2, 3]]).is_err());
    }

    #[skuld::test]
    fn the_folders_to_create_are_those_below_the_first_that_exists_outermost_first() {
        let root = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            root.path().join("a"),
            root.path().join("a").join("b"),
            root.path().join("a").join("b").join("c"),
        );

        assert_eq!(missing_components(&c).unwrap(), [a.clone(), b.clone(), c.clone()]);
        std::fs::create_dir(&a).unwrap();
        assert_eq!(missing_components(&c).unwrap(), [b, c]);
        assert!(missing_components(&a).unwrap().is_empty());
    }

    /// A folder found missing that exists when it is created was made by someone else in between,
    /// which goetia cannot tell from a hostile one: it is refused, not adopted.
    #[skuld::test]
    fn a_folder_that_appeared_after_it_was_found_missing_is_refused() {
        let path = Path::new(r"C:\Goetia");

        let refused = created(path, io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
        let failed = created(path, io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));

        let Error::Step { what, source } = &refused else {
            panic!("expected a refusal, got {refused:?}");
        };
        assert!(what.contains(r"C:\Goetia") && what.contains("appeared"), "{what}");
        assert!(matches!(**source, Error::AlreadyExists { .. }), "{source:?}");
        assert!(matches!(failed, Error::Os { .. }), "{failed:?}");
    }

    /// The race itself needs a folder to appear between the two steps, which no test can arrange
    /// without a seam: the folder that "appeared" is one already in the list.
    #[skuld::test]
    fn creating_stops_at_a_folder_that_exists_and_leaves_it_alone(#[fixture(scm_group)] _group: &Group) {
        let root = tempfile::tempdir().unwrap();
        let (made, taken, after) = (
            root.path().join("made"),
            root.path().join("taken"),
            root.path().join("after"),
        );
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("theirs"), b"x").unwrap();

        let refused = Descriptor::for_folder()
            .unwrap()
            .create_each(&[made.clone(), taken.clone(), after.clone()])
            .unwrap_err();

        assert!(matches!(refused, Error::Step { .. }), "{refused:?}");
        assert!(made.is_dir(), "what came before is goetia's own");
        assert!(!after.exists(), "nothing is created past the refusal");
        assert!(taken.join("theirs").is_file(), "the folder that was there is untouched");
    }
}
