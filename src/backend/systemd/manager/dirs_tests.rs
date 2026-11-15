use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use super::*;
use crate::backend::created_dirs::created_dirs_tests::Layout;
use crate::error::Error;
use crate::spec::{AccountId, ServiceSpec};
use crate::testing::elevation::ELEVATED;

/// A name no passwd database has: an account a package or `sysusers` makes after the install.
const NO_ENTRY: &str = "goetia-no-such-user-xyz";

fn spec(user: User, cwd: Option<&Path>, logs: Option<&Path>) -> DaemonSpec {
    let mut fields = crate::testing::base_fields("dirs-test", vec!["/bin/true".to_string()]);
    fields.common.user = user;
    fields.common.cwd = cwd.map(Path::to_path_buf);
    fields.common.logs = logs.map(Path::to_path_buf);
    ServiceSpec::new(fields).expect("a valid service").into()
}

fn no_entry() -> User {
    User::Name(NO_ENTRY.to_string())
}

/// The account is looked up to hand a directory to it, and for nothing else: an account that does not
/// exist yet is no reason to refuse an install that hands it nothing.
#[skuld::test(labels = [ELEVATED])]
fn an_account_with_no_entry_is_not_looked_up_when_nothing_is_handed_to_it() {
    let layout = Layout::new();
    let existing = layout.system().join("work");
    fs::create_dir(&existing).unwrap();
    let logs = layout.system().join("logs/out.log");

    for (cwd, logs) in [
        (None, None),
        (Some(existing.as_path()), None),
        (None, Some(logs.as_path())),
        (Some(existing.as_path()), Some(logs.as_path())),
    ] {
        let mut created = CreatedDirs::under(vec![layout.system()]);
        ensure_parent_dirs(&spec(no_entry(), cwd, logs), &mut created)
            .unwrap_or_else(|e| panic!("cwd {cwd:?}, logs {logs:?}: {e}"));
    }
}

#[skuld::test(labels = [ELEVATED])]
fn an_account_with_no_entry_refuses_the_install_before_it_makes_the_cwd_it_would_hand_over() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut created = CreatedDirs::under(vec![layout.system()]);

    let err = ensure_parent_dirs(&spec(no_entry(), Some(&cwd), None), &mut created).expect_err("no such user");

    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(!cwd.exists(), "the directory is made only for an account that is found");
}

/// A uid with no entry is the number, as it is to systemd: the directory is made and the account is
/// not looked up further.
#[skuld::test(labels = [ELEVATED])]
fn a_uid_with_no_entry_is_handed_the_cwd_as_a_number() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut created = CreatedDirs::under(vec![layout.system()]);

    ensure_parent_dirs(
        &spec(User::Id(AccountId::Uid(4_242_424)), Some(&cwd), None),
        &mut created,
    )
    .unwrap();

    assert_eq!(fs::metadata(&cwd).unwrap().uid(), 4_242_424);
}

/// uid 0 is root whichever way the spec spells it: nothing is handed over, so a log in the `cwd` is
/// the root-owned directory it stays, not one "that will belong to root".
#[skuld::test(labels = [ELEVATED])]
fn uid_zero_is_root_and_is_handed_nothing() {
    for user in [User::Root, User::Id(AccountId::Uid(0)), User::Name("root".to_string())] {
        let layout = Layout::new();
        let cwd = layout.system().join("work");
        let logs = cwd.join("out.log");
        let mut created = CreatedDirs::under(vec![layout.system()]);

        ensure_parent_dirs(&spec(user.clone(), Some(&cwd), Some(&logs)), &mut created)
            .unwrap_or_else(|e| panic!("{user:?}: {e}"));

        assert_eq!(fs::metadata(&cwd).unwrap().uid(), 0, "{user:?}");
    }
}
