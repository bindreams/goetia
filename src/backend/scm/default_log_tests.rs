//! The default log's file: what an install makes of it and what the shim may do to it. The files are in
//! temporary directories. An install names Administrators as the owner of the file it makes and of the one it
//! sets the permissions of, which only an elevated token may assign: a test of that is in the `scm` group, and
//! any other sets the permissions alone, as an unelevated token does.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use tempfile::TempDir;

use super::*;
use crate::backend::scm::folder::logs_folder;
use crate::backend::scm::shim::acl::{DEFAULT_LOG_ACCESS, Descriptor, is_as_wanted, read_path};
use crate::backend::scm::sid::{sid_display, sid_from_parts};
use crate::backend::scm::test_support::{
    delete_pending, effective_rights, make_mount_point, permit_only, set_dacl, set_owner, set_reparse_point, short,
    withdraw_append,
};
use crate::elevation::test_support::{ThreadTokenReset, process_user_sid, restricted_token_copy};
use crate::testing::groups::{Group, scm_group};
use windows_sys::Win32::Security::{DISABLE_MAX_PRIVILEGE, TOKEN_IMPERSONATE, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

/// `IO_REPARSE_TAG_HSM2`: a tag that names no other file, which an unelevated account can set on its own
/// file. It is data on the file, not a way to another one.
const A_TAG: u32 = 0x8000_0007;

/// A tag whose bit 29 is set, which says the file is another name for something else, as a symlink or a mount
/// point does.
const A_LINK_TAG: u32 = 0xA000_0001;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

/// [`super::ensure`] as an install that rewrites the file there is, and makes one, for `accounts`.
fn ensure(program_data: &Path, id: &Id, accounts: &[Sid]) -> Result<EnsureOutcome> {
    super::ensure(program_data, id, &Naming::alike(accounts))
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
fn a_log_that_is_there_has_its_permissions_set_to_the_account_s_append_and_nothing_else(
    #[fixture(scm_group)] _group: &Group,
) {
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

/// An install that asks to keep what is there leaves the permissions of a log that is there as they are: the
/// one made while a pending start may still need the account the file no longer names.
#[skuld::test]
fn a_log_that_is_there_is_left_as_it_is_when_the_install_keeps_it() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let (old, new) = (process_user_sid(), sid_from_parts(5, &[32, 545]));
    ensure(dir.path(), &id(), &[old.clone(), new]).unwrap();
    let before = entries_of(&log);

    let naming = Naming {
        made: vec![old.clone()],
        existing: Existing::Keep,
    };
    let outcome = super::ensure(dir.path(), &id(), &naming).unwrap();

    assert_eq!(outcome, EnsureOutcome::Kept);
    assert_eq!(entries_of(&log), before, "its permissions are as they were");
    assert_eq!(fs::metadata(&log).unwrap().len(), 5, "what is in the log is kept");
}

/// What an install says of the file: it made it, or set the permissions of the one there.
#[skuld::test]
fn an_install_says_whether_it_set_the_permissions_of_a_log_that_was_there() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();

    let outcome = ensure(dir.path(), &id(), &[process_user_sid()]).unwrap();

    assert_eq!(outcome, EnsureOutcome::Set);
}

/// A file that is missing is made even where the install keeps what is there, with the accounts it was given.
#[skuld::test]
fn a_log_that_is_missing_is_made_even_when_the_install_keeps_what_is_there(#[fixture(scm_group)] _group: &Group) {
    let (dir, log) = program_data_with_logs();
    let (old, new) = (process_user_sid(), sid_from_parts(5, &[32, 545]));

    let naming = Naming {
        made: vec![old.clone(), new.clone()],
        existing: Existing::Keep,
    };
    let outcome = super::ensure(dir.path(), &id(), &naming).unwrap();

    assert_eq!(outcome, EnsureOutcome::Made);
    assert_eq!(entries_of(&log), entries_wanted_for(&[old, new]));
}

/// A log whose owner is another account is Administrators' after the install, as the one the install made is: the
/// owner has the right to change the permissions of its file, which the account must never be left with.
#[skuld::test]
fn the_owner_of_a_log_that_is_there_is_set_to_administrators(#[fixture(scm_group)] _group: &Group) {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "kept\n").unwrap();
    let local_service = sid_from_parts(5, &[19]);
    set_owner(&log, &sid_display(&local_service));
    assert_eq!(
        read_path(&log).unwrap().owner,
        local_service,
        "the file is another account's"
    );

    ensure(dir.path(), &id(), &[local_service]).unwrap();

    assert_eq!(
        read_path(&log).unwrap().owner,
        crate::backend::scm::sid::administrators()
    );
    assert_eq!(fs::metadata(&log).unwrap().len(), 5, "what is in the log is kept");
}

/// What each account can do to a log, as its permissions grant it and not as they are spelt: the account the daemon
/// runs as appends, reads the attributes and waits on the handle, and nothing else; no other account, a group or
/// `OWNER RIGHTS` has any right; SYSTEM and Administrators have all.
#[skuld::test]
fn the_rights_each_account_has_on_a_log_are_the_ones_it_is_granted() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    let account = sid_from_parts(5, &[21, 1, 2, 3, 1001]);
    ensure(dir.path(), &id(), std::slice::from_ref(&account)).unwrap();
    let rights = |mut sid: Sid| effective_rights(&log, &mut sid);

    assert_eq!(rights(account), DEFAULT_LOG_ACCESS);
    for (what, sid) in [
        ("another account", sid_from_parts(5, &[21, 1, 2, 3, 1000])),
        ("Users", sid_from_parts(5, &[32, 545])),
        ("Everyone", sid_from_parts(1, &[0])),
        ("OWNER RIGHTS", sid_from_parts(3, &[4])),
    ] {
        assert_eq!(rights(sid), 0, "{what}");
    }
    for (what, sid) in [
        ("SYSTEM", sid_from_parts(5, &[18])),
        ("Administrators", sid_from_parts(5, &[32, 544])),
    ] {
        assert_eq!(rights(sid) & FILE_ALL_ACCESS, FILE_ALL_ACCESS, "{what}");
    }
}

/// An account named twice has one entry.
#[skuld::test]
fn an_account_named_twice_has_one_entry() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    let me = process_user_sid();

    ensure(dir.path(), &id(), &[me.clone(), me.clone()]).unwrap();

    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&me)));
}

/// A daemon that runs as SYSTEM is named in the log like any account, and the entry takes nothing from the
/// full control SYSTEM has on every log.
#[skuld::test]
fn a_system_account_keeps_its_full_control_of_the_log() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    let system = sid_from_parts(5, &[18]);

    ensure(dir.path(), &id(), std::slice::from_ref(&system)).unwrap();

    assert_eq!(
        effective_rights(&log, &mut system.clone()) & FILE_ALL_ACCESS,
        FILE_ALL_ACCESS
    );
}

/// What the install says when it made the log while the daemon ran: only then, only for a daemon whose output goes to
/// the default log (no `logs:`), and the words say what to do.
#[skuld::test]
fn a_log_made_while_the_daemon_runs_is_told_and_no_other_is() {
    let program_data = Path::new(r"C:\ProgramData");
    let told = |logs: Option<&Path>, outcome, running| made_while_running(program_data, &id(), logs, outcome, running);

    let said = told(None, EnsureOutcome::Made, true).expect("made while it runs");

    assert!(said.contains("nightly"), "{said}");
    assert!(said.contains(r"C:\ProgramData\Goetia\logs\nightly.log"), "{said}");
    assert!(said.contains("deleted log until it is restarted"), "{said}");
    assert!(said.contains("restart it"), "{said}");
    for (outcome, running) in [
        (EnsureOutcome::Made, false),
        (EnsureOutcome::Set, true),
        (EnsureOutcome::Kept, true),
    ] {
        assert_eq!(told(None, outcome, running), None, "{outcome:?}, running: {running}");
    }
    // The daemon's output goes to its own `logs:`, not to the file the install made.
    assert_eq!(
        told(Some(Path::new(r"D:\out.log")), EnsureOutcome::Made, true),
        None,
        "a daemon with `logs:` writes elsewhere"
    );
}

/// A run is up once its shim has reported past its hold on the log: a start in progress is not, since that shim
/// opens the log the install made, and a stopped service has no run.
#[skuld::test]
fn only_a_run_past_its_start_is_up() {
    use windows_service::service::ServiceState;

    assert_eq!(Run::of(ServiceState::Stopped), Run::Stopped);
    assert_eq!(Run::of(ServiceState::StartPending), Run::Starting);
    for up in [
        ServiceState::Running,
        ServiceState::StopPending,
        ServiceState::ContinuePending,
        ServiceState::PausePending,
        ServiceState::Paused,
    ] {
        assert_eq!(Run::of(up), Run::Up, "{up:?}");
    }
}

/// A directory at the log's name is a directory, which Windows refuses to open as a file with access denied, as it
/// does a delete that is pending: the install says what it is.
#[skuld::test]
fn a_directory_at_the_log_s_name_is_refused_as_a_directory() {
    let (dir, log) = program_data_with_logs();
    fs::create_dir(&log).unwrap();

    let refused = ensure(dir.path(), &id(), &[process_user_sid()])
        .unwrap_err()
        .report()
        .to_string();

    assert!(refused.contains(&log.display().to_string()), "{refused}");
    assert!(refused.contains("is a directory"), "{refused}");
    assert!(!refused.contains("being deleted"), "{refused}");
}

/// A link at the log's name that the install's open refuses with access denied, as it does a directory it cannot
/// open as a file, is a link: it is not followed to what it names, and the install says so.
#[skuld::test]
fn a_link_to_a_directory_at_the_log_s_name_is_refused_as_a_link() {
    let (dir, log) = program_data_with_logs();
    let target = dir.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&log).unwrap();
    make_mount_point(&log, &target);

    let refused = ensure(dir.path(), &id(), &[process_user_sid()])
        .unwrap_err()
        .report()
        .to_string();

    assert!(refused.contains(&log.display().to_string()), "{refused}");
    assert!(refused.contains("is a link"), "{refused}");
    assert!(!refused.contains("is a directory"), "{refused}");
}

/// A file whose permissions refuse the install even the attributes is no file with a delete pending: it is what
/// Windows says of it, for the open that was refused.
#[skuld::test]
fn a_log_whose_permissions_refuse_the_install_is_not_called_being_deleted() {
    let (dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    let me = process_user_sid();
    let only_change_permissions = Descriptor::from_sddl(&format!("D:P(A;;WD;;;{})", sid_display(&me))).unwrap();
    set_dacl(&only_change_permissions, &log).unwrap();

    let refused = ensure(dir.path(), &id(), &[me]).unwrap_err().report().to_string();

    assert!(refused.contains(&log.display().to_string()), "{refused}");
    assert!(!refused.contains("being deleted"), "{refused}");
}

/// What an install names in the log, in the file it makes and in the one there, by its step and by how far the
/// service's run is. A start in progress (`START_PENDING`) has a shim that may not yet hold the log it opens as the
/// account it was launched as; an up run holds the file it opened, but reopens by path, as its own account, one that
/// was made in its place.
#[skuld::test]
fn what_an_install_names_in_the_log_follows_its_step_and_its_run() {
    let (old, new) = (sid_from_parts(5, &[19]), sid_from_parts(5, &[20]));
    let kept = std::slice::from_ref(&old);
    let both = vec![old.clone(), new.clone()];
    let new_alone = Naming::alike(std::slice::from_ref(&new));

    // Before the change lands both accounts are named, whatever the run is doing.
    for run in [Run::Stopped, Run::Starting, Run::Up] {
        assert_eq!(naming(Step::Widen, run, kept, &new), Naming::alike(&both), "{run:?}");
    }
    // After it, the new account alone: it is the one every later start runs as.
    assert_eq!(naming(Step::Narrow, Run::Stopped, kept, &new), new_alone);
    // With a start in progress the file there is left, and one that has to be made names both.
    assert_eq!(
        naming(Step::Narrow, Run::Starting, kept, &new),
        Naming {
            made: both.clone(),
            existing: Existing::Keep
        }
    );
    // With a run up the file there is narrowed, as that run holds it; one that has to be made is the run's to
    // reopen as its own account, so it names both.
    assert_eq!(
        naming(Step::Narrow, Run::Up, kept, &new),
        Naming {
            made: both.clone(),
            existing: Existing::Set(vec![new.clone()])
        }
    );
    // What the install itself made under an up run stays as it is for the rest of that install.
    for run in [Run::Stopped, Run::Starting, Run::Up] {
        assert_eq!(
            naming(Step::Leave, run, kept, &new),
            Naming {
                made: both.clone(),
                existing: Existing::Keep
            },
            "{run:?}"
        );
    }
    // Where no account changes there is nothing else to name.
    assert_eq!(naming(Step::Narrow, Run::Up, &[], &new), new_alone);
    assert_eq!(
        naming(Step::Narrow, Run::Starting, &[], &new),
        Naming {
            made: vec![new],
            existing: Existing::Keep
        }
    );
}

/// A log the install has to make names what the naming says for a made file, and one that is there is set to what
/// it says for that: the same naming does both.
#[skuld::test]
fn a_log_that_is_made_and_one_that_is_there_are_named_by_their_own_part_of_the_naming(
    #[fixture(scm_group)] _group: &Group,
) {
    let (dir, log) = program_data_with_logs();
    let (old, new) = (process_user_sid(), sid_from_parts(5, &[32, 545]));
    let naming = Naming {
        made: vec![old.clone(), new.clone()],
        existing: Existing::Set(vec![new.clone()]),
    };

    let made = super::ensure(dir.path(), &id(), &naming).unwrap();

    assert_eq!(made, EnsureOutcome::Made);
    assert_eq!(entries_of(&log), entries_wanted_for(&[old, new.clone()]));

    let set = super::ensure(dir.path(), &id(), &naming).unwrap();

    assert_eq!(set, EnsureOutcome::Set);
    assert_eq!(entries_of(&log), entries_wanted_for(std::slice::from_ref(&new)));
}

/// A log whose delete is pending, because a run of the daemon still holds a handle on it, keeps its name until
/// that run ends, and Windows answers an open of it with access denied: the install says what it is, and the
/// way out, not only that access was denied.
#[skuld::test]
fn a_log_that_is_being_deleted_while_a_run_writes_it_is_refused_with_the_way_out() {
    let (dir, log) = program_data_with_logs();
    let _writer = delete_pending(&log);

    let refused = ensure(dir.path(), &id(), &[process_user_sid()])
        .unwrap_err()
        .report()
        .to_string();

    assert!(refused.contains(&log.display().to_string()), "{refused}");
    assert!(
        refused.contains("being deleted while a run of the daemon still writes it"),
        "{refused}"
    );
    assert!(refused.contains("stop the daemon, then install"), "{refused}");
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

/// The refusal reads as one sentence, with single spaces.
#[skuld::test]
fn the_refusal_says_what_it_refuses_and_the_way_out() {
    let (dir, log) = program_data_with_logs();
    let folder = log.parent().unwrap().to_path_buf();

    let said = refusal_of(&dir, &log).expect("refused");

    let expected = format!(
        "`logs: {}` is inside goetia's own log folder {}; remove `logs:` to use the default log, or name a file elsewhere",
        log.display(),
        folder.display()
    );
    assert!(said.contains(&expected), "{said}");
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

/// Only a part of the path that is not there is walked past: a name Windows refuses is no part that does not
/// exist, and the install fails naming the step, whether the name is in the logs folder or elsewhere.
#[skuld::test]
fn a_logs_with_a_name_windows_refuses_is_an_error_and_not_a_walk_to_the_folder_above() {
    let (dir, log) = program_data_with_logs();
    let folder = log.parent().unwrap().to_path_buf();

    for named in [dir.path().join("a*b"), folder.join("a*b")] {
        let refused = refuse_logs_in_folder(dir.path(), &id(), Some(&named))
            .expect_err(&format!("{} was accepted", named.display()))
            .report()
            .to_string();

        assert!(refused.contains("resolve"), "{refused}");
        assert!(refused.contains(&named.display().to_string()), "{refused}");
    }
}

/// The check needs no more of a folder than `canonicalize` does, which opens it for no access at all: a folder
/// the installer can pass through but may not read the permissions of (the owner's implicit right is withdrawn
/// by an entry for `OWNER RIGHTS`) is not a failure of the install, and `logs:` in it is accepted. The entries
/// grant what the temporary directory's removal needs. An elevated token holds `SeBackupPrivilege`, which opens
/// any folder opened with backup semantics whatever its permissions, so the thread runs as this token with its
/// privileges removed, as an unelevated account's is, and the test fails if that still reads the folder.
#[skuld::test]
fn a_logs_in_a_folder_whose_permissions_cannot_be_read_is_accepted() {
    let (dir, _log) = program_data_with_logs();
    let closed = dir.path().join("closed");
    fs::create_dir(&closed).unwrap();
    // List, traverse, delete, synchronize; and nothing else, not even the right to read the permissions.
    let rights = "0x110021";
    let me = sid_display(&process_user_sid());
    let descriptor = Descriptor::from_sddl(&format!("D:P(A;;{rights};;;{me})(A;;;;;OW)")).unwrap();
    set_dacl(&descriptor, &closed).unwrap();
    // As an unelevated user, whatever this process is: an elevated token opens any folder with backup semantics.
    let _unelevated = ThreadTokenReset::impersonate(&restricted_token_copy(
        DISABLE_MAX_PRIVILEGE,
        &mut [],
        TOKEN_QUERY | TOKEN_IMPERSONATE,
    ));
    read_path(&closed).expect_err("the premise: the permissions of the folder cannot be read");

    assert_eq!(refusal_of(&dir, &closed.join("x.log")), None);
}

/// `path`, a path on a local drive, as `\\localhost\<drive>$\...`: the administrative share of the same
/// folder, which the system lets an elevated account open. Its canonical form is a UNC path, which has no
/// text in common with the local one.
fn through_the_admin_share(path: &Path) -> PathBuf {
    let text = path.to_str().expect("a UTF-8 path");
    let (drive, rest) = text.split_once(r":\").expect("a path on a local drive");
    PathBuf::from(format!(r"\\localhost\{drive}$\{rest}"))
}

/// A folder is the same folder however it is reached. The administrative share of the drive spells the logs
/// folder in a form whose canonical path starts with another prefix; the check compares what the paths are
/// (the volume and the file index of the folder and of each folder above the part that exists), so the share's
/// spelling is in the folder too, and one outside it is not. The test needs the share, which an elevated
/// account has and CI's runner has, so it is in the `scm` group, and fails, never skips, without it.
#[skuld::test]
fn a_logs_spelled_through_the_administrative_share_is_still_in_the_logs_folder(#[fixture(scm_group)] _group: &Group) {
    let (dir, log) = program_data_with_logs();
    let folder = log.parent().unwrap().to_path_buf();
    let goetia = folder.parent().unwrap().to_path_buf();
    let share = through_the_admin_share(&folder);
    fs::metadata(&share).unwrap_or_else(|e| {
        panic!(
            "this test opens {} (the administrative share of the drive of the temporary directory), which \
             needs an elevated token and the share ({e}); the `scm` group is off with GOETIA_TEST_SCM=0",
            share.display()
        )
    });

    for exists in [false, true] {
        if exists {
            fs::write(&log, "").unwrap();
        }
        for named in [
            share.join("nightly.log"),
            share.join("sub").join("x.log"),
            share.clone(),
        ] {
            assert!(
                refusal_of(&dir, &named).is_some(),
                "exists={exists}: {} was accepted",
                named.display()
            );
        }
    }
    let elsewhere = through_the_admin_share(&goetia).join("mine.log");
    assert_eq!(refusal_of(&dir, &elsewhere), None, "{}", elsewhere.display());
}

// A deleted log -------------------------------------------------------------------------------------------------------

/// A log that the README's recovery deleted has lost the name the shim's handle was opened by: the handle is on
/// it still. One that was only moved has not.
#[skuld::test]
fn a_held_log_is_deleted_when_its_name_is_deleted_and_not_when_it_is_moved() {
    let (_dir, log) = program_data_with_logs();
    fs::write(&log, "").unwrap();
    let held = open_for_append(&log).unwrap();
    assert!(!is_deleted(&held).unwrap(), "a file with its name");

    let moved = log.with_file_name("moved.log");
    fs::rename(&log, &moved).unwrap();
    assert!(!is_deleted(&held).unwrap(), "a moved file has a name");

    fs::remove_file(&moved).unwrap();
    assert!(is_deleted(&held).unwrap(), "its name was deleted");
}
