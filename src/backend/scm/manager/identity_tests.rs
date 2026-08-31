use super::*;

#[skuld::test]
fn root_resolves_to_an_empty_unused_identity() {
    // `generate::registration` maps `User::Root` to `account: None`
    // (`LocalSystem`) without ever consulting `Identity` — see its own doc
    // comment — so the exact value here is unobservable, only that no
    // lookup is attempted (and therefore cannot fail).
    let identity = resolve(&User::Root).expect("Root never fails to resolve");
    assert_eq!(identity.user, "");
}

#[skuld::test]
fn name_resolves_to_itself_with_no_lookup() {
    let identity = resolve(&User::Name("bindreams".to_string())).expect("Name never fails to resolve");
    assert_eq!(identity.user, "bindreams");
}

#[skuld::test]
fn numeric_uid_is_rejected_with_a_message_naming_the_alternative() {
    let err = resolve(&User::Id(AccountId::Uid(1000))).expect_err("Windows has no UID account mapping");
    let message = err.to_string();
    assert!(
        message.contains("1000"),
        "message should name the offending uid: {message}"
    );
    assert!(
        message.contains("user.name") || message.contains("user.id"),
        "message should name a working alternative: {message}"
    );
}
