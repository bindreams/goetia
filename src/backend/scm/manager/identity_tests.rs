use super::*;
use crate::backend::scm::sid::{sid_from_parts, system};

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
fn a_name_that_is_no_user_account_is_refused_before_the_password_is_asked_for() {
    // Each names nothing, or something that is not a user: granting the right to it would change who may
    // log on as a service on the whole host. The refusal is the lookup's, so an install that reaches the
    // password reads it first: were the password asked for first, a host without `GOETIA_SERVICE_PASSWORD`
    // would answer `PasswordRequired` here.
    for name in [
        "",
        r"\",
        r".\",
        r"NT SERVICE\",
        "Administrators",
        "Users",
        r"NT AUTHORITY\SYSTEM",
    ] {
        let err = service_password_for(name, Some(("user.name", name))).expect_err(name);
        assert!(matches!(err, Error::AccountLookup { .. }), "`{name}`: {err:?}");
        assert!(err.to_string().contains("`user.name`"), "`{name}`: {err}");
    }
}

#[skuld::test]
fn a_group_is_refused_saying_it_is_not_a_user() {
    let err = require_user_account("Administrators", Some(("user.name", "Administrators"))).expect_err("a group");
    assert_eq!(
        err.to_string(),
        "account lookup failed: `user.name` `Administrators` is not a user account (it is a local group)"
    );
}

#[skuld::test]
fn a_sid_that_resolves_to_a_group_is_refused_naming_the_sid_as_written() {
    // `S-1-1-0` resolves to `Everyone`; the refusal quotes what the user wrote.
    let account = resolve(&User::Id(AccountId::Sid("S-1-1-0".to_string())))
        .expect("Everyone resolves")
        .user;
    let err = service_password_for(&account, Some(("user.id", "S-1-1-0"))).expect_err("a group");
    assert!(matches!(err, Error::AccountLookup { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "account lookup failed: `user.id` `S-1-1-0` is not a user account (it is a well-known group)"
    );
}

#[skuld::test]
fn a_refused_account_without_a_written_value_is_named_as_it_is() {
    let err = require_user_account("Administrators", None).expect_err("a group");
    assert_eq!(
        err.to_string(),
        "account lookup failed: account `Administrators` is not a user account (it is a local group)"
    );
}

/// The user SID of this process's token.
fn process_user_sid() -> Vec<u8> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut raw = std::ptr::null_mut();
    // SAFETY: a pointer to a local; a returned token is owned below.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) },
        0,
        "OpenProcessToken: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a valid handle `OpenProcessToken` just returned and nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    // 8-byte alignment for `TOKEN_USER`, which the buffer holds.
    let mut buf = vec![0u64; 64];
    let mut needed = 0;
    // SAFETY: `token` is live with `TOKEN_QUERY`; `buf` is the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut needed,
        )
    };
    assert_ne!(ok, 0, "GetTokenInformation: {}", std::io::Error::last_os_error());
    // SAFETY: the call filled the buffer with a `TOKEN_USER`, whose `Sid` points into it.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: a valid SID in `buf`, which is live.
    unsafe { crate::backend::scm::sid::copy_sid(sid) }.expect("the token user is a SID")
}

#[skuld::test]
fn the_account_this_test_runs_as_is_a_user() {
    let sid = process_user_sid();
    let name = account_name_of_sid(&sid).expect("the token user has a name");
    let (found, kind) = sid_for_account_name(&name, None).expect("the current user resolves by name");
    assert_eq!(found, sid);
    assert_eq!(kind, SidTypeUser);
    require_user_account(&name, None).expect("the current user is a user account");
}

// lookup_growing ======================================================================================================

/// A lookup call that plays `steps` in order: each sets the two lengths and answers with a status, `0` for success.
fn scripted<const N: usize>(
    steps: [(u32, u32, u32); N],
) -> impl FnMut(*mut u16, &mut u32, *mut u16, &mut u32) -> std::io::Result<()> {
    let mut steps = steps.into_iter();
    move |_first, first_len, _domain, domain_len| {
        let (status, first, domain) = steps.next().expect("the call ran more often than scripted");
        *first_len = first;
        *domain_len = domain;
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    }
}

#[skuld::test]
fn a_lookup_that_needs_more_grows_the_buffers_to_what_it_reported_and_cuts_them_to_what_it_wrote() {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER as MORE;
    let call = scripted([(MORE, 10, 4), (0, 3, 2)]);

    let (first, domain) = lookup_growing(|| "look up".to_string(), call).expect("the second call fits");

    assert_eq!((first.len(), domain.len()), (3, 2));
}

#[skuld::test]
fn a_lookup_that_asks_for_no_more_than_it_has_stops_with_an_os_error() {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER as MORE;
    // Asking again with the same lengths would change nothing, whether on the first call or a later one.
    for steps in [vec![(MORE, 0, 0)], vec![(MORE, 5, 3), (MORE, 5, 3)]] {
        let mut steps = steps.into_iter();
        let call = move |_first: *mut u16, first_len: &mut u32, _domain: *mut u16, domain_len: &mut u32| {
            let (_, first, domain) = steps.next().expect("the call ran more often than scripted");
            *first_len = first;
            *domain_len = domain;
            Err(std::io::Error::from_raw_os_error(MORE as i32))
        };
        let err = lookup_growing(|| "look up".to_string(), call).expect_err("no progress");
        assert!(matches!(err, Error::Os { .. }), "{err:?}");
        assert_eq!(err.to_string(), "failed to look up");
    }
}

#[skuld::test]
fn a_success_that_wrote_nothing_or_reports_more_than_it_was_given_is_an_os_error() {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER as MORE;
    let empty = lookup_growing(|| "look up".to_string(), scripted([(MORE, 4, 2), (0, 0, 0)])).expect_err("empty");
    let past_first = lookup_growing(|| "look up".to_string(), scripted([(MORE, 4, 2), (0, 5, 0)])).expect_err("past");
    let past_domain = lookup_growing(|| "look up".to_string(), scripted([(MORE, 4, 2), (0, 4, 3)])).expect_err("past");
    // Success on the first call, with no buffer: the lengths cannot fit it.
    let unsized_ = lookup_growing(|| "look up".to_string(), scripted([(0, 1, 0)])).expect_err("past");
    for err in [empty, past_first, past_domain, unsized_] {
        assert!(matches!(err, Error::Os { .. }), "{err:?}");
        assert_eq!(err.to_string(), "failed to look up");
    }
}

#[skuld::test]
fn a_lookup_that_fails_otherwise_goes_to_account_error() {
    let none_mapped = lookup_growing(|| "look up".to_string(), scripted([(1332, 0, 0)])).expect_err("no account");
    assert!(matches!(none_mapped, Error::AccountLookup { .. }), "{none_mapped:?}");
    let other = lookup_growing(|| "look up".to_string(), scripted([(8, 0, 0)])).expect_err("out of memory");
    assert!(matches!(other, Error::Os { .. }), "{other:?}");
}

#[skuld::test]
fn a_sid_is_cut_to_the_length_its_sub_authority_count_gives() {
    let sid = sid_from_parts(5, &[21, 1, 2]);
    let mut padded = sid.clone();
    padded.extend_from_slice(&[0; 9]);
    assert_eq!(whole_sid_prefix(padded), Some(sid.clone()));
    assert_eq!(whole_sid_prefix(sid[..sid.len() - 1].to_vec()), None);
    assert_eq!(whole_sid_prefix(Vec::new()), None);
    assert_eq!(whole_sid_prefix(vec![1]), None);
    assert_eq!(whole_sid_prefix(vec![2, 0, 0, 0, 0, 0, 0, 5]), None);
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
