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

use std::os::windows::ffi::OsStrExt as _;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authentication::Identity::{
    LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{LookupAccountNameW, LookupAccountSidW, PSID};

use crate::backend::Identity;
use crate::error::{Error, Result};
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
        User::Id(AccountId::Uid(uid)) => Err(Error::Other(format!(
            "user.id `{uid}` is a numeric UID, which Windows SCM has no account mapping for; use `user.name` \
             (an account name) or `user.id` with a SID string instead"
        ))),
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
        Err(std::env::VarError::NotUnicode(_)) => Err(Error::Other(
            "GOETIA_SERVICE_PASSWORD is set but is not valid Unicode".to_string(),
        )),
    }
}

/// Whether `account` is one Windows will start without a password: the
/// built-in `LocalSystem`/`LocalService`/`NetworkService` accounts (bare, or
/// `NT AUTHORITY\`-qualified), or a virtual per-service account
/// (`NT SERVICE\<name>`) — the exact vocabulary the design spec's "Windows
/// accounts" section names. Any other resolved account name is assumed to be
/// a real user account, which does.
///
/// A heuristic on the account *name* rather than a `LookupAccountSid`-based
/// well-known-SID check: `ServiceInfo`/`ChangeServiceConfigW` only ever see
/// the name (`reg.account`, from `generate::registration`/`Identity`), and
/// every one of these accounts is required to be named as such by SCM's own
/// conventions — there is no other spelling a caller could use for them.
pub fn account_needs_password(account: &str) -> bool {
    let lower = account.to_ascii_lowercase();
    let unqualified = lower.strip_prefix(r"nt authority\").unwrap_or(&lower);
    !(lower.starts_with(r"nt service\")
        // "system" alongside "localsystem": `LookupAccountSidW` resolves
        // LocalSystem's well-known SID (S-1-5-18) to `NT AUTHORITY\SYSTEM`,
        // not the literal string "LocalSystem" `CreateServiceW` also
        // accepts — `user.id`'s SID path (`account_name_from_sid_string`)
        // produces the former.
        || matches!(unqualified, "localsystem" | "localservice" | "networkservice" | "system"))
}

fn wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_error(context: &str) -> Error {
    Error::Other(format!("{context}: {}", std::io::Error::last_os_error()))
}

// SID <-> account name ================================================================================================

/// `ConvertStringSidToSidW` + `LookupAccountSidW`: turn a `user.id` SID
/// string into `DOMAIN\Name` (or bare `Name` for an unqualified/well-known
/// SID), which is what `ServiceInfo::account_name` needs — SCM has no SID
/// field.
fn account_name_from_sid_string(sid_str: &str) -> Result<String> {
    let wide_sid = wide_null(sid_str);
    let mut psid: PSID = std::ptr::null_mut();
    // SAFETY: `wide_sid` is a valid null-terminated wide string; `psid` is a
    // valid out-param. On success, `psid` must be freed with `LocalFree`.
    let ok = unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut psid) };
    if ok == 0 {
        return Err(last_error(&format!("user.id `{sid_str}` is not a valid SID string")));
    }
    let result = lookup_account_sid(psid, sid_str);
    // SAFETY: `psid` was allocated by `ConvertStringSidToSidW` above and is
    // freed exactly once, after every use of it above has completed.
    unsafe {
        LocalFree(psid);
    }
    result
}

fn lookup_account_sid(psid: PSID, sid_str: &str) -> Result<String> {
    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_: i32 = 0;
    // SAFETY: sizing call — every buffer pointer is null, every length
    // out-param is a valid `u32` lvalue. Expected to fail with
    // `ERROR_INSUFFICIENT_BUFFER`; any other failure is real.
    let sizing_ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            psid,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut use_,
        )
    };
    if sizing_ok != 0 {
        return Err(Error::Other(format!(
            "LookupAccountSidW for `{sid_str}` unexpectedly succeeded on a zero-sized buffer"
        )));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(Error::Other(format!("resolve account name for SID `{sid_str}`: {err}")));
    }

    let mut name_buf = vec![0u16; name_len as usize];
    let mut domain_buf = vec![0u16; domain_len as usize];
    // SAFETY: buffers are sized exactly to the lengths the sizing call
    // reported; the length out-params are re-passed as valid lvalues.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            psid,
            name_buf.as_mut_ptr(),
            &mut name_len,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    if ok == 0 {
        return Err(last_error(&format!("resolve account name for SID `{sid_str}`")));
    }
    let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
    let domain = String::from_utf16_lossy(&domain_buf[..domain_len as usize]);
    Ok(if domain.is_empty() {
        name
    } else {
        format!(r"{domain}\{name}")
    })
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
fn sid_for_account_name(account_name: &str) -> Result<Vec<u8>> {
    let account_name = account_name.strip_prefix(r".\").unwrap_or(account_name);
    let wide_name = wide_null(account_name);
    let mut sid_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_: i32 = 0;
    // SAFETY: sizing call — see `lookup_account_sid`.
    let sizing_ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide_name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut use_,
        )
    };
    if sizing_ok != 0 {
        return Err(Error::Other(format!(
            "LookupAccountNameW for `{account_name}` unexpectedly succeeded on a zero-sized buffer"
        )));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(Error::Other(format!("resolve SID for account `{account_name}`: {err}")));
    }

    let mut sid_buf = vec![0u8; sid_len as usize];
    let mut domain_buf = vec![0u16; domain_len as usize];
    // SAFETY: `sid_buf` is sized exactly to `sid_len` as a `PSID` out-buffer;
    // `domain_buf` likewise for the domain name.
    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide_name.as_ptr(),
            sid_buf.as_mut_ptr() as PSID,
            &mut sid_len,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    if ok == 0 {
        return Err(last_error(&format!("resolve SID for account `{account_name}`")));
    }
    Ok(sid_buf)
}

// SeServiceLogonRight =================================================================================================

fn nt_error(context: &str, status: i32) -> Error {
    // SAFETY: `LsaNtStatusToWinError` takes a plain `NTSTATUS` value and
    // cannot fail.
    let win32 = unsafe { LsaNtStatusToWinError(status) };
    Error::Other(format!(
        "{context}: {}",
        std::io::Error::from_raw_os_error(win32 as i32)
    ))
}

/// Grant `account_name` the right to log on as a service
/// (`SeServiceLogonRight`), via `LsaAddAccountRights`. Idempotent: an
/// account that already has the right is unaffected, so this is safe to
/// call on every install, not just the first one for a given account.
pub fn grant_service_logon_right(account_name: &str) -> Result<()> {
    let sid_buf = sid_for_account_name(account_name)?;

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

    let right_text = wide_null("SeServiceLogonRight");
    // `LSA_UNICODE_STRING::Length`/`MaximumLength` are byte counts,
    // excluding the trailing NUL `wide_null` appended for other APIs' sake.
    let right = LSA_UNICODE_STRING {
        Length: ((right_text.len() - 1) * 2) as u16,
        MaximumLength: (right_text.len() * 2) as u16,
        Buffer: right_text.as_ptr() as *mut u16,
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
