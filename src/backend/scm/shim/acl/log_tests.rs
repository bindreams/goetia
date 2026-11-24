//! The check of a daemon's own `logs:` path, on folders and files these tests make in a temporary directory
//! (ordinary files, with permissions the tests set on them: no system state is touched), and on stated entries for
//! the file the shim would make.

use std::fs;
use std::path::{Path, PathBuf};

use skuld::temp_dir;

use super::*;
use crate::backend::scm::shim::acl::acl_tests::windows::junction;
use crate::backend::scm::shim::acl::acl_tests::{FULL, OBJECT_INHERIT, allow, users};
use crate::backend::scm::sid::{administrators, sid_display, sid_from_parts, system};
use crate::elevation::test_support::process_user_sid;

const CONTAINER_INHERIT_ONLY: u8 = 0x02 | 0x08;
const OBJECT_INHERIT_ONLY: u8 = OBJECT_INHERIT | 0x08;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

/// The daemon of these tests: the account this process runs as, which owns what the tests make.
fn daemon() -> Sid {
    process_user_sid()
}

/// Makes the folder `parent\name` with protected permissions: SYSTEM and Administrators full control, the daemon
/// modify, and the entries `extra` (SDDL), each inherited by what is made in it. Returns its final path.
fn make_folder(parent: &Path, name: &str, extra: &str) -> PathBuf {
    let sddl = format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;{}){extra}",
        sid_display(&daemon())
    );
    let folder = parent.join(name);
    Descriptor::from_sddl(&sddl).unwrap().create_dir(&folder).unwrap();
    folder.canonicalize().unwrap()
}

/// Makes the file `path` with permissions of its own, whatever its folder would have it inherit.
fn make_file(path: &Path) {
    let sddl = format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1301bf;;;{})", sid_display(&daemon()));
    Descriptor::from_sddl(&sddl).unwrap().create_file(path).unwrap();
}

fn check(log: &Path) -> Result<()> {
    FolderCheck::new(CheckPermissions::Enable).check_log(&id(), log, &daemon())
}

/// What `check` refused of `log` for its permissions: the object, the right, and the account that has it.
fn unsafe_refusal(log: &Path) -> (PathBuf, &'static str, String) {
    match check(log) {
        Err(Error::FolderUnsafe {
            path,
            right,
            account,
            role,
        }) => {
            let FolderRole::DaemonLog { daemon: named, .. } = *role else {
                panic!("a daemon's log is refused as one: {role:?}");
            };
            assert_eq!(named, account_name(&daemon()), "the refusal names the daemon's account");
            (path, right, account)
        }
        other => panic!("{} must be refused for its permissions: {other:?}", log.display()),
    }
}

/// What `check` refused of `log` for what it is or where it is, as the text the user reads.
fn invalid_refusal(log: &Path) -> String {
    match check(log) {
        Err(Error::Invalid { daemon, message, .. }) => {
            assert_eq!(daemon, "nightly");
            message
        }
        other => panic!("{} must be refused: {other:?}", log.display()),
    }
}

// accepted ------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_log_in_a_folder_only_the_daemon_and_administrators_can_change_is_accepted(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "");
    let log = folder.join("app.log");

    assert!(check(&log).is_ok(), "a file the shim would make");

    fs::write(&log, "x").unwrap();
    assert!(
        check(&log).is_ok(),
        "a file that is there, as it inherited its permissions"
    );
}

/// A file that is there is judged by its own permissions, whatever its folder would have it inherit.
#[skuld::test]
fn a_log_that_is_there_is_judged_as_it_is_and_not_by_what_its_folder_would_pass_on(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "(A;OIIO;FA;;;BU)");
    let log = folder.join("app.log");
    make_file(&log);

    assert!(check(&log).is_ok());
}

/// An entry for accounts to add files that does not reach the folder's own folder is no refusal of the folder
/// above: only the folder the log is looked up in is judged strictly.
#[skuld::test]
fn an_account_that_adds_entries_above_the_logs_folder_is_accepted(#[fixture(temp_dir)] dir: &Path) {
    let outer = make_folder(&dir.canonicalize().unwrap(), "outer", "");
    let loose = format!(
        "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1301bf;;;{})(A;;0x2;;;BU)",
        sid_display(&daemon())
    );
    Descriptor::from_sddl(&loose)
        .unwrap()
        .create_dir(&outer.join("loose"))
        .unwrap();
    let folder = make_folder(&outer.join("loose"), "app", "");

    assert!(check(&folder.join("app.log")).is_ok());
}

#[skuld::test]
fn a_disabled_check_reads_nothing() {
    let log = Path::new(r"Z:\no\such\volume\app.log");
    assert!(
        FolderCheck::new(CheckPermissions::Disable)
            .check_log(&id(), log, &daemon())
            .is_ok()
    );
}

// refused for its permissions -----------------------------------------------------------------------------------------

#[skuld::test]
fn a_folder_everyone_can_write_is_refused(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "(A;OICI;FA;;;WD)");

    let (path, right, _account) = unsafe_refusal(&folder.join("app.log"));

    assert_eq!(path, display_path(&folder));
    assert_eq!(right, CHANGE_PERMISSIONS_OF);
}

/// An account that may only add entries cannot change the folder, but plants a link at the log's name.
#[skuld::test]
fn an_account_that_can_add_entries_to_the_logs_folder_is_refused(#[fixture(temp_dir)] dir: &Path) {
    for mask in ["0x2", "0x4"] {
        let folder = make_folder(
            &dir.canonicalize().unwrap(),
            &format!("app{mask}"),
            &format!("(A;OICI;{mask};;;BU)"),
        );

        let (path, right, account) = unsafe_refusal(&folder.join("app.log"));

        assert_eq!((path, right), (display_path(&folder), ADD_ENTRIES_TO), "{mask}");
        assert_eq!(account, account_name(&users()), "{mask}");
    }
}

/// The file the shim will make inherits what its folder passes on to files: an entry that only reaches files
/// (inherit-only) is nothing to the folder and everything to the log.
#[skuld::test]
fn a_log_the_shim_would_make_is_judged_by_what_it_would_inherit(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "(A;OIIO;0x2;;;BU)");
    let log = folder.join("app.log");

    let (path, right, account) = unsafe_refusal(&log);

    assert_eq!((right, account), (WRITE_TO, account_name(&users())));
    assert_eq!(path.file_name(), log.file_name());
    assert_eq!(path.parent(), Some(display_path(&folder).as_path()));
}

// refused for what it is ----------------------------------------------------------------------------------------------

#[skuld::test]
fn a_missing_folder_is_refused_naming_the_first_that_is_missing(#[fixture(temp_dir)] dir: &Path) {
    let root = dir.canonicalize().unwrap();
    let log = root.join("a").join("b").join("app.log");

    let said = invalid_refusal(&log);

    assert!(
        said.contains(&format!("does not exist ({})", root.join("a").display())),
        "{said}"
    );
    assert!(said.contains("icacls"), "{said}");
    assert!(!root.join("a").exists(), "the check made a folder");
}

#[skuld::test]
fn a_folder_that_is_a_file_is_refused(#[fixture(temp_dir)] dir: &Path) {
    let root = dir.canonicalize().unwrap();
    fs::write(root.join("app"), "x").unwrap();

    let said = invalid_refusal(&root.join("app").join("app.log"));

    assert!(said.contains("not a folder"), "{said}");
}

/// A link at the log's name is refused as itself, not followed and judged by where it leads.
#[skuld::test]
fn a_link_at_the_logs_name_is_refused(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "");
    let elsewhere = make_folder(&dir.canonicalize().unwrap(), "elsewhere", "");
    let log = folder.join("app.log");
    junction(&log, &elsewhere);

    let said = invalid_refusal(&log);

    assert!(said.contains("is a link"), "{said}");
    assert!(said.contains(&log.display().to_string()), "{said}");
}

/// A hard link planted while the folder could be added to would survive the remedy.
#[skuld::test]
fn a_log_with_two_names_is_refused(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "");
    let log = folder.join("app.log");
    make_file(&log);
    fs::hard_link(&log, folder.join("other-name")).unwrap();

    let said = invalid_refusal(&log);

    assert!(said.contains("more than one name"), "{said}");
}

#[skuld::test]
fn a_folder_at_the_logs_name_is_refused(#[fixture(temp_dir)] dir: &Path) {
    let folder = make_folder(&dir.canonicalize().unwrap(), "app", "");
    fs::create_dir(folder.join("app.log")).unwrap();

    let said = invalid_refusal(&folder.join("app.log"));

    assert!(said.contains("is a folder"), "{said}");
}

// the final folder ----------------------------------------------------------------------------------------------------

/// A junction on the way is judged through to the folder the log's name is looked up in, which is the final one:
/// the strict rule applies to it, whatever the junction's own permissions are.
#[skuld::test]
fn a_junction_on_the_way_is_judged_through_to_the_final_folder(#[fixture(temp_dir)] dir: &Path) {
    let root = dir.canonicalize().unwrap();
    let safe = make_folder(&root, "safe", "");
    let open = make_folder(&root, "open", "(A;OICI;0x2;;;BU)");
    let to_safe = root.join("to-safe");
    let to_open = root.join("to-open");
    junction(&to_safe, &safe);
    junction(&to_open, &open);

    assert!(check(&to_safe.join("app.log")).is_ok());
    let (path, right, _account) = unsafe_refusal(&to_open.join("app.log"));
    assert_eq!((path, right), (display_path(&open), ADD_ENTRIES_TO));
}

// the file the shim would make ----------------------------------------------------------------------------------------

/// A folder as the entries `acl` read from it, whose entries are passed on to what is made in it.
fn folder_with(dacl: Vec<Ace>) -> ObjectSecurity {
    ObjectSecurity {
        path: PathBuf::from(r"C:\Logs\app"),
        owner: administrators(),
        protected: true,
        kind: ObjectKind::LogFolder,
        dacl: Some(dacl),
    }
}

fn account() -> Sid {
    sid_from_parts(5, &[21, 1, 2, 3, 1001])
}

fn judged(dacl: Vec<Ace>) -> std::result::Result<(), Unsafe> {
    let file = inherited_file(&folder_with(dacl), &account(), PathBuf::from(r"C:\Logs\app\app.log"));
    verdict(&[file], Some(&account()))
}

#[skuld::test]
fn the_made_file_is_the_daemons_and_holds_what_its_folder_passes_to_files() {
    let owner = sid_from_parts(3, &[0]);
    let file = inherited_file(
        &folder_with(vec![
            allow(system(), FULL, OBJECT_INHERIT),
            allow(owner, 0x1301bf, OBJECT_INHERIT_ONLY),
            allow(users(), FULL, CONTAINER_INHERIT_ONLY),
            allow(users(), FULL, 0),
        ]),
        &account(),
        PathBuf::from(r"C:\Logs\app\app.log"),
    );

    assert_eq!(file.kind, ObjectKind::File);
    assert_eq!(file.owner, account());
    assert_eq!(file.path, PathBuf::from(r"C:\Logs\app\app.log"));
    let held: Vec<(Sid, u8)> = file
        .dacl
        .unwrap()
        .into_iter()
        .map(|ace| (ace.trustee, ace.flags))
        .collect();
    // CREATOR OWNER is the daemon, and an entry that is inherit-only on the folder applies to the file.
    assert_eq!(held, vec![(system(), 0), (account(), 0)]);
}

#[skuld::test]
fn a_creator_owner_entry_is_the_daemon_and_passes() {
    let creator_owner = sid_from_parts(3, &[0]);
    assert!(judged(vec![allow(creator_owner, FULL, OBJECT_INHERIT_ONLY)]).is_ok());
}

#[skuld::test]
fn a_creator_group_entry_is_not_trusted() {
    let creator_group = sid_from_parts(3, &[1]);

    let refusal = judged(vec![allow(creator_group.clone(), FULL, OBJECT_INHERIT_ONLY)]).unwrap_err();

    assert_eq!(refusal.account, creator_group);
    assert_eq!(refusal.path, PathBuf::from(r"C:\Logs\app\app.log"));
}

#[skuld::test]
fn an_inheritable_entry_for_files_that_lets_an_untrusted_account_tamper_is_refused() {
    for (mask, right) in [
        (0x2, WRITE_TO),
        (0x4, WRITE_TO),
        (0x4_0000, CHANGE_PERMISSIONS_OF),
        (0x8_0000, TAKE_OWNERSHIP_OF),
        (0x1_0000, DELETE_OR_RENAME),
    ] {
        for flags in [OBJECT_INHERIT, OBJECT_INHERIT_ONLY, OBJECT_INHERIT | 0x02] {
            let refusal = judged(vec![allow(users(), mask, flags)]).unwrap_err();
            assert_eq!(
                (refusal.right, refusal.account),
                (right, users()),
                "{mask:#x} {flags:#x}"
            );
        }
    }
}

#[skuld::test]
fn what_a_file_does_not_inherit_is_not_judged() {
    assert!(
        judged(vec![
            // Folders only, and not inheritable at all.
            allow(users(), FULL, CONTAINER_INHERIT_ONLY),
            allow(users(), FULL, 0),
            // Read access is not tampering.
            allow(users(), 0x12_0089, OBJECT_INHERIT),
        ])
        .is_ok()
    );
}
