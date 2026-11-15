use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;
use crate::backend::created_dirs::created_dirs_tests::{Layout, STRANGER, give_to_a_stranger};
use crate::testing::elevation::ELEVATED;

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The reason of a refusal.
fn refused(logs: &Path) -> String {
    match refuse_writable_log_dir(logs) {
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

    refuse_writable_log_dir(&dir.join("app.log")).unwrap();
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

/// `/tmp` is writable by everyone, and only the owner of an entry can rename it.
#[skuld::test(labels = [ELEVATED])]
fn a_directory_everyone_can_write_is_accepted_when_it_is_sticky() {
    let layout = Layout::new();
    let dir = layout.system().join("tmp");
    fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o1777);

    refuse_writable_log_dir(&dir.join("app.log")).unwrap();
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

    refuse_writable_log_dir(&layout.system().join("link/app.log")).unwrap();
}

#[skuld::test(labels = [ELEVATED])]
fn a_log_directory_that_does_not_exist_is_an_error_not_an_acceptance() {
    let layout = Layout::new();

    refuse_writable_log_dir(&layout.system().join("missing/app.log")).expect_err("nothing to check");
}
