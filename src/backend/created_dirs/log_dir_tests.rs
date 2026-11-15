use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

use super::*;
use crate::backend::created_dirs::CreatedDirs;
use crate::backend::created_dirs::created_dirs_tests::{Layout, STRANGER, give_to_a_stranger, stranger_owner};
use crate::testing::elevation::ELEVATED;

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The reason of a refusal.
fn refused(logs: &Path) -> String {
    match refuse_writable_log_dir(logs, &[]) {
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

    refuse_writable_log_dir(&dir.join("app.log"), &[]).unwrap();
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

    refuse_writable_log_dir(&dir.join("app.log"), &[]).unwrap();
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

#[skuld::test(labels = [ELEVATED])]
fn a_symlink_an_account_owns_on_the_way_to_the_log_directory_is_refused() {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    let link = layout.system().join("link");
    symlink(&real, &link).unwrap();
    std::os::unix::fs::lchown(&link, Some(STRANGER), Some(STRANGER)).unwrap();

    let why = refused(&link.join("app.log"));

    assert!(
        why.contains("symlink") && why.contains(&format!("uid {STRANGER}")),
        "{why}"
    );
}

#[skuld::test(labels = [ELEVATED])]
fn a_symlink_root_owns_to_a_safe_directory_is_followed() {
    let layout = Layout::new();
    let real = layout.system().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, layout.system().join("link")).unwrap();

    refuse_writable_log_dir(&layout.system().join("link/app.log"), &[]).unwrap();
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_that_does_not_exist_is_an_error_not_an_acceptance() {
    let layout = Layout::new();

    refuse_writable_log_dir(&layout.system().join("missing/app.log"), &[]).expect_err("nothing to check");
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

/// `/` is a directory root owns, and a path that ends in `..` is the directory above.
#[skuld::test(labels = [ELEVATED])]
fn a_log_in_the_root_directory_or_beside_a_dotdot_is_accepted() {
    let layout = Layout::new();
    fs::create_dir(layout.system().join("x")).unwrap();

    refuse_writable_log_dir(Path::new("/app.log"), &[]).unwrap();
    refuse_writable_log_dir(&layout.system().join("x/../app.log"), &[]).unwrap();
}

// What the install is about to hand over ------------------------------------------------------------------------------

/// The directory `cwd` is handed to an account after the check, so the check reads it as the
/// account's: a log in it, or under it, is refused, and says why.
#[skuld::test(labels = [ELEVATED])]
fn a_log_under_the_directory_about_to_be_handed_to_an_account_is_refused_before_the_hand_over() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut dirs = CreatedDirs::under(vec![layout.system()]);
    dirs.create(&cwd.join("out"), None).unwrap();
    dirs.create(&cwd, Some(stranger_owner())).unwrap();

    let at = |logs: &Path| match refuse_writable_log_dir(logs, &dirs.to_hand_over()) {
        Err(Error::DirectoryRefused { why, .. }) => why,
        other => panic!("expected a refusal of {}, got {other:?}", logs.display()),
    };
    for logs in [cwd.join("out/app.log"), cwd.join("app.log")] {
        let why = at(&logs);
        assert!(why.contains(cwd.to_str().unwrap()) && why.contains("hand"), "{why}");
        assert!(why.contains(&format!("uid {STRANGER}")), "{why}");
    }
    assert_eq!(fs::metadata(&cwd).unwrap().uid(), 0, "nothing was handed over");
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_beside_the_directory_about_to_be_handed_is_accepted() {
    let layout = Layout::new();
    let mut dirs = CreatedDirs::under(vec![layout.system()]);
    dirs.create(&layout.system().join("logs"), None).unwrap();
    dirs.create(&layout.system().join("work"), Some(stranger_owner()))
        .unwrap();

    refuse_writable_log_dir(&layout.system().join("logs/app.log"), &dirs.to_hand_over()).unwrap();
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

#[skuld::test(labels = [ELEVATED])]
fn the_hand_over_refusal_text_has_no_runs_of_spaces() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut dirs = CreatedDirs::under(vec![layout.system()]);
    dirs.create(&cwd, Some(stranger_owner())).unwrap();

    let why = match refuse_writable_log_dir(&cwd.join("app.log"), &dirs.to_hand_over()) {
        Err(Error::DirectoryRefused { why, .. }) => why,
        other => panic!("expected a refusal, got {other:?}"),
    };

    assert!(!why.contains("  "), "{why}");
}
