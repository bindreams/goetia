use std::fs;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::Path;

use super::*;
use crate::backend::Settle as _;
use crate::backend::Standing;
use crate::backend::created_dirs::created_dirs_tests::{Layout, STRANGER, give_to_a_stranger};
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
/// not looked up further. It is the account's when this returns: the unit can run as soon as the write
/// that follows starts.
#[skuld::test(labels = [ELEVATED])]
fn a_uid_with_no_entry_is_handed_the_cwd_as_a_number_before_the_write_starts() {
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

/// A write that fails after the directories are made takes them back, handed over as they were: the
/// account has not been able to write in one, since the unit it is for never stood.
#[skuld::test(labels = [ELEVATED])]
fn a_failed_install_takes_back_the_cwd_it_handed_over() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    let mut created = CreatedDirs::under(vec![layout.system()]);

    ensure_parent_dirs(
        &spec(User::Id(AccountId::Uid(4_242_424)), Some(&cwd), None),
        &mut created,
    )
    .unwrap();
    created.settle(|| Standing::No);
    drop(created);

    assert!(!cwd.exists());
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

/// A log the check refuses leaves nothing made, whatever the refusal is for, and whether the directory
/// it is in is to be made or not: no directory is made until every path is judged.
#[skuld::test(labels = [ELEVATED])]
fn nothing_is_made_when_the_log_check_refuses() {
    let layout = Layout::new();
    let owned = layout.system().join("owned");
    fs::create_dir(&owned).unwrap();
    give_to_a_stranger(&owned);
    let planted = layout.system().join("planted");
    fs::create_dir(&planted).unwrap();
    symlink(layout.outside(), planted.join("out.log")).unwrap();
    let user = || User::Id(AccountId::Uid(4_242_424));
    let work = layout.system().join("work");
    let new_logs = layout.system().join("newlogs");

    for (name, logs) in [
        ("a log directory an account owns", owned.join("out.log")),
        ("a log that is a symlink", planted.join("out.log")),
    ] {
        let mut created = CreatedDirs::under(vec![layout.system()]);

        let err = ensure_parent_dirs(&spec(user(), Some(&work), Some(&logs)), &mut created).expect_err(name);

        assert!(matches!(err, Error::DirectoryRefused { .. }), "{name}: {err:?}");
        assert!(!work.exists(), "{name}: the cwd was made");
    }
    // The log's own directory, to be made, goes with it.
    let mut created = CreatedDirs::under(vec![layout.system()]);
    let logs = new_logs.join("out.log");
    let err = ensure_parent_dirs(&spec(user(), Some(&owned.join("w")), Some(&logs)), &mut created)
        .expect_err("a cwd under a directory an account owns is refused as the plan of it");
    assert!(matches!(err, Error::DirectoryRefused { .. }), "{err:?}");
    assert!(!new_logs.exists(), "the log directory was made");
}

/// A log the check refuses is refused whatever the account is: it comes before the account is looked up.
#[skuld::test(labels = [ELEVATED])]
fn a_log_the_check_refuses_comes_before_the_account_is_looked_up() {
    let layout = Layout::new();
    let owned = layout.system().join("owned");
    fs::create_dir(&owned).unwrap();
    give_to_a_stranger(&owned);
    let mut created = CreatedDirs::under(vec![layout.system()]);

    let err = ensure_parent_dirs(
        &spec(
            no_entry(),
            Some(&layout.system().join("work")),
            Some(&owned.join("out.log")),
        ),
        &mut created,
    )
    .expect_err("the log directory is an account's");

    assert!(matches!(err, Error::DirectoryRefused { .. }), "{err:?}");
}

/// The log in a `cwd` that is to be the account's, or under it, is refused naming the directory and the
/// account, and what the install had made by then is taken back when the guard drops: that the two are
/// one directory is found when the `cwd` is reached.
#[skuld::test(labels = [ELEVATED])]
fn a_log_in_or_under_the_cwd_to_be_handed_is_refused_and_what_was_made_is_taken_back() {
    let layout = Layout::new();
    let cwd = layout.system().join("work");
    for logs in [cwd.join("out.log"), cwd.join("sub/out.log")] {
        let mut created = CreatedDirs::under(vec![layout.system()]);

        let err = ensure_parent_dirs(
            &spec(User::Id(AccountId::Uid(STRANGER)), Some(&cwd), Some(&logs)),
            &mut created,
        )
        .expect_err("a log in the directory that is to be the account's");
        drop(created);

        let Error::DirectoryRefused { path, why } = err else {
            panic!("{err:?}")
        };
        assert_eq!(path, cwd, "{why}");
        assert!(why.contains(&format!("uid {STRANGER}")) && !why.contains("  "), "{why}");
        assert!(!cwd.exists(), "{}: what was made for the log was left", logs.display());
    }
}

/// A log directory beside the `cwd` that is handed over is accepted, and a log directory above it too.
#[skuld::test(labels = [ELEVATED])]
fn a_log_beside_or_above_the_cwd_to_be_handed_is_accepted() {
    let layout = Layout::new();
    for (cwd, logs) in [("work", "logs/out.log"), ("a/work", "a/out.log")] {
        let mut created = CreatedDirs::under(vec![layout.system()]);

        ensure_parent_dirs(
            &spec(
                User::Id(AccountId::Uid(STRANGER)),
                Some(&layout.system().join(cwd)),
                Some(&layout.system().join(logs)),
            ),
            &mut created,
        )
        .unwrap_or_else(|e| panic!("{cwd} {logs}: {e}"));

        assert_eq!(fs::metadata(layout.system().join(cwd)).unwrap().uid(), STRANGER);
        assert_eq!(
            fs::metadata(layout.system().join(logs).parent().unwrap())
                .unwrap()
                .uid(),
            0
        );
        created.settle(|| Standing::No);
    }
}
