use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use super::*;
use crate::backend::Settle as _;
use crate::backend::Standing;
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
/// not looked up further. It is the account's once the unit stands, and root's until then.
#[skuld::test(labels = [ELEVATED])]
fn a_uid_with_no_entry_is_handed_the_cwd_as_a_number_once_the_unit_stands() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut created = CreatedDirs::under(vec![layout.system()]);

    ensure_parent_dirs(
        &spec(User::Id(AccountId::Uid(4_242_424)), Some(&cwd), None),
        &mut created,
    )
    .unwrap();
    assert_eq!(fs::metadata(&cwd).unwrap().uid(), 0, "root's until the unit stands");
    created.settle(|| Standing::Yes);

    assert_eq!(fs::metadata(&cwd).unwrap().uid(), 4_242_424);
}

/// A write that fails after the directories are made leaves none an account could have written into:
/// they are root's when the guard takes them back.
#[skuld::test(labels = [ELEVATED])]
fn a_failed_install_never_hands_the_cwd_to_the_account() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut created = CreatedDirs::under(vec![layout.system()]);

    ensure_parent_dirs(
        &spec(User::Id(AccountId::Uid(4_242_424)), Some(&cwd), None),
        &mut created,
    )
    .unwrap();
    assert_eq!(
        fs::metadata(&cwd).unwrap().uid(),
        0,
        "the write that follows has not stood yet: the account owns nothing"
    );
    created.settle(|| Standing::No);
    drop(created);

    assert!(!cwd.exists(), "taken back while still root's");
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

/// A log the check refused under a `cwd` this install made is never handed over: a later `settle` that
/// keeps the directories (an unknown standing) leaves the `cwd` root's, not the account's.
#[skuld::test(labels = [ELEVATED])]
fn a_log_the_check_refused_under_a_new_cwd_leaves_the_cwd_root_s_whatever_settles_it() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let logs = cwd.join("out.log");
    let mut created = CreatedDirs::under(vec![layout.system()]);

    let err = ensure_parent_dirs(
        &spec(User::Id(AccountId::Uid(4_242_424)), Some(&cwd), Some(&logs)),
        &mut created,
    )
    .expect_err("a log in the directory that is to be the account's");
    assert!(matches!(err, Error::DirectoryRefused { .. }), "{err:?}");
    created.settle(|| Standing::Unknown("could not read the unit".to_string()));

    assert_eq!(
        fs::metadata(&cwd).unwrap().uid(),
        0,
        "the refused directory was handed over"
    );
}
