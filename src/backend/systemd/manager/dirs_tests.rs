use super::*;

fn ids(owner: Option<Owner>) -> (u32, Option<u32>) {
    let owner = owner.expect("an account to hand over to");
    (owner.uid.as_raw(), owner.gid.map(|gid| gid.as_raw()))
}

#[skuld::test]
fn root_is_handed_nothing() {
    assert!(owner_of(&User::Root).unwrap().is_none());
}

#[skuld::test]
fn a_name_is_the_accounts_uid_and_primary_group() {
    assert_eq!(ids(owner_of(&User::Name("root".to_string())).unwrap()), (0, Some(0)));
}

#[skuld::test]
fn a_uid_with_an_entry_takes_the_entrys_group() {
    assert_eq!(ids(owner_of(&User::Id(AccountId::Uid(0))).unwrap()), (0, Some(0)));
}

/// A numeric uid that names no account is still handed to; its group is left as it is.
#[skuld::test]
fn a_uid_with_no_entry_leaves_the_group() {
    assert_eq!(
        ids(owner_of(&User::Id(AccountId::Uid(4_242_424))).unwrap()),
        (4_242_424, None)
    );
}

/// A name that is no account is read as a number, as `chown` reads one.
#[skuld::test]
fn a_numeric_name_with_no_entry_is_a_uid() {
    assert_eq!(
        ids(owner_of(&User::Name("4242424".to_string())).unwrap()),
        (4_242_424, None)
    );
}

#[skuld::test]
fn a_name_that_is_no_account_and_no_number_is_an_error() {
    let err = owner_of(&User::Name("no-such-goetia-account".to_string())).expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
}

/// What `chown` reads as "unchanged" is no account.
#[skuld::test]
fn the_uid_that_means_unchanged_is_refused() {
    owner_of(&User::Id(AccountId::Uid(u32::MAX))).expect_err("`chown` would leave the owner as it is");
}
