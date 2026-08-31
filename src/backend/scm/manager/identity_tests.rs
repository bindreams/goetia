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

#[skuld::test]
fn sid_resolves_a_well_known_sid_to_an_account_name() {
    // S-1-5-18 (LocalSystem) is a fixed, universal well-known SID:
    // `LookupAccountSidW` resolves it without elevation on any Windows host,
    // exercising `account_name_from_sid_string`'s real Win32 sizing/fetch
    // dance and `DOMAIN\Name` formatting.
    let identity = resolve(&User::Id(AccountId::Sid("S-1-5-18".to_string()))).expect("S-1-5-18 always resolves");
    assert!(!identity.user.is_empty());
    assert!(
        identity.user.to_ascii_uppercase().contains("SYSTEM"),
        "expected the LocalSystem account name, got {:?}",
        identity.user
    );
}

#[skuld::test]
fn sid_rejects_a_malformed_sid_string() {
    let err = resolve(&User::Id(AccountId::Sid("not-a-sid".to_string()))).expect_err("not a valid SID string");
    assert!(err.to_string().contains("not-a-sid"));
}

// service_password / account_needs_password ===========================================================================

#[skuld::test]
fn password_var_absent_is_none() {
    assert_eq!(parse_password_var(Err(std::env::VarError::NotPresent)).unwrap(), None);
}

#[skuld::test]
fn password_var_present_is_some() {
    assert_eq!(
        parse_password_var(Ok("hunter2".to_string())).unwrap(),
        Some("hunter2".to_string())
    );
}

#[skuld::test]
fn password_var_not_unicode_errs_rather_than_downgrading_to_none() {
    use std::os::windows::ffi::OsStringExt as _;
    // An unpaired surrogate: not representable as `String`, but a valid
    // `OsString` on Windows.
    let not_unicode = std::ffi::OsString::from_wide(&[0xD800]);
    let err = parse_password_var(Err(std::env::VarError::NotUnicode(not_unicode)))
        .expect_err("malformed password must not silently become \"no password\"");
    assert!(err.to_string().contains("GOETIA_SERVICE_PASSWORD"));
}

#[skuld::test]
fn account_needs_password_is_false_for_builtin_and_virtual_accounts() {
    for account in [
        "LocalSystem",
        "LocalService",
        "NetworkService",
        r"NT AUTHORITY\LocalService",
        r"NT AUTHORITY\NetworkService",
        r"NT AUTHORITY\SYSTEM",
        r"NT SERVICE\my-daemon",
        // Case-insensitive.
        "localsystem",
        r"nt service\my-daemon",
    ] {
        assert!(!account_needs_password(account), "{account} should not need a password");
    }
}

#[skuld::test]
fn account_needs_password_is_true_for_a_real_account() {
    for account in [r".\svc-account", "svc-account", r"CORP\svc-account"] {
        assert!(account_needs_password(account), "{account} should need a password");
    }
}
