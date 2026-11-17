use std::cell::{Cell, RefCell};
use std::fs;
use std::path::Path;

use super::*;
use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck, is_as_wanted, read_path};
use crate::backend::scm::sid::administrators;
use crate::testing::groups::{Group, scm_group};

fn enabled() -> FolderCheck {
    FolderCheck::new(CheckPermissions::Enable)
}

/// A `Goetia` folder that is there before goetia makes one is refused when its owner is not trusted (the
/// folder here is the test account's own), and nothing is made in it: a standard user can create it in
/// `%ProgramData%`, and put a link in it.
#[skuld::test]
fn a_goetia_folder_that_is_there_and_is_not_safe_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let goetia = goetia_folder(dir.path());
    fs::create_dir(&goetia).unwrap();

    let refused = make_goetia_folders(dir.path(), |path| enabled().check(path)).unwrap_err();

    let Error::FolderUnsafe { path, .. } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &goetia);
    assert_eq!(fs::read_dir(&goetia).unwrap().count(), 0, "nothing was made in it");
}

/// The logs folder is judged as `Goetia` is: one that is there and is not safe is refused, and the
/// refusal names it.
#[skuld::test]
fn a_logs_folder_that_is_there_and_is_not_safe_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let logs = logs_folder(dir.path());
    fs::create_dir_all(&logs).unwrap();

    // `Goetia` passes, as an elevated install's folder does; only the logs folder is judged by the check.
    let refused = make_goetia_folders(
        dir.path(),
        |path| {
            if path == logs { enabled().check(path) } else { Ok(()) }
        },
    )
    .unwrap_err();

    let Error::FolderUnsafe { path, .. } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &logs);
}

/// A `Goetia` folder that a standard user makes after the first judgement, which saw it missing, and before
/// goetia looks for what is missing, is judged all the same: the state a folder is left in is what is
/// judged, whichever step found it.
#[skuld::test]
fn a_goetia_folder_that_appears_after_the_first_judgement_is_judged_before_it_is_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let goetia = goetia_folder(dir.path());
    let judged_goetia = Cell::new(false);

    // The first judgement of `Goetia` passes, as one of a missing folder under safe ancestors does; the
    // folder appears right after it. Every later one is the real check, which this account's folder fails.
    let refused = make_goetia_folders(dir.path(), |path| {
        if path != goetia {
            Ok(())
        } else if judged_goetia.replace(true) {
            enabled().check(path)
        } else {
            fs::create_dir(&goetia).unwrap();
            Ok(())
        }
    })
    .unwrap_err();

    let Error::FolderUnsafe { path, .. } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &goetia);
}

/// The logs folder is goetia's alone to make: one that is there and is not as goetia makes it (a standard
/// user can make it first, and the folder of an earlier goetia inherits what `%ProgramData%` gives) is
/// refused, naming it, and left as it is. `Goetia` passes the judgement, as an elevated install's does.
#[skuld::test]
fn a_logs_folder_that_is_there_and_is_not_as_made_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (goetia, logs) = (goetia_folder(dir.path()), logs_folder(dir.path()));
    fs::create_dir(&goetia).unwrap();
    fs::create_dir(&logs).unwrap();

    let refused = make_goetia_folders(dir.path(), |_| Ok(())).unwrap_err();

    let Error::FolderNotAsMade { path } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &logs);
    assert_eq!(fs::read_dir(&logs).unwrap().count(), 0, "nothing was made in it");
}

/// A logs folder that a standard user makes after goetia found it missing is refused, not adopted.
#[skuld::test]
fn a_logs_folder_that_appears_after_the_first_judgement_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (goetia, logs) = (goetia_folder(dir.path()), logs_folder(dir.path()));
    fs::create_dir(&goetia).unwrap();
    let judged_logs = Cell::new(false);

    let refused = make_goetia_folders(dir.path(), |path| {
        if path == logs && !judged_logs.replace(true) {
            fs::create_dir(&logs).unwrap();
        }
        Ok(())
    })
    .unwrap_err();

    let Error::FolderNotAsMade { path } = &refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(path, &logs);
}

fn owner_and_protection_of(folder: &Path) -> (Vec<u8>, bool) {
    let security = read_path(folder).unwrap();
    (security.owner, security.protected)
}

/// Making the folders names Administrators as their owner, which only an elevated token may.
#[skuld::test]
fn the_folders_are_made_owned_by_administrators_goetia_inheriting_and_logs_protected_and_each_is_judged_before_and_after(
    #[fixture(scm_group)] _group: &Group,
) {
    let dir = tempfile::tempdir().unwrap();
    let (goetia, logs) = (goetia_folder(dir.path()), logs_folder(dir.path()));
    let judged = RefCell::new(Vec::new());
    let judge = |path: &Path| {
        judged.borrow_mut().push(path.to_path_buf());
        Ok(())
    };

    make_goetia_folders(dir.path(), judge).unwrap();

    assert_eq!(
        *judged.borrow(),
        [goetia.clone(), goetia.clone(), logs.clone(), logs.clone()]
    );
    for folder in [&goetia, &logs] {
        let (owner, _) = owner_and_protection_of(folder);
        assert_eq!(
            owner,
            administrators(),
            "{}: named, whatever the creator's own default",
            folder.display()
        );
    }
    assert!(!owner_and_protection_of(&goetia).1, "it inherits what %ProgramData% gives");
    let wanted = Descriptor::for_logs().unwrap().wanted().unwrap();
    assert!(
        is_as_wanted(&read_path(&logs).unwrap(), &wanted),
        "logs is protected, and SYSTEM and Administrators alone have entries"
    );
    judged.borrow_mut().clear();
    make_goetia_folders(dir.path(), judge).unwrap();
    assert_eq!(
        judged.borrow().len(),
        4,
        "a second call adopts what it made, judging it"
    );
}

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
