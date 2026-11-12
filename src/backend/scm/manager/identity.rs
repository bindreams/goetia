//! Windows account resolution and `SeServiceLogonRight` provisioning.
//!
//! Identity resolution is effectful (`spec::User -> backend::Identity`): a
//! Windows SID needs `LookupAccountSidW`, so it cannot happen inside the
//! pure generator (`generate::registration`) — see the crate-level design
//! notes on this (`backend::Identity`'s doc comment).
//!
//! **A real account needs `SeServiceLogonRight`.** `CreateServiceW` succeeds
//! without it, and the service's first start then fails with error 1069
//! (`ERROR_LOGON_FAILURE`) — install reports success, boot fails silently.
//! `sc.exe` does not grant this either. [`grant_service_logon_right`] grants
//! it via `LsaAddAccountRights`, idempotently (adding a right an account
//! already has is not an error) — `LocalSystem` is exempt (see `resolve`'s
//! caller in `manager.rs`), and neither reference repo (`hole`, `wsm`) has
//! solved this for any other account.

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_INVALID_SID, ERROR_NONE_MAPPED, LocalFree,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
    LookupAccountNameW, LookupAccountSidW, PSID, SidTypeAlias, SidTypeComputer, SidTypeDeletedAccount, SidTypeDomain,
    SidTypeGroup, SidTypeLabel, SidTypeLogonSession, SidTypeUser, SidTypeWellKnownGroup,
};
use windows_sys::core::w;

use crate::backend::Identity;
use crate::backend::scm::sid::{is_whole_sid, sid_display};
use crate::backend::scm::wide::{units_until_nul, wide_null};
use crate::error::{BoxError, Error, Result};
use crate::spec::{AccountId, User};

// resolve =============================================================================================================

/// `spec::User -> backend::Identity`, per the design spec's "Windows
/// accounts" section. `User::Root` resolves to an empty (unused) `user`:
/// `generate::registration` maps `User::Root` to `account: None`
/// (`LocalSystem`) without consulting `Identity` at all, so nothing here
/// needs to do a lookup for it.
pub fn resolve(user: &User) -> Result<Identity> {
    match user {
        User::Root => Ok(Identity { user: String::new() }),
        User::Name(name) => Ok(Identity { user: name.clone() }),
        User::Id(AccountId::Sid(sid)) => Ok(Identity {
            user: account_name_from_sid_string(sid)?,
        }),
        User::Id(AccountId::Uid(uid)) => Err(Error::AccountLookup {
            detail: format!(
                "user.id `{uid}` is a numeric UID, which Windows SCM has no account mapping for; use `user.name` \
                 (an account name) or `user.id` with a SID string instead"
            ),
            source: None,
        }),
    }
}

/// The `GOETIA_SERVICE_PASSWORD` environment variable, read fresh at every
/// install so a rotated password takes effect on the next `install` without
/// a process restart. Per the design spec's "Windows accounts" section: a
/// real user account needs a password; built-in (`LocalSystem`,
/// `LocalService`, `NetworkService`) and virtual (`NT SERVICE\<id>`)
/// accounts do not, and the caller is responsible for not setting this
/// variable when installing one of those. Never read from `goetia.yaml`
/// itself — the design spec is explicit that a password never appears
/// there.
///
/// Unset is `Ok(None)`. Set-but-not-valid-Unicode (env vars on Windows are
/// UTF-16 and can carry unpaired surrogates) is `Err`, not `Ok(None)`:
/// silently downgrading a malformed password to "no password" would install
/// a real account that cannot log on, with a message that has nothing to do
/// with the actual cause.
pub fn service_password() -> Result<Option<String>> {
    parse_password_var(std::env::var("GOETIA_SERVICE_PASSWORD"))
}

/// The decision [`service_password`] makes, pulled out as a pure function of
/// the lookup result so it is unit-testable without mutating the process's
/// real environment (which — shared mutable state across every concurrently
/// running `#[skuld::test]` — `std::env::set_var` cannot safely do; see
/// `tests/scm_integration/install_helper.rs`'s doc comment for the same
/// reasoning applied at the integration-test level).
fn parse_password_var(var: std::result::Result<String, std::env::VarError>) -> Result<Option<String>> {
    match var {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        // The `VarError` is not the source: its `Display` quotes the value, which is the password.
        Err(std::env::VarError::NotUnicode(_)) => Err(Error::InvalidPassword {
            variable: "GOETIA_SERVICE_PASSWORD".to_string(),
        }),
    }
}

/// What an account lookup was handed, for a message: `value` under the spec field it came from,
/// or under `noun` when it did not come from one (a name `LookupAccountSidW` returned). `value` is
/// plain; the report escapes it.
fn subject(field: Option<&str>, noun: &str, value: &str) -> String {
    match field {
        Some(field) => format!("`{field}` `{value}`"),
        None => format!("{noun} `{value}`"),
    }
}

/// The failure of a Win32 account lookup that was doing `action`. An input the OS cannot take (no
/// such account, a SID it does not accept, a string that is not a SID) is an
/// [`Error::AccountLookup`]; any other code (`ERROR_NOT_ENOUGH_MEMORY`) is an [`Error::Os`].
fn account_error(action: String, err: std::io::Error) -> Error {
    let input = [ERROR_NONE_MAPPED, ERROR_INVALID_SID, ERROR_INVALID_PARAMETER]
        .iter()
        .any(|&code| err.raw_os_error() == Some(code as i32));
    if input {
        Error::AccountLookup {
            detail: format!("cannot {action}"),
            source: Some(err.into()),
        }
    } else {
        Error::os(action, err)
    }
}

/// The refusal of a spec value that holds a NUL: no Windows call could take it whole.
fn refused_input(action: String, cause: impl Into<BoxError>) -> Error {
    Error::AccountLookup {
        detail: format!("cannot {action}"),
        source: Some(cause.into()),
    }
}

// SID <-> account name ================================================================================================

/// `ConvertStringSidToSidW` + `LookupAccountSidW`: turn a `user.id` SID
/// string into `DOMAIN\Name` (or bare `Name` for an unqualified/well-known
/// SID), which is what `ServiceInfo::account_name` needs — SCM has no SID
/// field.
fn account_name_from_sid_string(sid_str: &str) -> Result<String> {
    let parse = || format!("parse {} as a SID string", subject(Some("user.id"), "SID", sid_str));
    let wide_sid = wide_null(sid_str).map_err(|e| refused_input(parse(), e))?;
    let mut psid: PSID = std::ptr::null_mut();
    // SAFETY: `wide_sid` is a valid null-terminated wide string; `psid` is a
    // valid out-param. On success, `psid` must be freed with `LocalFree`.
    let ok = unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut psid) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        return Err(account_error(parse(), err));
    }
    let result = lookup_account_sid(psid, Some("user.id"), || sid_str.to_string());
    // SAFETY: `psid` was allocated by `ConvertStringSidToSidW` above and is
    // freed exactly once, after every use of it above has completed.
    unsafe {
        LocalFree(psid);
    }
    result
}

/// The domain-qualified name of the account a binary SID names. An error when the bytes are not
/// a SID or no account has it (a deleted account's SID still appears in ACLs).
pub(in crate::backend::scm) fn account_name_of_sid(sid: &[u8]) -> Result<String> {
    // `is_whole_sid` checks the length the SID's own sub-authority count implies, which the lookup
    // would otherwise read past a short slice to find out.
    if !is_whole_sid(sid) {
        return Err(Error::AccountLookup {
            detail: format!("{sid:?} is not a SID"),
            source: None,
        });
    }
    lookup_account_sid(sid.as_ptr() as PSID, None, || sid_display(sid))
}

/// A pointer to `buf` for a Win32 call that takes its length alongside, null while it is empty:
/// the first call of a sizing loop passes no buffer and a zero length.
fn buf_ptr<T>(buf: &mut [T]) -> *mut T {
    if buf.is_empty() {
        std::ptr::null_mut()
    } else {
        buf.as_mut_ptr()
    }
}

/// The account `psid` names. `sid_str` writes the SID for an error, and runs only for one; `field`
/// is the spec field the SID came from, if any.
fn lookup_account_sid(psid: PSID, field: Option<&str>, sid_str: impl FnOnce() -> String) -> Result<String> {
    let mut name_buf: Vec<u16> = Vec::new();
    let mut domain_buf: Vec<u16> = Vec::new();
    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_: i32 = 0;
    // The first call has no buffers and fails with `ERROR_INSUFFICIENT_BUFFER`, naming the lengths
    // it needs; a later call can fail the same way when the account changed in between.
    loop {
        // SAFETY: each buffer is null with a zero length or has exactly the length passed beside it;
        // the length and `use_` out-params are valid lvalues.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                psid,
                buf_ptr(&mut name_buf),
                &mut name_len,
                buf_ptr(&mut domain_buf),
                &mut domain_len,
                &mut use_,
            )
        };
        if ok != 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(account_error(
                format!("resolve account name for {}", subject(field, "SID", &sid_str())),
                err,
            ));
        }
        name_buf.resize(name_len as usize, 0);
        domain_buf.resize(domain_len as usize, 0);
    }
    let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
    let domain = String::from_utf16_lossy(&domain_buf[..domain_len as usize]);
    Ok(if domain.is_empty() {
        name
    } else {
        format!(r"{domain}\{name}")
    })
}

/// What a `SID_NAME_USE` other than a user is, for [`sid_for_account_name`]'s refusal.
#[allow(non_upper_case_globals)] // windows-sys spells `SID_NAME_USE` values in CamelCase.
fn name_use_kind(use_: i32) -> &'static str {
    match use_ {
        SidTypeGroup => "a group",
        SidTypeDomain => "a domain",
        SidTypeAlias => "a local group",
        SidTypeWellKnownGroup => "a well-known group",
        SidTypeDeletedAccount => "a deleted account",
        SidTypeComputer => "a computer",
        SidTypeLabel => "an integrity label",
        SidTypeLogonSession => "a logon session",
        _ => "of no known kind",
    }
}

/// `LookupAccountNameW`: the reverse of [`account_name_from_sid_string`],
/// needed because [`LsaAddAccountRights`] takes a `PSID`, not a name.
/// Returns the raw SID bytes — the pointer `LsaAddAccountRights` needs is
/// `sid_buf.as_mut_ptr()`, valid exactly as long as the returned `Vec` is.
///
/// Strips a leading `.\` (SCM's shorthand for "an account on this
/// machine", accepted by `CreateServiceW`/`ChangeServiceConfigW`) before
/// looking up: `LookupAccountNameW` does not understand it and fails the
/// sizing call with `ERROR_NONE_MAPPED` — confirmed empirically (`.\<local
/// user>` fails, the bare name and a fully domain-qualified name both
/// succeed). A bare local account name resolves against the local machine
/// on its own, so dropping the prefix loses nothing.
///
/// `field` is the spec field `account_name` came from, if any.
pub(crate) fn sid_for_account_name(account_name: &str, field: Option<&str>) -> Result<Vec<u8>> {
    let subject = subject(field, "account", account_name);
    let account_name = account_name.strip_prefix(r".\").unwrap_or(account_name);
    let wide_name = wide_null(account_name).map_err(|e| refused_input(format!("resolve SID for {subject}"), e))?;
    let mut sid_buf: Vec<u8> = Vec::new();
    let mut domain_buf: Vec<u16> = Vec::new();
    let mut sid_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_: i32 = 0;
    // The first call has no buffers and fails with `ERROR_INSUFFICIENT_BUFFER`, naming the lengths
    // it needs; a later call can fail the same way when the account changed in between.
    loop {
        // SAFETY: `wide_name` is a valid null-terminated wide string; each buffer is null with a zero
        // length or has exactly the length passed beside it (bytes for the SID, units for the domain);
        // the length and `use_` out-params are valid lvalues.
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                wide_name.as_ptr(),
                buf_ptr(&mut sid_buf) as PSID,
                &mut sid_len,
                buf_ptr(&mut domain_buf),
                &mut domain_len,
                &mut use_,
            )
        };
        if ok != 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(account_error(format!("resolve SID for {subject}"), err));
        }
        sid_buf.resize(sid_len as usize, 0);
        domain_buf.resize(domain_len as usize, 0);
    }
    // A group, a domain or a machine resolves too, and a `SeServiceLogonRight` granted to one would
    // reach every account in it.
    if use_ != SidTypeUser {
        return Err(Error::AccountLookup {
            detail: format!("{subject} is not a user account (it is {})", name_use_kind(use_)),
            source: None,
        });
    }
    Ok(sid_buf)
}

// SeServiceLogonRight =================================================================================================

fn nt_error(context: &str, status: i32) -> Error {
    // SAFETY: `LsaNtStatusToWinError` takes a plain `NTSTATUS` value and
    // cannot fail.
    let win32 = unsafe { LsaNtStatusToWinError(status) };
    Error::os(context, std::io::Error::from_raw_os_error(win32 as i32))
}

/// Grant `account_name` the right to log on as a service
/// (`SeServiceLogonRight`), via `LsaAddAccountRights`. Idempotent: an
/// account that already has the right is unaffected, so this is safe to
/// call again on a later install of the same account. `field` is the spec field `account_name`
/// came from, if any.
pub fn grant_service_logon_right(account_name: &str, field: Option<&str>) -> Result<()> {
    let sid_buf = sid_for_account_name(account_name, field)?;

    let mut policy_handle: isize = 0;
    let object_attributes = LSA_OBJECT_ATTRIBUTES::default();
    // SAFETY: `object_attributes` is a valid, zeroed `LSA_OBJECT_ATTRIBUTES`
    // (per its own `Default` impl); `systemname: null` targets the local
    // system; `policy_handle` is a valid out-param.
    let status = unsafe {
        LsaOpenPolicy(
            std::ptr::null(),
            &object_attributes,
            (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32,
            &mut policy_handle,
        )
    };
    if status != 0 {
        return Err(nt_error(
            &format!("open the local security policy to grant `{account_name}` SeServiceLogonRight"),
            status,
        ));
    }

    // `LSA_UNICODE_STRING::Length`/`MaximumLength` are byte counts.
    let right_text = w!("SeServiceLogonRight");
    // SAFETY: `w!` yields a NUL-terminated static string.
    let right_len = unsafe { units_until_nul(right_text) }.len();
    let right = LSA_UNICODE_STRING {
        Length: (right_len * 2) as u16,
        MaximumLength: ((right_len + 1) * 2) as u16,
        Buffer: right_text as *mut u16,
    };

    // SAFETY: `policy_handle` was just opened with `POLICY_CREATE_ACCOUNT`;
    // `sid_buf` is a valid `PSID` buffer live for this call; `right` points
    // into `right_text`, which outlives this call.
    let add_status = unsafe { LsaAddAccountRights(policy_handle, sid_buf.as_ptr() as PSID, &right, 1) };

    // SAFETY: `policy_handle` was successfully opened above and is closed
    // exactly once, after its last use.
    unsafe {
        LsaClose(policy_handle);
    }

    if add_status != 0 {
        return Err(nt_error(
            &format!("grant `{account_name}` SeServiceLogonRight"),
            add_status,
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod identity_tests;
