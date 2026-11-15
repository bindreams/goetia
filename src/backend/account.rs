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
    /// accepts a bare number: the daemon runs, so what it works in must be its.
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
    /// Whom a directory is handed to for this account.
    pub(crate) fn owner(&self) -> Result<Owner> {
        Owner::new(self.uid, self.gid)
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
            let found = nix::unistd::User::from_name(name).map_err(|e| Error::AccountLookup {
                detail: format!("look up user `{name}`"),
                source: Some(e.into()),
            })?;
            match (found, unlisted, name.parse::<u32>()) {
                (Some(found), ..) => Ok(found.into()),
                (None, Unlisted::AsNumber, Ok(uid)) => Ok(Account::number(uid)),
                (None, ..) => Err(Error::AccountLookup {
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

#[cfg(test)]
#[path = "account_tests.rs"]
mod account_tests;
