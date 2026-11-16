use std::fs;
use std::os::unix::fs::{PermissionsExt as _, lchown, symlink};
use std::path::PathBuf;

use super::*;
use crate::backend::created_dirs::CreatedDirs;
use crate::backend::created_dirs::created_dirs_tests::{Fsuid, Layout, STRANGER, give_to_a_stranger};
use crate::testing::elevation::ELEVATED;
use crate::testing::groups::root_dirs;

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The check on what exists, planned with no root to make anything under.
fn check(logs: &Path) -> Result<()> {
    checked(&CreatedDirs::under(Vec::new()), logs)
}

fn checked(dirs: &CreatedDirs, logs: &Path) -> Result<()> {
    let plan = dirs.plan(logs.parent().unwrap())?;
    refuse_writable_log_dir(logs, &plan, "app", dirs)
}

/// The reason of a refusal, with no root to make anything under.
fn refused(logs: &Path) -> String {
    refused_by(&CreatedDirs::under(Vec::new()), logs)
}

fn refused_by(dirs: &CreatedDirs, logs: &Path) -> String {
    match checked(dirs, logs) {
        Err(Error::DirectoryRefused { why, .. }) => why,
        other => panic!("expected a refusal of {}, got {other:?}", logs.display()),
    }
}

/// Give `path` an access ACL that names the stranger, as `setfacl -m u:4242:rwx` leaves it: the group
/// bits of the mode then read as the ACL's mask.
fn name_the_stranger_in_the_acl(path: &Path) {
    const NO_ID: u32 = u32::MAX;
    let mut acl = 2u32.to_le_bytes().to_vec(); // POSIX_ACL_XATTR_VERSION
    for (tag, perm, id) in [
        (0x01u16, 7u16, NO_ID), // ACL_USER_OBJ
        (0x02, 7, STRANGER),    // ACL_USER
        (0x04, 5, NO_ID),       // ACL_GROUP_OBJ
        (0x10, 7, NO_ID),       // ACL_MASK
        (0x20, 5, NO_ID),       // ACL_OTHER
    ] {
        acl.extend([tag.to_le_bytes().as_slice(), &perm.to_le_bytes(), &id.to_le_bytes()].concat());
    }
    let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: both strings are NUL-terminated and `acl` is the length it is given with, all for this call.
    let set = unsafe {
        libc::setxattr(
            path_c.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            acl.as_ptr().cast(),
            acl.len(),
            0,
        )
    };
    assert_eq!(
        set,
        0,
        "set a POSIX ACL on {}: {} (the file system under /tmp needs ACLs)",
        path.display(),
        std::io::Error::last_os_error()
    );
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_only_root_can_change_is_accepted(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o755);
    chmod(&layout.system(), 0o755);

    check(&dir.join("app.log")).unwrap();
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_an_account_owns_is_refused_naming_it(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();
    give_to_a_stranger(&dir);

    let why = refused(&dir.join("app.log"));

    assert!(why.contains(&format!("uid {STRANGER}")), "{why}");
    assert!(why.contains(dir.to_str().unwrap()), "{why}");
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_a_group_or_everyone_can_write_is_refused(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();

    chmod(&dir, 0o775);
    assert!(refused(&dir.join("app.log")).contains("group"));
    chmod(&dir, 0o757);
    let why = refused(&dir.join("app.log"));
    assert!(why.contains("every user"), "{why}");
    assert!(
        !why.contains("such as"),
        "a directory of its own would be refused too: {why}"
    );
}

/// `/tmp` is writable by everyone, and only the owner of an entry can rename it: that is enough for an
/// ancestor, which is only passed through.
#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_everyone_can_write_is_accepted_when_it_is_sticky(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let ancestor = layout.system().join("tmp");
    let dir = ancestor.join("logs");
    fs::create_dir_all(&dir).unwrap();
    chmod(&ancestor, 0o1777);

    check(&dir.join("app.log")).unwrap();
}

/// The sticky bit keeps others from renaming what is there, not from making an entry: whoever can
/// write the log's own directory can plant a link at the log's name before the first start.
#[skuld::test(labels = [ELEVATED])]
fn the_log_directory_itself_is_refused_when_everyone_can_write_it_though_it_is_sticky(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let dir = layout.system().join("tmp");
    fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o1777);

    assert!(refused(&dir.join("app.log")).contains("every user"));

    chmod(&dir, 0o1775);
    assert!(refused(&dir.join("app.log")).contains("group"));
}

/// Debian's and Ubuntu's `/var/log` is `root:syslog 0775`. The group of a directory root owns is trusted
/// as an ancestor, as systemd's own `LogsDirectory=` trusts it, when the log's directory is root's.
#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_a_group_can_write_is_accepted_when_the_log_directory_is_roots(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let var_log = layout.system().join("var/log");
    let dir = var_log.join("app");
    fs::create_dir_all(&dir).unwrap();
    chmod(&var_log, 0o775);

    check(&dir.join("app.log")).unwrap();

    // A directory that is to be made is root's, and so is as safe.
    let dirs = CreatedDirs::under(vec![layout.system()]);
    checked(&dirs, &var_log.join("new/app.log")).unwrap();
    assert!(!var_log.join("new").exists(), "the check makes nothing");
}

/// The log's own directory keeps the strict rule: a log directly in a directory a group can write is
/// refused, whatever its ancestors are.
#[skuld::test(labels = [ELEVATED])]
fn a_log_directly_in_a_directory_a_group_can_write_is_refused_though_its_ancestors_may_be_group_writable(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let var = layout.system().join("var");
    let var_log = var.join("log");
    fs::create_dir_all(&var_log).unwrap();
    chmod(&var_log, 0o775);
    let under_a_root = CreatedDirs::under(vec![layout.system()]);

    let why = refused_by(&under_a_root, &var_log.join("app.log"));
    assert!(
        why.contains("group") && why.contains(var_log.to_str().unwrap()),
        "{why}"
    );
    assert!(
        why.contains(var_log.join("app/app.log").to_str().unwrap()),
        "suggests one: {why}"
    );

    chmod(&var, 0o775);
    let why = refused_by(&under_a_root, &var_log.join("app.log"));
    assert!(
        why.contains(var_log.to_str().unwrap()),
        "names the log's directory: {why}"
    );
}

/// A directory made for the log is made only under a root: a log directory that is not under one is
/// not told to make one, whatever its own mode says, and is told what does work.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_of_its_own_is_suggested_only_under_a_root(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let dir = layout.system().join("opt/logs");
    fs::create_dir_all(&dir).unwrap();
    chmod(&dir, 0o775);

    let why = refused(&dir.join("app.log"));

    assert!(!why.contains("such as"), "no root to make it under: {why}");
    assert!(why.contains("journal"), "{why}");
    assert!(why.contains("under"), "{why}");
}

/// A root below a regular file is skipped, as `plan` skips it: a log directory under a remaining root is
/// judged under it, and one outside every root is not told to make a directory of its own.
#[skuld::test(labels = [ELEVATED])]
fn a_skipped_root_leaves_the_log_refusal_to_the_roots_that_remain(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let broken = layout.broken_root();
    let logs_in = |parent: PathBuf| {
        let dir = parent.join("opt/logs");
        fs::create_dir_all(&dir).unwrap();
        chmod(&dir, 0o775);
        dir.join("app.log")
    };
    let held = logs_in(layout.system());
    let not_held = logs_in(layout.outside());
    let roots = || CreatedDirs::under(vec![broken.clone(), layout.system()]);

    let why = refused_by(&roots(), &held);
    assert!(
        why.contains(held.parent().unwrap().join("app/app.log").to_str().unwrap()),
        "{why}"
    );

    let why = refused_by(&roots(), &not_held);
    assert!(!why.contains("such as") && why.contains("journal"), "{why}");
    assert!(
        why.contains(layout.system().to_str().unwrap()) && !why.contains(broken.to_str().unwrap()),
        "lists the roots that exist: {why}"
    );
}

/// A root that cannot be read does not hide the refusal: the log is refused with the advice that does
/// not depend on the roots. The roots listed are those that could be read, and the one that could not
/// is said so in a clause of its own, with the OS error.
#[cfg(target_os = "linux")]
#[skuld::test(labels = [ELEVATED])]
fn a_root_that_cannot_be_read_leaves_the_log_refused_with_the_generic_advice_and_is_named_apart(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let unreadable = layout.unreadable_root();
    let dir = layout.outside().join("opt/logs");
    fs::create_dir_all(&dir).unwrap();
    chmod(&dir, 0o775);
    let _as_a_stranger = Fsuid::set(STRANGER);

    let why = refused_by(
        &CreatedDirs::under(vec![unreadable.clone(), layout.system()]),
        &dir.join("app.log"),
    );

    assert!(!why.contains("such as") && why.contains("journal"), "{why}");
    assert!(
        why.contains(&format!(
            "under {}; {} could not be read: Permission denied (os error 13)",
            layout.system().display(),
            unreadable.display()
        )),
        "{why}"
    );
    assert_eq!(why.matches(unreadable.to_str().unwrap()).count(), 1, "{why}");
}

/// Trusting the group of a root-owned ancestor does not trust everyone, or an account.
#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_everyone_can_write_without_the_sticky_bit_or_an_account_owns_is_refused_though_a_group_can_write_it(
    #[fixture(root_dirs)] _root: &(),
) {
    for mode in [0o757, 0o777, 0o772] {
        let layout = Layout::new();
        let ancestor = layout.system().join("log");
        let dir = ancestor.join("app");
        fs::create_dir_all(&dir).unwrap();
        chmod(&ancestor, mode);

        let why = refused(&dir.join("app.log"));
        assert!(
            why.contains("every user") && why.contains(ancestor.to_str().unwrap()),
            "{mode:o}: {why}"
        );
    }

    let layout = Layout::new();
    let ancestor = layout.system().join("log");
    let dir = ancestor.join("app");
    fs::create_dir_all(&dir).unwrap();
    give_to_a_stranger(&ancestor);
    chmod(&ancestor, 0o775);

    let why = refused(&dir.join("app.log"));
    assert!(
        why.contains(&format!("uid {STRANGER}")) && why.contains(ancestor.to_str().unwrap()),
        "{why}"
    );

    // A directory made in the log's own account-owned directory would be refused as well: no suggestion.
    let why = refused(&ancestor.join("app.log"));
    assert!(!why.contains("such as"), "{why}");
}

/// With a POSIX ACL the group bits of the mode are the ACL's mask, so a group-write bit says an account
/// the ACL names can write the directory, not that its group can: the group is trusted, not the
/// entries, and such a directory is judged as one others can write.
#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_whose_acl_names_an_account_is_refused_though_its_group_bits_allow_writing(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let var_log = layout.system().join("var/log");
    let dir = var_log.join("app");
    fs::create_dir_all(&dir).unwrap();
    chmod(&var_log, 0o775);
    name_the_stranger_in_the_acl(&var_log);
    assert_eq!(
        fs::metadata(&var_log).unwrap().permissions().mode() & 0o070,
        0o070,
        "the mask is what the group bits read"
    );

    let why = refused(&dir.join("app.log"));

    assert!(why.contains(var_log.to_str().unwrap()) && why.contains("ACL"), "{why}");

    // As for an ancestor others can write, the sticky bit excuses it.
    chmod(&var_log, 0o1775);
    check(&dir.join("app.log")).unwrap();
}

/// The log's own directory is refused for a group that can write it, and an ACL says whom it is.
#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_whose_acl_names_an_account_is_refused_with_no_directory_of_its_own_suggested(
    #[fixture(root_dirs)] _root: &(),
) {
    let layout = Layout::new();
    let dir = layout.system().join("var/log");
    fs::create_dir_all(&dir).unwrap();
    chmod(&dir, 0o775);
    name_the_stranger_in_the_acl(&dir);
    let under_a_root = CreatedDirs::under(vec![layout.system()]);

    let why = refused_by(&under_a_root, &dir.join("app.log"));

    assert!(why.contains("ACL"), "{why}");
    assert!(
        !why.contains("such as"),
        "a directory made inside would be refused as an ancestor too: {why}"
    );
}

/// Whether an ancestor's group is trusted is the one thing the ACL decides, so it is read there. A
/// directory whose ACL cannot be read (here, one this account may search and not open) is a log refused
/// for that reason, never trusted or distrusted on a guess.
#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_whose_acl_cannot_be_read_refuses_the_log_saying_so(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    chmod(&layout.base(), 0o711);
    chmod(&layout.system(), 0o755);
    let ancestor = layout.system().join("shared");
    let dir = ancestor.join("logs");
    fs::create_dir_all(&dir).unwrap();
    chmod(&dir, 0o755);
    chmod(&ancestor, 0o731);
    let _as_a_stranger = Fsuid::set(STRANGER);

    let why = refused(&dir.join("app.log"));

    assert!(
        why.contains(ancestor.to_str().unwrap()) && why.contains("ACL") && why.contains("Permission denied"),
        "{why}"
    );
}

#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_an_account_can_change_is_refused_though_the_directory_is_roots(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let ancestor = layout.system().join("shared");
    let dir = ancestor.join("logs");
    fs::create_dir_all(&dir).unwrap();
    chmod(&ancestor, 0o777);

    let why = refused(&dir.join("app.log"));

    assert!(why.contains(ancestor.to_str().unwrap()), "{why}");
}

/// A symlink's owner cannot change where it leads: a link an account made in a directory only root can
/// write is followed.
#[skuld::test(labels = [ELEVATED])]
fn a_symlink_an_account_owns_in_a_directory_only_root_can_write_is_followed(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    let link = layout.system().join("link");
    symlink(&real, &link).unwrap();
    lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();

    check(&link.join("app.log")).unwrap();
}

/// A link is judged by its holder: whoever can write the directory it is in can replace it.
#[skuld::test(labels = [ELEVATED])]
fn a_symlink_in_a_directory_an_account_can_write_is_refused_naming_the_directory(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    let holder = layout.system().join("holder");
    fs::create_dir(&holder).unwrap();
    symlink(&real, holder.join("link")).unwrap();
    give_to_a_stranger(&holder);

    let why = refused(&holder.join("link/app.log"));

    assert!(
        why.contains(holder.to_str().unwrap()) && why.contains(&format!("uid {STRANGER}")),
        "{why}"
    );
}

/// In a sticky directory a group or everyone can write (`/tmp`), the owner of an entry can delete it and
/// make another: a link an account owns there is refused though the directory is an ancestor the sticky
/// bit excuses. A link root owns there is followed, and so is an account's where only root can write.
#[skuld::test(labels = [ELEVATED])]
fn a_symlink_an_account_owns_in_a_sticky_directory_others_can_write_is_refused(#[fixture(root_dirs)] _root: &()) {
    for writable in [0o1777, 0o1775] {
        let layout = Layout::new();
        let real = layout.system().join("real");
        fs::create_dir(&real).unwrap();
        let sticky = layout.system().join("tmp");
        fs::create_dir(&sticky).unwrap();
        chmod(&sticky, writable);
        let link = sticky.join("link");
        symlink(&real, &link).unwrap();
        lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();

        let why = refused(&link.join("app.log"));
        assert!(
            why.contains(link.to_str().unwrap()) && why.contains(&format!("uid {STRANGER}")),
            "{writable:o}: {why}"
        );

        chmod(&sticky, 0o1755);
        check(&link.join("app.log")).unwrap_or_else(|e| panic!("only root can write it: {e}"));
        chmod(&sticky, writable);
        lchown(&link, Some(0), Some(0)).unwrap();
        check(&link.join("app.log")).unwrap_or_else(|e| panic!("root's link: {e}"));
    }
}

#[skuld::test(labels = [ELEVATED])]
fn a_symlink_root_owns_to_a_safe_directory_is_followed(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, layout.system().join("link")).unwrap();

    check(&layout.system().join("link/app.log")).unwrap();
}

/// A directory that is to be made is root's: it is judged by the one it is made in.
#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_that_is_to_be_made_is_judged_by_the_directory_it_is_made_in(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let dirs = CreatedDirs::under(vec![layout.system()]);
    let logs = layout.system().join("new/deeper/app.log");

    checked(&dirs, &logs).unwrap();

    chmod(&layout.system(), 0o777);
    let Err(Error::DirectoryRefused { why, .. }) = checked(&dirs, &logs) else {
        panic!("a directory everyone can write is refused")
    };
    assert!(why.contains(layout.system().to_str().unwrap()), "{why}");
    assert!(!layout.system().join("new").exists(), "the check makes nothing");
}

// The last component --------------------------------------------------------------------------------------------------

/// A link as the last component of the directory is walked like one in the middle: the directories
/// its target is under are on the trail too.
#[skuld::test(labels = [ELEVATED])]
fn a_link_as_the_log_directory_is_judged_by_every_directory_its_target_is_under(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let owned = layout.system().join("owned");
    fs::create_dir_all(owned.join("sub")).unwrap();
    give_to_a_stranger(&owned);
    symlink(owned.join("sub"), layout.system().join("link")).unwrap();

    let why = refused(&layout.system().join("link/app.log"));

    assert!(
        why.contains(owned.to_str().unwrap()) && why.contains(&format!("uid {STRANGER}")),
        "{why}"
    );
}

/// A link to `.` as the log's directory leaves the walk in the sticky directory the link is in: that
/// directory is where the log lands, so it is judged as the log's own, not as an ancestor.
#[skuld::test(labels = [ELEVATED])]
fn a_link_to_dot_makes_its_directory_the_log_directory_whatever_step_reached_it(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let sticky = layout.system().join("tmp");
    fs::create_dir(&sticky).unwrap();
    chmod(&sticky, 0o1777);
    symlink(".", sticky.join("x")).unwrap();

    let why = refused(&sticky.join("x/out.log"));

    assert!(
        why.contains("every user") && why.contains(sticky.to_str().unwrap()),
        "{why}"
    );
}

/// `/` is a directory root owns, and a path that ends in `..` is the directory above.
#[skuld::test(labels = [ELEVATED])]
fn a_log_in_the_root_directory_or_beside_a_dotdot_is_accepted(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("x")).unwrap();

    check(Path::new("/app.log")).unwrap();
    check(&layout.system().join("x/../app.log")).unwrap();
}

// The log itself ------------------------------------------------------------------------------------------------------

#[skuld::test(labels = [ELEVATED])]
fn a_log_that_is_absent_or_a_file_root_owns_is_accepted(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let logs = layout.system().join("app.log");

    check(&logs).unwrap();

    fs::write(&logs, b"x").unwrap();
    check(&logs).unwrap();
}

/// Root's open follows a link at the log's name to wherever it leads.
#[skuld::test(labels = [ELEVATED])]
fn a_log_that_is_a_symlink_is_refused_whoever_owns_it_and_wherever_it_leads(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let logs = layout.system().join("app.log");

    symlink(layout.outside().join("target"), &logs).unwrap();
    let why = refused(&logs);
    assert!(why.contains("symlink") && why.contains(logs.to_str().unwrap()), "{why}");

    fs::remove_file(&logs).unwrap();
    fs::write(layout.outside().join("target"), b"x").unwrap();
    symlink(layout.outside().join("target"), &logs).unwrap();
    assert!(refused(&logs).contains("symlink"), "a link to a file that exists");
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_an_account_owns_is_refused_naming_the_account(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let logs = layout.system().join("app.log");
    fs::write(&logs, b"x").unwrap();
    give_to_a_stranger(&logs);

    let why = refused(&logs);

    assert!(
        why.contains(logs.to_str().unwrap()) && why.contains(&format!("uid {STRANGER}")),
        "{why}"
    );
}

#[skuld::test(labels = [ELEVATED])]
fn the_refusal_text_has_no_runs_of_spaces(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o777);

    let why = refused(&dir.join("app.log"));

    assert!(!why.contains("  "), "{why}");
}

/// The service manager opens the log by the path as written, so a directory the path leaves through
/// `..` decides where the log lands: the account that owns it can replace a subdirectory with a symlink.
#[skuld::test(labels = [ELEVATED])]
fn a_log_beside_a_dotdot_that_leaves_an_account_directory_is_refused(#[fixture(root_dirs)] _root: &()) {
    let layout = Layout::new();
    let account = layout.system().join("u");
    fs::create_dir(&account).unwrap();
    give_to_a_stranger(&account);

    let why = refused(&account.join("../app.log"));

    assert!(why.contains(account.to_str().unwrap()), "{why}");
}
