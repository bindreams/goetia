//! The Unix account a daemon runs as, resolved from a [`User`] through the passwd database: whom
//! `install` hands a directory to so the daemon can use it.

use nix::unistd::{Gid, Uid};

use super::created_dirs::Owner;
use crate::error::{Error, Result};
use crate::spec::{AccountId, User};

/// What an account with no passwd entry is: a uid or a number given as a name that no entry owns.
// Each backend names one variant, so on one OS the other is never constructed.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum Unlisted {
    /// An error. launchd resolves the account by name when it starts the job and `launchd.plist(5)`
    /// does not say it accepts a number, so a number with no entry is no account there.
    Refuse,
    /// The number, with no name and no group. systemd takes `User=4242` with no entry, and `chown`
    /// accepts a bare number: the daemon runs, so what it works in must be its. A name that is a number
    /// is a uid before it is a name, as systemd reads it: `get_user_creds` in `src/basic/user-util.c`
    /// calls `parse_uid` and then only `getpwuid`, and asks `getpwnam` for a string that is no uid.
    AsNumber,
}

/// An account, resolved.
#[derive(Debug)]
pub(crate) struct Account {
    /// Its name; the number as text for one taken [`AsNumber`](Unlisted::AsNumber).
    // Read where a backend names the account to its manager (launchd's `UserName`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) name: String,
    pub(crate) uid: Uid,
    /// Its primary group, which an entry gives and a bare number does not.
    pub(crate) gid: Option<Gid>,
}

impl Account {
    /// Whom a directory is handed to for this account: nobody for root, whose directory it already is
    /// (`chown(0, 0)` would take its group away).
    pub(crate) fn owner(&self) -> Result<Option<Owner>> {
        if self.uid.is_root() {
            return Ok(None);
        }
        Owner::new(self.uid, self.gid).map(Some)
    }

    fn number(uid: u32) -> Self {
        Account {
            name: uid.to_string(),
            uid: Uid::from_raw(uid),
            gid: None,
        }
    }
}

impl From<nix::unistd::User> for Account {
    fn from(user: nix::unistd::User) -> Self {
        Account {
            name: user.name,
            uid: user.uid,
            gid: Some(user.gid),
        }
    }
}

/// Resolve `user` to an account. `User::Root` is uid 0, and a SID names no Unix account.
pub(crate) fn resolve_account(user: &User, unlisted: Unlisted) -> Result<Account> {
    match user {
        User::Root => by_uid(0, unlisted),
        User::Id(AccountId::Uid(uid)) => by_uid(*uid, unlisted),
        User::Name(name) => {
            // systemd reads a name that is a uid as the uid, and never looks it up as a name.
            if matches!(unlisted, Unlisted::AsNumber)
                && let Some(uid) = systemd_uid(name)
            {
                return by_uid(uid, unlisted);
            }
            let found = nix::unistd::User::from_name(name).map_err(|e| Error::AccountLookup {
                detail: format!("look up user `{name}`"),
                source: Some(e.into()),
            })?;
            match found {
                Some(found) => Ok(found.into()),
                None => Err(Error::AccountLookup {
                    detail: format!("no such user `{name}` on this host"),
                    source: None,
                }),
            }
        }
        User::Id(AccountId::Sid(sid)) => Err(Error::AccountLookup {
            detail: format!("user `{{id: {sid}}}` names a Windows SID, which is not meaningful on Unix"),
            source: None,
        }),
    }
}

/// `name` as a uid, if it is the canonical decimal spelling of one: digits only, no sign, and no
/// leading zero except `0` itself. Any other spelling is a name, which no account may have.
fn canonical_uid(name: &str) -> Option<u32> {
    let canonical = name.bytes().all(|b| b.is_ascii_digit()) && (name == "0" || !name.starts_with('0'));
    canonical.then(|| name.parse().ok()).flatten()
}

/// `name` as systemd's `parse_uid` reads it: a canonical number that is a valid uid, which excludes
/// 0xFFFF and 0xFFFFFFFF (`uid_is_valid`). Anything else is a name to systemd.
fn systemd_uid(name: &str) -> Option<u32> {
    canonical_uid(name).filter(|&uid| uid != 0xFFFF && uid != u32::MAX)
}

fn by_uid(uid: u32, unlisted: Unlisted) -> Result<Account> {
    let found = nix::unistd::User::from_uid(Uid::from_raw(uid)).map_err(|e| Error::AccountLookup {
        detail: format!("look up uid {uid}"),
        source: Some(e.into()),
    })?;
    match (found, unlisted) {
        (Some(found), _) => Ok(found.into()),
        (None, Unlisted::AsNumber) => Ok(Account::number(uid)),
        (None, Unlisted::Refuse) => Err(Error::AccountLookup {
            detail: format!("no such uid {uid} on this host"),
            source: None,
        }),
    }
}

/// An account as an error names it: its name where it has a passwd entry, and always its uid.
pub(crate) fn describe_uid(uid: u32) -> String {
    match by_uid(uid, Unlisted::Refuse) {
        Ok(account) => format!("`{}` (uid {uid})", account.name),
        Err(_) => format!("uid {uid}"),
    }
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod account_tests;
