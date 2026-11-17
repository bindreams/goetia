//! The default log's file: what an install makes of it and what the shim may do to it. The files are in
//! temporary directories; a file that is there is the test account's own, so what an install sets on it
//! needs no elevation, and only making one (it names Administrators as its owner) is a test of the `scm`
//! group.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use tempfile::TempDir;

use super::*;
use crate::backend::scm::folder::logs_folder;
use crate::backend::scm::shim::acl::{DEFAULT_LOG_ACCESS, Descriptor, is_as_wanted, read_path};
use crate::backend::scm::sid::{sid_display, sid_from_parts};
use crate::backend::scm::test_support::{permit_only, set_dacl, set_reparse_point, short, withdraw_append};
use crate::elevation::test_support::process_user_sid;
use crate::testing::groups::{Group, scm_group};

/// `IO_REPARSE_TAG_HSM2`: a tag that names no other file, which an unelevated account can set on its own
/// file. It is data on the file, not a way to another one.
const A_TAG: u32 = 0x8000_0007;

/// A tag whose bit 29 is set, which says the file is another name for something else, as a symlink or a mount
/// point does.
const A_LINK_TAG: u32 = 0xA000_0001;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

/// A `%ProgramData%` with the logs folder made plainly, and the path of the log in it.
fn program_data_with_logs() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let logs = logs_folder(dir.path());
    fs::create_dir_all(&logs).unwrap();
    let log = path_in(&logs, "nightly");
    (dir, log)
}

fn entries_of(path: &Path) -> Vec<crate::backend::scm::shim::acl::Ace> {
    let mut entries = read_path(path).unwrap().dacl.expect("a DACL");
    entries.sort();
    entries
}

fn entries_wanted_for(accounts: &[Sid]) -> Vec<crate::backend::scm::shim::acl::Ace> {
    let mut entries = Descriptor::for_default_log(accounts)
        .unwrap()
        .wanted()
        .unwrap()
        .dacl
        .unwrap();
    entries.sort();
    entries
}

#[skuld::test]
fn the_default_log_is_named_for_its_daemon_in_the_logs_folder() {
    assert_eq!(
        path_in(Path::new(r"C:\ProgramData\Goetia\logs"), "svc"),
        PathBuf::from(r"C:\ProgramData\Goetia\logs\svc.log")
    );
}

/// The permissions are set on a log that is there, and it is not otherwise changed.
#[skuld::test]
fn a_log_that_is_there_has_its_permissions_set_to_the_account_s_append_and_nothing_else() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let me = process_user_sid();

    ensure(dir.path(), &id(), std::slice::from_ref(&me)).unwrap();

    let security = read_path(&log).unwrap();
    assert!(security.protected);
    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&me)));
}

/// A reinstall as another account takes the old account's entry away and gives the new one its own.
#[skuld::test]
fn an_account_change_rewrites_the_entry_and_keeps_what_is_in_the_log() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let (first, second) = (process_user_sid(), sid_from_parts(5, &[32, 545]));
    ensure(dir.path(), &id(), std::slice::from_ref(&first)).unwrap();

    ensure(dir.path(), &id(), std::slice::from_ref(&second)).unwrap();

    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&second)));
    assert_eq!(fs::metadata(&log).unwrap().len(), 5, "what the account wrote is kept");
}

/// An account change names the registered account and the new one before the service changes, and the new
/// one alone once it has: what is on the file is computed from the accounts install knows, so it is the same
/// whatever the file held, and what is in the log is kept.
#[skuld::test]
fn an_account_change_names_both_accounts_and_then_the_new_one_alone() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let (first, second) = (process_user_sid(), sid_from_parts(5, &[32, 545]));
    ensure(dir.path(), &id(), std::slice::from_ref(&first)).unwrap();

    ensure(dir.path(), &id(), &[first.clone(), second.clone()]).unwrap();

    assert!(read_path(&log).unwrap().protected);
    assert_eq!(entries_of(&log), entries_wanted_for(&[first, second.clone()]));

    ensure(dir.path(), &id(), std::slice::from_ref(&second)).unwrap();

    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&second)));
    assert_eq!(fs::metadata(&log).unwrap().len(), 5, "what is in the log is kept");
}

/// An entry an administrator added is not carried over: install sets the permissions it computes and does
/// not read the file's back, so a read-only entry for another account is gone after any install.
#[skuld::test]
fn an_entry_an_administrator_added_is_not_carried_over() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    let me = process_user_sid();
    let added = Descriptor::from_sddl(&format!(
        "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)(A;;FA;;;{})",
        sid_display(&me)
    ))
    .unwrap();
    set_dacl(&added, &log).unwrap();
    assert!(entries_of(&log).len() > entries_wanted_for(std::slice::from_ref(&me)).len());

    ensure(dir.path(), &id(), std::slice::from_ref(&me)).unwrap();

    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&me)));
}

/// A handle's rights are fixed when it is opened: a shim that opened its log while its account had an entry
/// keeps appending after an install takes the entry away (an account change, which SCM does not apply to a
/// running service), though a new open is refused. This is what lets install set the permissions it computes
/// without waiting for the running shim to stop.
#[skuld::test]
fn a_handle_opened_while_the_account_had_its_entry_keeps_appending_when_the_entry_is_taken_away() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "first\n").unwrap();
    let me = process_user_sid();
    permit_only(&log, &sid_display(&me));
    let mut held = open_for_append(&log).unwrap();

    withdraw_append(&log, &sid_display(&me));

    let refused = open_for_append(&log).expect_err("the entry is gone, so a new open is refused");
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
    held.write_all(b"second\n")
        .expect("the handle was opened with the right, which the permissions no longer name");
    drop(held);
    permit_only(&log, &sid_display(&me));
    assert_eq!(fs::read_to_string(&log).unwrap(), "first\nsecond\n");
}

/// A log that is another name for a file is not goetia's: nothing is set on it, and it is refused naming it.
#[skuld::test]
fn a_log_that_is_a_link_is_refused_and_its_permissions_are_left_alone() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    set_reparse_point(&log, A_LINK_TAG, b"x");

    let refused = ensure(dir.path(), &id(), &[process_user_sid()]).unwrap_err();

    assert!(
        refused.report().to_string().contains(&log.display().to_string()),
        "{}",
        refused.report()
    );
    assert!(!read_path(&log).unwrap().protected, "its permissions are as they were");
}

/// A tag that names no other file is data on this one, which the install sets the permissions of like any.
#[skuld::test]
fn a_log_with_a_tag_that_is_no_link_has_its_permissions_set() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    set_reparse_point(&log, A_TAG, b"x");
    let me = process_user_sid();

    ensure(dir.path(), &id(), std::slice::from_ref(&me)).unwrap();

    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&me)));
}

/// The folder is an install's, made before the file: where it is not there nothing is made.
#[skuld::test]
fn a_missing_logs_folder_is_an_error_and_nothing_is_made() {
    let dir = tempfile::tempdir().unwrap();

    let refused = ensure(dir.path(), &id(), &[process_user_sid()]).unwrap_err();

    assert!(
        refused.report().to_string().contains("nightly.log"),
        "{}",
        refused.report()
    );
    assert!(!dir.path().join("Goetia").exists());
}

/// Making the file names Administrators as its owner, which only an elevated token may.
#[skuld::test]
fn a_log_that_is_missing_is_made_empty_with_the_whole_descriptor(#[fixture(scm_group)] _group: &Group) {
    let (dir, log) = program_data_with_logs();
    let me = process_user_sid();

    ensure(dir.path(), &id(), std::slice::from_ref(&me)).unwrap();

    assert_eq!(fs::metadata(&log).unwrap().len(), 0);
    let wanted = Descriptor::for_default_log(std::slice::from_ref(&me))
        .unwrap()
        .wanted()
        .unwrap();
    assert!(is_as_wanted(&read_path(&log).unwrap(), &wanted));
    ensure(dir.path(), &id(), std::slice::from_ref(&me)).unwrap();
}

/// The shim never makes the file: one that is missing is an open that fails, and nothing is made.
#[skuld::test]
fn the_shim_does_not_make_a_missing_log() {
    let (_dir, log) = program_data_with_logs();

    let refused = open_for_append(&log).unwrap_err();

    assert_eq!(refused.kind(), io::ErrorKind::NotFound);
    assert!(!log.exists());
}

/// What an account may do to its log is append, read the attributes and wait on the handle, and nothing
/// that changes what is in it, who may reach it or who owns it.
#[skuld::test]
fn the_access_granted_on_a_default_log_is_append_and_nothing_that_rewrites_it() {
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
        SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
    };

    assert_eq!(
        DEFAULT_LOG_ACCESS,
        FILE_APPEND_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE
    );
    for denied in [
        FILE_READ_DATA,
        FILE_WRITE_DATA,
        FILE_WRITE_ATTRIBUTES,
        DELETE,
        WRITE_DAC,
        WRITE_OWNER,
    ] {
        assert_eq!(DEFAULT_LOG_ACCESS & denied, 0, "{denied:#x}");
    }
}

/// The open is for exactly what the account is granted: it appends, after what is there, and the handle
/// can do nothing else.
#[skuld::test]
fn the_shim_appends_to_a_log_through_a_handle_that_can_do_nothing_else() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "first\n").unwrap();

    let mut file = open_for_append(&log).unwrap();
    file.write_all(b"second\n").unwrap();

    assert_eq!(fs::read_to_string(&log).unwrap(), "first\nsecond\n");
    let mut read = [0u8; 1];
    let denied = file.read(&mut read).unwrap_err();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied, "the handle cannot read");
}

/// A log that is another name for a file is refused on the shim's own handle, which a link set after the
/// install checked it would otherwise lead to anywhere.
#[skuld::test]
fn the_shim_refuses_a_log_that_is_a_link() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    set_reparse_point(&log, A_LINK_TAG, b"x");

    let refused = open_for_append(&log).unwrap_err();

    assert!(refused.to_string().contains(&log.display().to_string()), "{refused}");
}

/// The refusal is the install's and the shim's alike: one message, naming the file and the way out.
#[skuld::test]
fn the_install_and_the_shim_refuse_a_link_in_the_same_words() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    set_reparse_point(&log, A_LINK_TAG, b"x");

    let install = ensure(dir.path(), &id(), &[process_user_sid()])
        .unwrap_err()
        .report()
        .to_string();
    let shim = open_for_append(&log).unwrap_err().to_string();

    assert!(install.contains(&shim), "{install} / {shim}");
}

/// A tag that names no other file is data on the file: the shim appends through it.
#[skuld::test]
fn the_shim_appends_to_a_log_with_a_tag_that_is_no_link() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "first\n").unwrap();
    set_reparse_point(&log, A_TAG, b"x");

    let mut file = open_for_append(&log).expect("a tag that is no link is not refused");
    file.write_all(b"second\n").unwrap();
    drop(file);

    // A file with a tag is read as itself, as a file with any other tag is.
    let mut written = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&log)
        .unwrap()
        .read_to_string(&mut written)
        .unwrap();
    assert_eq!(written, "first\nsecond\n");
}

// A `logs:` in the folder of the default logs -------------------------------------------------------------------------

/// What the install says of `logs`, for the daemon `nightly` under the `%ProgramData%` of `dir`.
fn refusal_of(dir: &TempDir, logs: &Path) -> Option<String> {
    refuse_logs_in_folder(dir.path(), &id(), Some(logs))
        .err()
        .map(|e| e.report().to_string())
}

/// A `logs:` anywhere in the logs folder is refused, in words that name the folder and the way out: a service
/// account cannot make a file there, and one that runs as SYSTEM would append to another daemon's default
/// log. Which file in the folder it names does not matter.
#[skuld::test]
fn a_logs_in_the_logs_folder_is_refused_whichever_file_it_names() {
    let (dir, log) = program_data_with_logs();
    let folder = log.parent().unwrap().to_path_buf();
    fs::write(&log, "").unwrap();
    let other = path_in(&folder, "another-daemon");
    fs::write(&other, "").unwrap();

    for named in [
        log.clone(),
        other,
        folder.join("not-a-log.txt"),
        folder.join("sub").join("x.log"),
    ] {
        let said = refusal_of(&dir, &named).unwrap_or_else(|| panic!("{} was accepted", named.display()));

        assert!(said.contains("is inside goetia's own log folder"), "{said}");
        assert!(said.contains(&named.display().to_string()), "{said}");
        assert!(said.contains(&folder.display().to_string()), "{said}");
        assert!(said.contains("remove `logs:` to use the default log"), "{said}");
    }
}

/// A first install has no `<id>.log` yet, and no other file or folder under the logs folder either: only the
/// part of the path that exists is looked at.
#[skuld::test]
fn a_logs_naming_the_default_log_of_a_first_install_is_refused() {
    let (dir, log) = program_data_with_logs();
    assert!(!log.exists());

    assert!(refusal_of(&dir, &log).is_some());
}

/// Windows reads a path by its own rules, which the check follows by asking the file system for the folder:
/// a spelling in another case, as an 8.3 alias, with `..` or with a trailing dot is in the folder too, whether
/// or not the file is there.
#[skuld::test]
fn a_logs_spelled_another_way_is_still_in_the_logs_folder() {
    let (dir, log) = program_data_with_logs();
    let folder = log.parent().unwrap().to_path_buf();
    let upper = |path: &Path| PathBuf::from(path.to_string_lossy().to_uppercase());

    for exists in [false, true] {
        if exists {
            fs::write(&log, "").unwrap();
        }
        let mut spelled = vec![
            upper(&log),
            short(&folder).join("nightly.log"),
            folder.join("sub").join("..").join("nightly.log"),
            folder.join("nightly.log."),
            folder.join("nightly.log "),
        ];
        if exists {
            spelled.push(short(&log));
        }
        for spelled in spelled {
            assert!(
                refusal_of(&dir, &spelled).is_some(),
                "exists={exists}: {} was accepted",
                spelled.display()
            );
        }
    }
}

/// Elsewhere is accepted: a file next to the logs folder, a folder whose name starts with its name, a folder
/// that is not there at all, and no `logs:`.
#[skuld::test]
fn a_logs_elsewhere_is_accepted() {
    let (dir, log) = program_data_with_logs();
    let folder = log.parent().unwrap().to_path_buf();
    let goetia = folder.parent().unwrap().to_path_buf();
    fs::create_dir(goetia.join("logs-old")).unwrap();

    for elsewhere in [
        goetia.join("mine.log"),
        goetia.join("logs-old").join("x.log"),
        goetia.join("logs.log"),
        dir.path().join("somewhere").join("not").join("made").join("x.log"),
        goetia.join("runs").join("..").join("mine.log"),
    ] {
        assert_eq!(refusal_of(&dir, &elsewhere), None, "{}", elsewhere.display());
    }
    assert!(refuse_logs_in_folder(dir.path(), &id(), None).is_ok());
}
