use std::io;

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_INVALID_SID, ERROR_NONE_MAPPED, ERROR_NOT_ENOUGH_MEMORY,
};

use super::*;
use crate::backend::scm::manager::accounts::fake::{Asked, Fake};
use crate::backend::scm::manager::accounts::{AccountKind, Lsa};
use crate::backend::scm::sid::{sid_display, sid_from_parts, system};
use crate::elevation::test_support::process_user_sid;

/// The SID of `Everyone`, `S-1-1-0`.
fn everyone_sid() -> Sid {
    sid_from_parts(1, &[0])
}

/// A SID no account has, `S-1-5-21-1-2-3-999999`.
fn nobody_sid() -> Sid {
    sid_from_parts(5, &[21, 1, 2, 3, 999999])
}

#[skuld::test]
fn root_resolves_to_an_empty_unused_identity() {
    // `generate::registration` maps `User::Root` to `account: None`
    // (`LocalSystem`) without ever consulting `Identity` — see its own doc
    // comment — so the exact value here is unobservable, only that no
    // lookup is attempted (and therefore cannot fail).
    let accounts = Fake::new();

    let identity = resolve(&accounts, &User::Root).expect("Root never fails to resolve");

    assert_eq!(identity.user, "");
    assert_eq!(accounts.asked(), []);
}

#[skuld::test]
fn name_resolves_to_itself_with_no_lookup() {
    let accounts = Fake::new();

    let identity = resolve(&accounts, &User::Name("bindreams".to_string())).expect("Name never fails to resolve");

    assert_eq!(identity.user, "bindreams");
    assert_eq!(accounts.asked(), []);
}

#[skuld::test]
fn numeric_uid_is_rejected_with_a_message_naming_the_alternative() {
    let err = resolve(&Fake::new(), &User::Id(AccountId::Uid(1000))).expect_err("Windows has no UID account mapping");
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
fn a_sid_string_resolves_to_the_name_the_database_gives_that_sid() {
    let accounts = Fake::new().account(r"NT AUTHORITY\SYSTEM", &system(), AccountKind::WellKnownGroup);

    let identity = resolve(&accounts, &User::Id(AccountId::Sid("S-1-5-18".to_string()))).expect("S-1-5-18 resolves");

    assert_eq!(identity.user, r"NT AUTHORITY\SYSTEM");
    assert_eq!(accounts.asked(), [Asked::Sid(system())]);
}

#[skuld::test]
fn sid_rejects_a_malformed_sid_string() {
    let accounts = Fake::new();

    let err =
        resolve(&accounts, &User::Id(AccountId::Sid("not-a-sid".to_string()))).expect_err("not a valid SID string");

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
    assert_eq!(
        accounts.asked(),
        [],
        "a string that is no SID asks the database nothing"
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

// What a failed lookup says ===========================================================================================

#[skuld::test]
fn a_nul_in_a_sid_string_is_refused_not_cut_there() {
    // Cut at the NUL, this would resolve `S-1-5-18`.
    let accounts = Fake::new();

    let err = account_name_from_sid_string(&accounts, "S-1-5-18\0junk").expect_err("a NUL ends nothing here");

    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: cannot parse `user.id` `S-1-5-18\0junk` as a SID string"
    );
    assert_eq!(accounts.asked(), []);
}

#[skuld::test]
fn a_name_the_database_refuses_for_its_form_is_an_account_lookup_naming_the_field() {
    // `Lsa` refuses a NUL before any call, with `InvalidInput` and no OS code.
    let refused = || io::Error::new(io::ErrorKind::InvalidInput, "the text holds a NUL at position 6");
    let accounts = Fake::new().fails("SYSTEM\0junk", refused());

    let err = lookup_name(&accounts, "SYSTEM\0junk", Some("user.name")).expect_err("a NUL ends nothing here");

    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: cannot resolve SID for `user.name` `SYSTEM\0junk`"
    );
}

#[skuld::test]
fn a_well_formed_sid_no_account_has_is_an_account_lookup_naming_the_field() {
    let accounts = Fake::new().no_such_sid(&nobody_sid());

    let err = account_name_from_sid_string(&accounts, "S-1-5-21-1-2-3-999999").expect_err("no such account");

    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve account name for `user.id` `S-1-5-21-1-2-3-999999`"),
        "{err}"
    );
}

#[skuld::test]
fn an_unknown_account_name_is_an_account_lookup_naming_the_field() {
    let accounts = Fake::new().no_such_account("goetia-no-such-account-7f3a");

    let err = lookup_name(&accounts, "goetia-no-such-account-7f3a", Some("user.name")).expect_err("no such account");

    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve SID for `user.name` `goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

#[skuld::test]
fn an_account_name_without_a_field_names_the_account() {
    let accounts = Fake::new().no_such_account("goetia-no-such-account-7f3a");

    let err = lookup_name(&accounts, "goetia-no-such-account-7f3a", None).expect_err("no such account");

    assert!(
        err.to_string()
            .starts_with("account lookup failed: cannot resolve SID for account `goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

#[skuld::test]
fn a_name_is_quoted_as_written_and_its_backslash_is_not_doubled() {
    // A backslash then `n` must not read as an escaped newline either.
    let accounts = Fake::new()
        .no_such_account(r"GOETIA-NO-SUCH-DOMAIN\nobody")
        .no_such_account(r".\goetia-no-such-account-7f3a");

    let err = lookup_name(&accounts, r"GOETIA-NO-SUCH-DOMAIN\nobody", Some("user.name")).expect_err("no such domain");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with(r"account lookup failed: cannot resolve SID for `user.name` `GOETIA-NO-SUCH-DOMAIN\nobody`"),
        "{err}"
    );
    let err = lookup_name(&accounts, r".\goetia-no-such-account-7f3a", Some("user.name")).expect_err("no such account");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with(r"account lookup failed: cannot resolve SID for `user.name` `.\goetia-no-such-account-7f3a`"),
        "{err}"
    );
}

#[skuld::test]
fn a_lookup_that_fails_for_another_reason_is_an_os_failure_naming_the_field() {
    let accounts = Fake::new().fails("alice", io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32));

    let err = lookup_name(&accounts, "alice", Some("user.name")).expect_err("out of memory");

    assert!(matches!(err, Error::Os { .. }), "{err:?}");
    assert_eq!(err.to_string(), "failed to resolve SID for `user.name` `alice`");
}

// An account that is not a user =======================================================================================

#[skuld::test]
fn a_name_that_is_no_user_account_is_refused() {
    // Each names nothing, or something that is not a user: granting the right to it would change who may
    // log on as a service on the whole host. The names that need a password are also pinned in order,
    // through `apply`, in `manager_tests.rs`; `apply` skips the check for `""`, `NT SERVICE\` and
    // `NT AUTHORITY\SYSTEM`, which need none, so they are pinned only here.
    let accounts = Fake::new()
        .no_such_account("")
        .no_such_account(r"\")
        .no_such_account(r".\")
        .no_such_account(r"NT SERVICE\")
        .account(
            "Administrators",
            &sid_from_parts(5, &[32, 544]),
            AccountKind::LocalGroup,
        )
        .account("Users", &sid_from_parts(5, &[32, 545]), AccountKind::LocalGroup)
        .account(r"NT AUTHORITY\SYSTEM", &system(), AccountKind::WellKnownGroup);
    for name in [
        "",
        r"\",
        r".\",
        r"NT SERVICE\",
        "Administrators",
        "Users",
        r"NT AUTHORITY\SYSTEM",
    ] {
        let err = require_user_account(&accounts, name, Some(Written::Name(name))).expect_err(name);
        assert!(matches!(err, Error::AccountLookup { .. }), "`{name}`: {err:?}");
        assert!(err.to_string().contains("`user.name`"), "`{name}`: {err}");
    }
}

#[skuld::test]
fn each_kind_that_is_no_user_is_named_in_the_refusal() {
    for (kind, said) in [
        (AccountKind::Group, "a group"),
        (AccountKind::Domain, "a domain"),
        (AccountKind::LocalGroup, "a local group"),
        (AccountKind::WellKnownGroup, "a well-known group"),
        (AccountKind::DeletedAccount, "a deleted account"),
        (AccountKind::Computer, "a computer"),
        (AccountKind::IntegrityLabel, "an integrity label"),
        (AccountKind::LogonSession, "a logon session"),
        (AccountKind::Other, "of no known kind"),
    ] {
        let accounts = Fake::new().account("thing", &nobody_sid(), kind);

        let err = require_user_account(&accounts, "thing", None).expect_err("not a user");

        assert_eq!(
            err.to_string(),
            format!("account lookup failed: account `thing` is not a user account (it is {said})"),
            "{kind:?}"
        );
    }
}

#[skuld::test]
fn a_group_is_refused_saying_it_is_not_a_user() {
    let accounts = Fake::new().account(
        "Administrators",
        &sid_from_parts(5, &[32, 544]),
        AccountKind::LocalGroup,
    );

    let err =
        require_user_account(&accounts, "Administrators", Some(Written::Name("Administrators"))).expect_err("a group");

    assert_eq!(
        err.to_string(),
        "account lookup failed: `user.name` `Administrators` is not a user account (it is a local group)"
    );
}

#[skuld::test]
fn a_sid_that_resolves_to_a_group_is_refused_naming_the_sid_as_written() {
    // `S-1-1-0` resolves to `Everyone`; the refusal quotes what the user wrote.
    let accounts = Fake::new().account("Everyone", &everyone_sid(), AccountKind::WellKnownGroup);
    let account = resolve(&accounts, &User::Id(AccountId::Sid("S-1-1-0".to_string())))
        .expect("Everyone resolves")
        .user;

    let err = require_user_account(&accounts, &account, Some(Written::Sid("S-1-1-0"))).expect_err("a group");

    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: `user.id` `S-1-1-0` is not a user account (it is a well-known group)"
    );
}

#[skuld::test]
fn a_refused_account_without_a_written_value_is_named_as_it_is() {
    let accounts = Fake::new().account(
        "Administrators",
        &sid_from_parts(5, &[32, 544]),
        AccountKind::LocalGroup,
    );

    let err = require_user_account(&accounts, "Administrators", None).expect_err("a group");

    assert_eq!(
        err.to_string(),
        "account lookup failed: account `Administrators` is not a user account (it is a local group)"
    );
}

#[skuld::test]
fn a_user_is_accepted_with_the_sid_and_the_name_that_were_asked() {
    let accounts = Fake::new().user("alice", &nobody_sid());

    let user = require_user_account(&accounts, "alice", Some(Written::Name("alice"))).expect("a user");

    assert_eq!(user.sid(), &nobody_sid());
    assert_eq!(user.account(), "alice");
}

#[skuld::test]
fn a_user_written_as_a_sid_is_accepted_with_the_name_of_that_sid_and_no_name_is_looked_up() {
    let accounts = Fake::new().user(r"HOST\alice", &nobody_sid());

    let user = require_user_account(
        &accounts,
        "whatever the registration says",
        Some(Written::Sid("S-1-5-21-1-2-3-999999")),
    )
    .expect("a user");

    assert_eq!(user.sid(), &nobody_sid());
    assert_eq!(user.account(), r"HOST\alice");
    assert_eq!(accounts.asked(), [Asked::Sid(nobody_sid())]);
}

#[skuld::test]
fn the_account_this_test_runs_as_is_accepted_if_it_is_a_user_and_refused_if_it_is_not() {
    // The token's own SID, written as `user.id`, against the real database: a user account on a developer host,
    // `SYSTEM` (a well-known group) on a service host. Both are lookups only.
    let token_sid = process_user_sid();
    let written = sid_display(&token_sid);
    let account = resolve_sid_string(&Lsa, &written).expect("the token user resolves");
    let result = require_user_account(&Lsa, &account.name, Some(Written::Sid(&written)));
    if account.kind == AccountKind::User {
        let user = result.expect("a user account is accepted");
        assert_eq!(user.sid(), &token_sid);
        assert_eq!(user.account(), account.name);
    } else {
        let err = result.expect_err("not a user account");
        assert!(err.to_string().contains("is not a user account"), "{err}");
    }
}

// account_error =======================================================================================================

#[skuld::test]
fn an_input_code_is_an_account_lookup_and_any_other_code_is_an_os_failure() {
    for code in [ERROR_NONE_MAPPED, ERROR_INVALID_SID, ERROR_INVALID_PARAMETER] {
        let err = account_error("resolve x".to_string(), io::Error::from_raw_os_error(code as i32));
        assert!(matches!(err, Error::AccountLookup { .. }), "{code}: {err:?}");
    }
    let err = account_error(
        "resolve x".to_string(),
        io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32),
    );
    assert!(matches!(err, Error::Os { .. }), "{err:?}");
}

#[skuld::test]
fn a_refusal_before_any_call_is_an_account_lookup_and_an_other_failure_without_a_code_is_not() {
    let refused = account_error(
        "resolve x".to_string(),
        io::Error::new(io::ErrorKind::InvalidInput, "refused"),
    );
    assert!(matches!(refused, Error::AccountLookup { .. }), "{refused:?}");
    let other = account_error("resolve x".to_string(), io::Error::other("no whole SID"));
    assert!(matches!(other, Error::Os { .. }), "{other:?}");
}

// Right ===============================================================================================================

#[skuld::test]
fn each_right_is_named_as_windows_names_it() {
    assert_eq!(Right::Service.name(), "SeServiceLogonRight");
    assert_eq!(Right::Batch.name(), "SeBatchLogonRight");
}

// service_account_sid =================================================================================================

/// A service with no account, and every spelling of a built-in that goetia registers or Windows reads back,
/// runs as that built-in's well-known SID, with no lookup.
#[skuld::test]
fn a_services_built_in_account_is_its_well_known_sid() {
    let accounts = Fake::new();
    for (account, sid) in [
        (None, system()),
        (Some("LocalSystem"), system()),
        (Some(r"NT AUTHORITY\LocalService"), sid_from_parts(5, &[19])),
        (Some(r"nt authority\local service"), sid_from_parts(5, &[19])),
        (Some(r"NT AUTHORITY\NetworkService"), sid_from_parts(5, &[20])),
        (Some("NetworkService"), sid_from_parts(5, &[20])),
    ] {
        assert_eq!(
            service_account_sid(&accounts, account, None).unwrap(),
            sid,
            "{account:?}"
        );
    }
    assert_eq!(accounts.asked(), []);
}

/// A spec that gave a SID names that SID, as the logon gate does: the account name it registers is not
/// looked up again, which could name another account.
#[skuld::test]
fn a_service_whose_spec_gave_a_sid_is_that_sid_with_no_lookup() {
    let users = sid_from_parts(5, &[32, 545]);
    let accounts = Fake::new();

    let sid = service_account_sid(
        &accounts,
        Some("no-such-account-goetia"),
        Some(Written::Sid("S-1-5-32-545")),
    );

    assert_eq!(sid.unwrap(), users);
    let built_in = service_account_sid(&accounts, Some("NetworkService"), Some(Written::Name("NetworkService")));
    assert_eq!(built_in.unwrap(), sid_from_parts(5, &[20]));
    assert_eq!(accounts.asked(), []);
}

#[skuld::test]
fn any_other_service_account_is_looked_up_by_name() {
    let accounts = Fake::new().user("alice", &nobody_sid());

    let sid = service_account_sid(&accounts, Some("alice"), Some(Written::Name("alice")));

    assert_eq!(sid.unwrap(), nobody_sid());
    assert_eq!(accounts.asked(), [Asked::Name("alice".to_string())]);
}

// sid_of_principal ====================================================================================================

#[skuld::test]
fn a_principal_that_is_a_sid_string_is_that_sid_and_any_other_is_looked_up_by_name() {
    let accounts = Fake::new().user("alice", &nobody_sid());

    assert_eq!(sid_of_principal(&accounts, "S-1-5-18").unwrap(), system());
    assert_eq!(accounts.asked(), [], "a SID string asks nothing");
    assert_eq!(sid_of_principal(&accounts, "alice").unwrap(), nobody_sid());
    assert_eq!(accounts.asked(), [Asked::Name("alice".to_string())]);
}
