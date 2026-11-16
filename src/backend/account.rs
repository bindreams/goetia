//! The Unix account a daemon runs as, resolved from a [`User`] through the passwd database: whom
//! `install` hands a directory to so the daemon can use it.

use nix::unistd::{Gid, Uid};

use super::created_dirs::Owner;
use crate::error::{Error, Result};
use crate::spec::{AccountId, User};

/// An account, resolved.
#[derive(Debug)]
pub(crate) struct Account {
    /// Its name.
    // Read where a backend names the account to its manager (launchd's `UserName`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) name: String,
    pub(crate) uid: Uid,
    /// Its primary group.
    pub(crate) gid: Gid,
}

impl Account {
    /// Whom a directory is handed to for this account: nobody for root, whose directory it already is
    /// (`chown(0, 0)` would take its group away).
    pub(crate) fn owner(&self) -> Result<Option<Owner>> {
        if self.uid.is_root() {
            return Ok(None);
        }
        Owner::new(self.uid, Some(self.gid)).map(Some)
    }
}

impl From<nix::unistd::User> for Account {
    fn from(user: nix::unistd::User) -> Self {
        Account {
            name: user.name,
            uid: user.uid,
            gid: user.gid,
        }
    }
}

/// Resolve `user` to an account. `User::Root` is uid 0, and a SID names no Unix account. A name is looked up
/// as a name, and an account with no passwd entry is an error, a uid included: launchd resolves the account
/// by name when it starts the job and `launchd.plist(5)` does not say it accepts a number.
pub(crate) fn resolve_account(user: &User) -> Result<Account> {
    match user {
        User::Root => by_uid(0),
        User::Id(AccountId::Uid(uid)) => by_uid(*uid),
        User::Name(name) => {
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

/// The account with passwd entry `uid`, if there is one.
pub(crate) fn find_uid(uid: u32) -> Result<Option<Account>> {
    let found = nix::unistd::User::from_uid(Uid::from_raw(uid)).map_err(|e| Error::AccountLookup {
        detail: format!("look up uid {uid}"),
        source: Some(e.into()),
    })?;
    Ok(found.map(Account::from))
}

fn by_uid(uid: u32) -> Result<Account> {
    find_uid(uid)?.ok_or_else(|| Error::AccountLookup {
        detail: format!("no such uid {uid} on this host"),
        source: None,
    })
}

/// An account as an error names it: its name where it has a passwd entry, and always its uid.
pub(crate) fn describe_uid(uid: u32) -> String {
    match by_uid(uid) {
        Ok(account) => format!("`{}` (uid {uid})", account.name),
        Err(_) => format!("uid {uid}"),
    }
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod account_tests;
