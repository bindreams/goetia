use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;
use crate::backend::created_dirs::CreatedDirs;
use crate::backend::created_dirs::created_dirs_tests::{Layout, STRANGER, give_to_a_stranger};
use crate::testing::elevation::ELEVATED;

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The check on what exists, planned with no root to make anything under.
fn check(logs: &Path) -> Result<()> {
    checked(&CreatedDirs::under(Vec::new()), logs)
}

fn checked(dirs: &CreatedDirs, logs: &Path) -> Result<()> {
    let plan = dirs.plan(logs.parent().unwrap())?;
    refuse_writable_log_dir(logs, &plan)
}

/// The reason of a refusal.
fn refused(logs: &Path) -> String {
    match check(logs) {
        Err(Error::DirectoryRefused { why, .. }) => why,
        other => panic!("expected a refusal of {}, got {other:?}", logs.display()),
    }
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_only_root_can_change_is_accepted() {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o755);
    chmod(&layout.system(), 0o755);

    check(&dir.join("app.log")).unwrap();
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_an_account_owns_is_refused_naming_it() {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();
    give_to_a_stranger(&dir);

    let why = refused(&dir.join("app.log"));

    assert!(why.contains(&format!("uid {STRANGER}")), "{why}");
    assert!(why.contains(dir.to_str().unwrap()), "{why}");
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_a_group_or_everyone_can_write_is_refused() {
    let layout = Layout::new();
    let dir = layout.system().join("logs");
    fs::create_dir(&dir).unwrap();

    chmod(&dir, 0o775);
    assert!(refused(&dir.join("app.log")).contains("group"));
    chmod(&dir, 0o757);
    assert!(refused(&dir.join("app.log")).contains("every user"));
}

/// `/tmp` is writable by everyone, and only the owner of an entry can rename it: that is enough for an
/// ancestor, which is only passed through.
#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_everyone_can_write_is_accepted_when_it_is_sticky() {
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
fn the_log_directory_itself_is_refused_when_everyone_can_write_it_though_it_is_sticky() {
    let layout = Layout::new();
    let dir = layout.system().join("tmp");
    fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o1777);

    assert!(refused(&dir.join("app.log")).contains("every user"));

    chmod(&dir, 0o1775);
    assert!(refused(&dir.join("app.log")).contains("group"));
}

#[skuld::test(labels = [ELEVATED])]
fn an_ancestor_an_account_can_change_is_refused_though_the_directory_is_roots() {
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
fn a_symlink_an_account_owns_in_a_directory_only_root_can_write_is_followed() {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    let link = layout.system().join("link");
    symlink(&real, &link).unwrap();
    std::os::unix::fs::lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();

    check(&link.join("app.log")).unwrap();
}

/// A link is judged by its holder: whoever can write the directory it is in can replace it.
#[skuld::test(labels = [ELEVATED])]
fn a_symlink_in_a_directory_an_account_can_write_is_refused_naming_the_directory() {
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

#[skuld::test(labels = [ELEVATED])]
fn a_symlink_root_owns_to_a_safe_directory_is_followed() {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, layout.system().join("link")).unwrap();

    check(&layout.system().join("link/app.log")).unwrap();
}

/// A directory that is to be made is root's: it is judged by the one it is made in.
#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_that_is_to_be_made_is_judged_by_the_directory_it_is_made_in() {
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
fn a_link_as_the_log_directory_is_judged_by_every_directory_its_target_is_under() {
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
fn a_link_to_dot_makes_its_directory_the_log_directory_whatever_step_reached_it() {
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
fn a_log_in_the_root_directory_or_beside_a_dotdot_is_accepted() {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("x")).unwrap();

    check(Path::new("/app.log")).unwrap();
    check(&layout.system().join("x/../app.log")).unwrap();
}

// The log itself ------------------------------------------------------------------------------------------------------

#[skuld::test(labels = [ELEVATED])]
fn a_log_that_is_absent_or_a_file_root_owns_is_accepted() {
    let layout = Layout::new();
    let logs = layout.system().join("app.log");

    check(&logs).unwrap();

    fs::write(&logs, b"x").unwrap();
    check(&logs).unwrap();
}

/// Root's open follows a link at the log's name to wherever it leads.
#[skuld::test(labels = [ELEVATED])]
fn a_log_that_is_a_symlink_is_refused_whoever_owns_it_and_wherever_it_leads() {
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
fn a_log_an_account_owns_is_refused_naming_the_account() {
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
fn the_refusal_text_has_no_runs_of_spaces() {
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
fn a_log_beside_a_dotdot_that_leaves_an_account_directory_is_refused() {
    let layout = Layout::new();
    let account = layout.system().join("u");
    fs::create_dir(&account).unwrap();
    give_to_a_stranger(&account);

    let why = refused(&account.join("../app.log"));

    assert!(why.contains(account.to_str().unwrap()), "{why}");
}
