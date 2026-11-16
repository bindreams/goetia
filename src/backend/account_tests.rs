use super::*;
use crate::error::Error;
use crate::spec::AccountId;

fn gid(account: &Account) -> Option<u32> {
    account.gid.map(|gid| gid.as_raw())
}

#[skuld::test]
fn root_is_uid_zero_named_root() {
    let account = resolve_account(&User::Root, Unlisted::Refuse).expect("root always exists");
    assert_eq!((account.uid.as_raw(), account.name.as_str()), (0, "root"));
}

/// `install` compares the identity of the new spec with the embedded one, which must agree whichever
/// spelling of uid 0 either used.
#[skuld::test]
fn uid_zero_is_also_root() {
    let account = resolve_account(&User::Id(AccountId::Uid(0)), Unlisted::Refuse).unwrap();
    assert_eq!(account.name, "root");
}

#[skuld::test]
fn a_name_is_the_accounts_uid_and_primary_group() {
    let account = resolve_account(&User::Name("root".to_string()), Unlisted::AsNumber).unwrap();
    assert_eq!((account.uid.as_raw(), gid(&account)), (0, Some(0)));
}

#[skuld::test]
fn a_windows_sid_is_rejected() {
    let sid = User::Id(AccountId::Sid("S-1-5-21-0".to_string()));
    for unlisted in [Unlisted::Refuse, Unlisted::AsNumber] {
        let err = resolve_account(&sid, unlisted).expect_err("a SID names no Unix account");
        assert!(
            matches!(err, Error::AccountLookup { .. }) && err.to_string().contains("SID"),
            "{err}"
        );
    }
}

#[skuld::test]
fn a_name_that_is_no_account_and_no_number_is_an_error_either_way() {
    for unlisted in [Unlisted::Refuse, Unlisted::AsNumber] {
        let err =
            resolve_account(&User::Name("goetia-no-such-user-xyz".to_string()), unlisted).expect_err("no such user");
        assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
        assert!(err.to_string().contains("goetia-no-such-user-xyz"), "{err}");
    }
}

/// launchd's behaviour: an account with no passwd entry is an error, a number included.
#[skuld::test]
fn refuse_makes_a_uid_or_a_number_with_no_entry_an_error() {
    for user in [User::Id(AccountId::Uid(4_242_424)), User::Name("4242424".to_string())] {
        let err = resolve_account(&user, Unlisted::Refuse).expect_err("no such account");
        assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    }
}

/// systemd's behaviour, which `chown` shares: systemd takes `User=4242424` with no entry, so the
/// directory is handed to that uid and its group is left as it is.
#[skuld::test]
fn as_number_takes_a_uid_or_a_number_with_no_entry_as_it_is() {
    for user in [User::Id(AccountId::Uid(4_242_424)), User::Name("4242424".to_string())] {
        let account = resolve_account(&user, Unlisted::AsNumber).unwrap();
        assert_eq!((account.uid.as_raw(), gid(&account)), (4_242_424, None));
    }
}

#[skuld::test]
fn a_uid_with_an_entry_takes_the_entrys_group_either_way() {
    for unlisted in [Unlisted::Refuse, Unlisted::AsNumber] {
        let account = resolve_account(&User::Id(AccountId::Uid(0)), unlisted).unwrap();
        assert_eq!(gid(&account), Some(0));
    }
}

/// What `chown` reads as "unchanged" is no account.
#[skuld::test]
fn the_uid_that_means_unchanged_is_not_an_owner() {
    let account = resolve_account(&User::Id(AccountId::Uid(u32::MAX)), Unlisted::AsNumber).unwrap();
    account.owner().expect_err("`chown` would leave the owner as it is");
}

#[skuld::test]
fn an_account_is_an_owner_with_its_group() {
    let owner = resolve_account(&User::Root, Unlisted::Refuse).unwrap().owner().unwrap();
    assert_eq!((owner.uid.as_raw(), owner.gid.map(|gid| gid.as_raw())), (0, Some(0)));
}

#[cfg(target_os = "linux")]
#[skuld::test]
fn an_account_is_named_by_its_entry_and_always_by_its_uid() {
    assert_eq!(describe_uid(0), "`root` (uid 0)");
    assert_eq!(describe_uid(4_242_424), "uid 4242424");
}
