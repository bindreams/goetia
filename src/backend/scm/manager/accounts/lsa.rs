//! The real account database: `LookupAccountNameW`, `LookupAccountSidW`, and `LsaAddAccountRights` on the
//! local security policy.
//!
//! **A real account needs a logon right.** `CreateServiceW` succeeds without `SeServiceLogonRight`, and the
//! service's first start then fails with error 1069 (`ERROR_LOGON_FAILURE`) -- install reports success, boot
//! fails silently. A task's first start without `SeBatchLogonRight` starts nothing and raises nothing.
//! `sc.exe` does not grant either. [`Lsa::grant`] grants one idempotently (adding a right an account already
//! has is not an error); `LocalSystem` is exempt, and neither reference repo (`hole`, `wsm`) has solved this
//! for any other account.

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::Security::Authentication::Identity::{
    LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows_sys::Win32::Security::{LookupAccountNameW, LookupAccountSidW, PSID};

use super::{Account, AccountKind, Accounts};
use crate::backend::scm::manager::identity::{Right, UserSid};
use crate::backend::scm::sid::{Sid, is_whole_sid};
use crate::backend::scm::wide::wide_null;
use crate::error::{Error, Result};

/// The accounts of this machine, as Windows answers for them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Lsa;

impl Accounts for Lsa {
    /// `LookupAccountNameW`. It refuses nothing by kind: a group or a domain resolves too.
    ///
    /// Strips a leading `.\` (SCM's shorthand for "an account on this machine", accepted by
    /// `CreateServiceW`/`ChangeServiceConfigW`) before looking up: `LookupAccountNameW` does not understand it
    /// and fails the sizing call with `ERROR_NONE_MAPPED` -- confirmed empirically (`.\<local user>` fails,
    /// the bare name and a fully domain-qualified name both succeed). A bare local account name resolves
    /// against the local machine on its own, so dropping the prefix loses nothing.
    ///
    /// A name with a NUL is refused with [`std::io::ErrorKind::InvalidInput`], and no call is made.
    fn by_name(&self, name: &str) -> std::io::Result<Account> {
        let wide_name = wide_null(name.strip_prefix(r".\").unwrap_or(name))?;
        let mut use_ = 0;
        let (bytes, _domain) = lookup_growing(|sid, sid_len, domain, domain_len| {
            // SAFETY: `wide_name` is a valid null-terminated wide string; each buffer is null with a zero
            // length or has exactly the length passed beside it (bytes for the SID, units for the domain);
            // the length and `use_` out-params are valid lvalues.
            win32_status(unsafe {
                LookupAccountNameW(
                    std::ptr::null(),
                    wide_name.as_ptr(),
                    sid as PSID,
                    sid_len,
                    domain,
                    domain_len,
                    &mut use_,
                )
            })
        })?;
        let sid =
            whole_sid_prefix(bytes).ok_or_else(|| std::io::Error::other("the SID it wrote is not a whole SID"))?;
        Ok(Account {
            sid,
            name: name.to_string(),
            kind: AccountKind::of(use_),
        })
    }

    /// `LookupAccountSidW`. Bytes that are not a whole SID are refused with
    /// [`std::io::ErrorKind::InvalidInput`], and no call is made: the lookup would read past a short slice to
    /// find out.
    fn by_sid(&self, sid: &Sid) -> std::io::Result<Account> {
        if !is_whole_sid(sid) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{sid:?} is not a SID"),
            ));
        }
        let mut use_ = 0;
        let (name, domain) = lookup_growing(|name, name_len, domain, domain_len| {
            // SAFETY: `sid` is a whole SID; each buffer is null with a zero length or has exactly the length
            // passed beside it; the length and `use_` out-params are valid lvalues.
            win32_status(unsafe {
                LookupAccountSidW(
                    std::ptr::null(),
                    sid.as_ptr() as PSID,
                    name,
                    name_len,
                    domain,
                    domain_len,
                    &mut use_,
                )
            })
        })?;
        let name = String::from_utf16_lossy(&name);
        let domain = String::from_utf16_lossy(&domain);
        let name = if domain.is_empty() {
            name
        } else {
            format!(r"{domain}\{name}")
        };
        Ok(Account {
            sid: sid.clone(),
            name,
            kind: AccountKind::of(use_),
        })
    }

    /// `LsaAddAccountRights`. Idempotent: an account that already has the right is unaffected, so this is safe
    /// to call again on a later install of the same account.
    fn grant(&self, user: &UserSid, right: Right) -> Result<()> {
        let (sid_buf, account_name) = (user.sid(), user.account());
        let right_name = right.name();

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
                &format!("open the local security policy to grant `{account_name}` {right_name}"),
                status,
            ));
        }

        // `LSA_UNICODE_STRING::Length`/`MaximumLength` are byte counts.
        let mut right_text: Vec<u16> = right_name.encode_utf16().collect();
        let right_len = right_text.len();
        let right = LSA_UNICODE_STRING {
            Length: (right_len * 2) as u16,
            MaximumLength: (right_len * 2) as u16,
            Buffer: right_text.as_mut_ptr(),
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
            return Err(nt_error(&format!("grant `{account_name}` {right_name}"), add_status));
        }
        Ok(())
    }
}

fn nt_error(context: &str, status: i32) -> Error {
    // SAFETY: `LsaNtStatusToWinError` takes a plain `NTSTATUS` value and
    // cannot fail.
    let win32 = unsafe { LsaNtStatusToWinError(status) };
    Error::os(context, std::io::Error::from_raw_os_error(win32 as i32))
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

/// Runs a Win32 account lookup that fills two buffers, until it has both. `call` gets each buffer
/// (null while empty) with its length beside it, which it lowers to what it wrote on success or
/// raises to what it needs on `ERROR_INSUFFICIENT_BUFFER`. A lookup that needs more grows the buffers to the
/// lengths it reported, and one whose lengths did not grow would change nothing, so that is its own
/// `ERROR_INSUFFICIENT_BUFFER` error. A success whose lengths do not fit the buffers, or whose first buffer is
/// empty, is an [`std::io::Error::other`]: a lookup always writes its name or SID. A failure of any other kind is
/// `call`'s error as it came. The buffers come back cut to the lengths written.
fn lookup_growing<T: Copy + Default>(
    mut call: impl FnMut(*mut T, &mut u32, *mut u16, &mut u32) -> std::io::Result<()>,
) -> std::io::Result<(Vec<T>, Vec<u16>)> {
    let mut first: Vec<T> = Vec::new();
    let mut domain: Vec<u16> = Vec::new();
    loop {
        let mut first_len = first.len() as u32;
        let mut domain_len = domain.len() as u32;
        match call(
            buf_ptr(&mut first),
            &mut first_len,
            buf_ptr(&mut domain),
            &mut domain_len,
        ) {
            Ok(()) => {
                let (first_len, domain_len) = (first_len as usize, domain_len as usize);
                if first_len == 0 || first_len > first.len() || domain_len > domain.len() {
                    return Err(std::io::Error::other(format!(
                        "unexpected lengths {first_len} and {domain_len} for buffers of {} and {}",
                        first.len(),
                        domain.len()
                    )));
                }
                first.truncate(first_len);
                domain.truncate(domain_len);
                return Ok((first, domain));
            }
            Err(err) if err.raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER as i32) => {
                let (needed_first, needed_domain) = (first_len as usize, domain_len as usize);
                if needed_first <= first.len() && needed_domain <= domain.len() {
                    return Err(err);
                }
                // A buffer never shrinks: a lookup that reports a smaller length for one buffer than
                // it was given still needs the other grown.
                first.resize(needed_first.max(first.len()), T::default());
                domain.resize(needed_domain.max(domain.len()), 0);
            }
            Err(err) => return Err(err),
        }
    }
}

/// The result of a Win32 call whose return value is `0` on failure, with the thread's last error.
fn win32_status(ok: i32) -> std::io::Result<()> {
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The SID `bytes` start with, cut to the length its own sub-authority count gives. `None` when
/// `bytes` is not a whole SID, or is shorter than that.
fn whole_sid_prefix(mut bytes: Vec<u8>) -> Option<Sid> {
    let len = 8 + 4 * usize::from(*bytes.get(1)?);
    if len > bytes.len() {
        return None;
    }
    bytes.truncate(len);
    is_whole_sid(&bytes).then_some(bytes)
}

#[cfg(test)]
#[path = "lsa_tests.rs"]
mod lsa_tests;
