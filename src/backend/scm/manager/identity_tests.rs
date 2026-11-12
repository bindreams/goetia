use super::*;
use crate::backend::scm::sid::{is_whole_sid, sid_from_parts, system};

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
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: cannot parse `user.id` `not-a-sid` as a SID string"
    );
    assert!(
        err.report()
            .to_string()
            .starts_with("account lookup failed: cannot parse `user.id` `not-a-sid` as a SID string: "),
        "{}",
        err.report()
    );
}

// service_password ====================================================================================================

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
    assert!(matches!(err, Error::InvalidPassword { .. }), "{err:?}");
    assert_eq!(
        err.report().to_string(),
        "`GOETIA_SERVICE_PASSWORD` is set but its value is not valid Unicode"
    );
}

// account_name_of_sid =================================================================================================

#[skuld::test]
fn a_sid_in_bytes_resolves_to_its_account_name() {
    let name = account_name_of_sid(&system()).expect("LocalSystem always resolves");
    assert!(name.contains("SYSTEM"), "{name}");
}

#[skuld::test]
fn bytes_that_are_not_a_sid_are_an_error_not_a_read_past_them() {
    let err = account_name_of_sid(&[1, 3, 0, 0, 0, 0, 0, 5, 1, 0]).expect_err("truncated");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
}

#[skuld::test]
fn a_nul_in_a_sid_string_is_refused_not_cut_there() {
    // Cut at the NUL, this would resolve `S-1-5-18`.
    let err = account_name_from_sid_string("S-1-5-18\0junk").expect_err("a NUL ends nothing here");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: cannot parse `user.id` `S-1-5-18\0junk` as a SID string"
    );
}

#[skuld::test]
fn a_nul_in_an_account_name_is_refused_not_cut_there() {
    let err = sid_for_account_name("SYSTEM\0junk", Some("user.name")).expect_err("a NUL ends nothing here");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: cannot resolve SID for `user.name` `SYSTEM\0junk`"
    );
}

// A lookup that finds no account ======================================================================================

#[skuld::test]
fn a_well_formed_sid_no_account_has_is_an_account_lookup_naming_the_field() {
    let err = account_name_from_sid_string("S-1-5-21-1-2-3-999999").expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve account name for `user.id` `S-1-5-21-1-2-3-999999`"),
        "{err}"
    );
}

#[skuld::test]
fn a_binary_sid_no_account_has_is_an_account_lookup_naming_no_field() {
    let sid = sid_from_parts(5, &[21, 1, 2, 3, 999999]);
    let err = account_name_of_sid(&sid).expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve account name for SID `S-1-5-21-1-2-3-999999`"),
        "{err}"
    );
}

#[skuld::test]
fn an_unknown_account_name_is_an_account_lookup_naming_the_field() {
    let err = sid_for_account_name("goetia-no-such-account-7f3a", Some("user.name")).expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve SID for `user.name` `goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

#[skuld::test]
fn an_account_name_without_a_field_names_the_account() {
    let err = sid_for_account_name("goetia-no-such-account-7f3a", None).expect_err("no such account");
    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve SID for account `goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

#[skuld::test]
fn a_name_is_quoted_as_written_and_its_backslash_is_not_doubled() {
    // A backslash then `n` must not read as an escaped newline either.
    let err = sid_for_account_name(r"GOETIA-NO-SUCH-DOMAIN\nobody", Some("user.name")).expect_err("no such domain");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with(r"account lookup failed: cannot resolve SID for `user.name` `GOETIA-NO-SUCH-DOMAIN\nobody`"),
        "{err}"
    );
    let err = sid_for_account_name(r".\goetia-no-such-account-7f3a", Some("user.name")).expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with(r"account lookup failed: cannot resolve SID for `user.name` `.\goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

// An account that is not a user =======================================================================================

#[skuld::test]
fn a_name_that_is_no_user_account_is_refused_before_any_grant() {
    // Each names nothing, or something that is not a user: granting the right to it would change who may
    // log on as a service on the whole host.
    for name in [
        "",
        r"\",
        r".\",
        r"NT SERVICE\",
        "Administrators",
        "Users",
        r"NT AUTHORITY\SYSTEM",
    ] {
        let err = sid_for_account_name(name, Some("user.name")).expect_err(name);
        assert!(matches!(err, Error::AccountLookup { .. }), "`{name}`: {err:?}");
        assert!(err.to_string().contains("`user.name`"), "`{name}`: {err}");
    }
}

#[skuld::test]
fn a_group_is_refused_saying_it_is_not_a_user() {
    let err = sid_for_account_name("Administrators", Some("user.name")).expect_err("a group");
    assert_eq!(
        err.to_string(),
        "account lookup failed: `user.name` `Administrators` is not a user account (it is a local group)"
    );
}

#[skuld::test]
fn the_account_this_test_runs_as_is_a_user() {
    let name = std::env::var("USERNAME").expect("Windows sets USERNAME");
    let sid = sid_for_account_name(&name, Some("user.name")).expect("the current user is a user account");
    assert!(is_whole_sid(&sid));
}

#[skuld::test]
fn the_field_reaches_the_grant_refusal() {
    // No such account: refused by the lookup, before the policy is opened.
    let err = grant_service_logon_right("goetia-no-such-account-7f3a", Some("user.name")).expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string().contains("`user.name` `goetia-no-such-account-7f3a`"),
        "{err}"
    );
    let err = grant_service_logon_right("goetia-no-such-account-7f3a", None).expect_err("no such account");
    assert!(
        err.to_string().contains("account `goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

// account_error =======================================================================================================

#[skuld::test]
fn an_input_code_is_an_account_lookup_and_any_other_code_is_an_os_failure() {
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_PARAMETER, ERROR_INVALID_SID, ERROR_NONE_MAPPED, ERROR_NOT_ENOUGH_MEMORY,
    };
    for code in [ERROR_NONE_MAPPED, ERROR_INVALID_SID, ERROR_INVALID_PARAMETER] {
        let err = account_error("resolve x".to_string(), std::io::Error::from_raw_os_error(code as i32));
        assert!(matches!(err, Error::AccountLookup { .. }), "{code}: {err:?}");
    }
    let err = account_error(
        "resolve x".to_string(),
        std::io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32),
    );
    assert!(matches!(err, Error::Os { .. }), "{err:?}");
}
