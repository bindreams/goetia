use super::*;
use crate::error::Error;
use crate::spec::AccountId;

fn gid(account: &Account) -> u32 {
    account.gid.as_raw()
}

#[skuld::test]
fn root_is_uid_zero_named_root() {
    let account = resolve_account(&User::Root).expect("root always exists");
    assert_eq!((account.uid.as_raw(), account.name.as_str()), (0, "root"));
}

/// `install` compares the identity of the new spec with the embedded one, which must agree whichever
/// spelling of uid 0 either used.
#[skuld::test]
fn uid_zero_is_also_root() {
    let account = resolve_account(&User::Id(AccountId::Uid(0))).unwrap();
    assert_eq!(account.name, "root");
}

#[skuld::test]
fn a_name_is_the_accounts_uid_and_primary_group() {
    let account = resolve_account(&User::Name("root".to_string())).unwrap();
    assert_eq!((account.uid.as_raw(), gid(&account)), (0, 0));
}

#[skuld::test]
fn a_windows_sid_is_rejected() {
    let sid = User::Id(AccountId::Sid("S-1-5-21-0".to_string()));
    let err = resolve_account(&sid).expect_err("a SID names no Unix account");
    assert!(
        matches!(err, Error::AccountLookup { .. }) && err.to_string().contains("SID"),
        "{err}"
    );
}

#[skuld::test]
fn a_name_that_is_no_account_is_an_error() {
    let err = resolve_account(&User::Name("goetia-no-such-user-xyz".to_string())).expect_err("no such user");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(err.to_string().contains("goetia-no-such-user-xyz"), "{err}");
}

/// An account with no passwd entry is an error, a number included: a number given as a name is looked up
/// as a name.
#[skuld::test]
fn a_uid_or_a_number_with_no_entry_is_an_error() {
    for user in [User::Id(AccountId::Uid(4_242_424)), User::Name("4242424".to_string())] {
        let err = resolve_account(&user).expect_err("no such account");
        assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    }
}

#[skuld::test]
fn a_uid_with_an_entry_takes_the_entrys_group() {
    let account = resolve_account(&User::Id(AccountId::Uid(0))).unwrap();
    assert_eq!(gid(&account), 0);
}

#[skuld::test]
fn an_account_is_an_owner_with_its_group() {
    let account = resolve_account(&User::Name("nobody".to_string())).expect("every host has `nobody`");
    let owner = account
        .owner()
        .unwrap()
        .expect("an account that is not root's is handed what it is to write");
    assert_eq!((owner.uid, owner.gid), (account.uid, Some(account.gid)));
}

/// A directory is already root's, and `chown(0, 0)` would take its group away: uid 0 is handed nothing,
/// by any spelling.
#[skuld::test]
fn root_is_handed_nothing() {
    for user in [User::Root, User::Id(AccountId::Uid(0)), User::Name("root".to_string())] {
        let account = resolve_account(&user).unwrap_or_else(|e| panic!("{user:?}: {e}"));
        assert!(account.owner().unwrap().is_none(), "{user:?}");
    }
}

#[cfg(target_os = "linux")]
#[skuld::test]
fn an_account_is_named_by_its_entry_and_always_by_its_uid() {
    assert_eq!(describe_uid(0), "`root` (uid 0)");
    assert_eq!(describe_uid(4_242_424), "uid 4242424");
}
