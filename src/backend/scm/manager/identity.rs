//! Windows account resolution: what goetia decides from what the account database says.
//!
//! Identity resolution is effectful (`spec::User -> backend::Identity`): a
//! Windows SID needs `LookupAccountSidW`, so it cannot happen inside the
//! pure generator (`generate::registration`) — see the crate-level design
//! notes on this (`backend::Identity`'s doc comment).
//!
//! The database is [`Accounts`]. Its lookups fail with the OS error; the functions here turn it into the
//! error the user reads, which names the manifest field the value came from. **A real account needs a
//! logon right**: see [`Lsa::grant`](super::accounts::Lsa). `LocalSystem` is exempt (see `resolve`'s caller in
//! `manager.rs`).

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_INVALID_SID, ERROR_NONE_MAPPED, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::PSID;

use super::accounts::{Account, AccountKind, Accounts};
use crate::backend::Identity;
use crate::backend::scm::sid::{Sid, copy_sid};
use crate::backend::scm::task_xml::builtin_sid;
use crate::backend::scm::wide::wide_null;
use crate::error::{Error, Result};
use crate::spec::{AccountId, User, windows_builtin};

// resolve =============================================================================================================

/// `spec::User -> backend::Identity`, per the design spec's "Windows
/// accounts" section. `User::Root` resolves to an empty (unused) `user`:
/// `generate::registration` maps `User::Root` to `account: None`
/// (`LocalSystem`) without consulting `Identity` at all, so nothing here
/// needs to do a lookup for it.
pub fn resolve(accounts: &dyn Accounts, user: &User) -> Result<Identity> {
    match user {
        User::Root => Ok(Identity { user: String::new() }),
        User::Name(name) => Ok(Identity { user: name.clone() }),
        User::Id(AccountId::Sid(sid)) => Ok(Identity {
            user: account_name_from_sid_string(accounts, sid)?,
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

/// The decision [`service_password`] makes, as a pure function of the lookup result.
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
/// or under `noun` when it did not come from one. `value` is plain; the report escapes it.
fn subject(field: Option<&str>, noun: &str, value: &str) -> String {
    match field {
        Some(field) => format!("`{field}` `{value}`"),
        None => format!("{noun} `{value}`"),
    }
}

/// The failure of a Win32 account lookup that was doing `action`. An input the OS cannot take (no
/// such account, a SID it does not accept, a string that is not a SID) or that was refused before any call (a
/// value that holds a NUL: [`std::io::ErrorKind::InvalidInput`] with no OS code) is an
/// [`Error::AccountLookup`]; any other failure (`ERROR_NOT_ENOUGH_MEMORY`) is an [`Error::Os`].
fn account_error(action: String, err: std::io::Error) -> Error {
    let refused = err.kind() == std::io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
    let input = refused
        || [ERROR_NONE_MAPPED, ERROR_INVALID_SID, ERROR_INVALID_PARAMETER]
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

// SID <-> account name ================================================================================================

/// The account `name` names, looked up in `accounts`: `field` is the spec field `name` came from, if any.
fn lookup_name(accounts: &dyn Accounts, name: &str, field: Option<&str>) -> Result<Account> {
    accounts
        .by_name(name)
        .map_err(|e| account_error(format!("resolve SID for {}", subject(field, "account", name)), e))
}

/// `ConvertStringSidToSidW` + the account of that SID: turn a `user.id` SID
/// string into `DOMAIN\Name` (or bare `Name` for an unqualified/well-known
/// SID), which is what `ServiceInfo::account_name` needs — SCM has no SID
/// field.
fn account_name_from_sid_string(accounts: &dyn Accounts, sid_str: &str) -> Result<String> {
    resolve_sid_string(accounts, sid_str).map(|account| account.name)
}

/// The account a `user.id` string spells.
fn resolve_sid_string(accounts: &dyn Accounts, sid_str: &str) -> Result<Account> {
    let sid = sid_from_string(sid_str, Some("user.id"))?;
    accounts.by_sid(&sid).map_err(|e| {
        account_error(
            format!("resolve account name for {}", subject(Some("user.id"), "SID", sid_str)),
            e,
        )
    })
}

/// The SID that `sid_str` spells (`S-1-5-32-544`), by `ConvertStringSidToSidW`: no account is looked up,
/// so the SID of a deleted account is still a SID. `field` is the spec field it came from, if any.
fn sid_from_string(sid_str: &str, field: Option<&str>) -> Result<Sid> {
    let parse = || format!("parse {} as a SID string", subject(field, "SID", sid_str));
    let wide_sid = wide_null(sid_str).map_err(|e| account_error(parse(), e.into()))?;
    let mut psid: PSID = std::ptr::null_mut();
    // SAFETY: `wide_sid` is a valid null-terminated wide string; `psid` is a
    // valid out-param. On success, `psid` must be freed with `LocalFree`.
    let ok = unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut psid) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        return Err(account_error(parse(), err));
    }
    // SAFETY: `ConvertStringSidToSidW` succeeded, so `psid` is a whole SID.
    let sid = unsafe { copy_sid(psid) };
    // SAFETY: `psid` was allocated by `ConvertStringSidToSidW` above and is
    // freed exactly once, after its last use above.
    unsafe {
        LocalFree(psid);
    }
    Ok(sid.expect("ConvertStringSidToSidW writes a valid SID"))
}

/// The SID of the account a task's stored principal names, as Task Scheduler stores it: a SID string as
/// the SID it spells, anything else as an account name, looked up. An error when a name names no account.
pub(in crate::backend::scm) fn sid_of_principal(accounts: &dyn Accounts, principal: &str) -> Result<Sid> {
    if principal.starts_with("S-1-") {
        sid_from_string(principal, None)
    } else {
        lookup_name(accounts, principal, None).map(|account| account.sid)
    }
}

/// The SID of the account a service registered as `account`
/// ([`ScmRegistration::account`](crate::backend::scm::generate::ScmRegistration::account)) runs as: `None` is LocalSystem and a recognised built-in is its well-known SID, neither of which
/// `LookupAccountNameW` resolves by the spelling goetia registers; any other account is looked up. A
/// `written` `user.id` is the SID the spec gave, which the logon gate used too: the name it registers is not
/// looked up again. A virtual account (`NT SERVICE\<id>`) resolves only once the service exists, so call this
/// after it is created. A real user's SID is the one [`require_user_account`] found, which the caller keeps.
pub(in crate::backend::scm) fn service_account_sid(
    accounts: &dyn Accounts,
    account: Option<&str>,
    written: Option<Written>,
) -> Result<Sid> {
    if let Some(Written::Sid(sid)) = written {
        return sid_from_string(sid, Some("user.id"));
    }
    let account = account.unwrap_or_default();
    match windows_builtin(account) {
        Some(builtin) => sid_from_string(builtin_sid(builtin), None),
        None => lookup_name(accounts, account, None).map(|account| account.sid),
    }
}

// user accounts only ==================================================================================================

/// The SID of an account [`require_user_account`] found to be a user account. Only that function
/// builds one, so a grant that takes one acts on the SID that was checked.
#[derive(Debug)]
pub struct UserSid {
    sid: Sid,
    /// The name of the account, for the grant's errors.
    account: String,
}

impl UserSid {
    /// The SID of the account.
    pub fn sid(&self) -> &Sid {
        &self.sid
    }

    /// The name of the account, as it was asked for.
    pub fn account(&self) -> &str {
        &self.account
    }
}

/// The account of a spec, as the user wrote it.
#[derive(Clone, Copy, Debug)]
pub enum Written<'a> {
    /// `user.name`.
    Name(&'a str),
    /// `user.id`, a SID string.
    Sid(&'a str),
}

/// Refuses `account` unless it names a user account, and returns the SID it found. A
/// `SeServiceLogonRight` granted to a group, a domain or a machine would reach every account in it,
/// and the right is what installing a service that runs as `account` grants. `written` is what the
/// user wrote, for the refusal to quote; `None` names `account`. A `user.id` is resolved from the
/// SID string, so its kind and its SID are those of the SID the user wrote, not of whatever
/// `account` resolves to by name; any other `account` is looked up by name. Either way it changes
/// nothing.
pub fn require_user_account(accounts: &dyn Accounts, account: &str, written: Option<Written>) -> Result<UserSid> {
    let (field, value) = match written {
        Some(Written::Name(name)) => (Some("user.name"), name),
        Some(Written::Sid(sid)) => (Some("user.id"), sid),
        None => (None, account),
    };
    let found = match written {
        Some(Written::Sid(sid_str)) => resolve_sid_string(accounts, sid_str)?,
        _ => lookup_name(accounts, account, field)?,
    };
    if found.kind == AccountKind::User {
        return Ok(UserSid {
            sid: found.sid,
            account: found.name,
        });
    }
    Err(Error::AccountLookup {
        detail: format!(
            "{} is not a user account (it is {})",
            subject(field, "account", value),
            found.kind.describe()
        ),
        source: None,
    })
}

// Logon rights ========================================================================================================

/// A logon right an install grants the account its daemon runs as: what the account must hold for
/// Windows to start the daemon under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    /// To log on as a service: a service run as a real account.
    Service,
    /// To log on as a batch job: a Task Scheduler task run as a real account. Without it a task's
    /// first start starts nothing and raises nothing.
    Batch,
}

impl Right {
    /// The right's name, as `LsaAddAccountRights` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Right::Service => "SeServiceLogonRight",
            Right::Batch => "SeBatchLogonRight",
        }
    }
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod identity_tests;
