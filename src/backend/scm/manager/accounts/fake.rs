//! A test account database: a table of the answers a test stated, and the grants it was asked for, and nothing
//! else. It models no Windows (it does not strip `.\`, fold case or know any account on its own).
//!
//! A name or SID no test stated is recorded (in [`Fake::asked`]) and panics, so that a function that must ask nothing is shown to
//! have asked nothing. "No such account" and a failure of another kind are answers a test states.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use windows_sys::Win32::Foundation::ERROR_NONE_MAPPED;

use super::{Account, AccountKind, Accounts};
use crate::backend::scm::manager::identity::{Right, UserSid};
use crate::backend::scm::sid::Sid;
use crate::error::Result;

/// One lookup the code under test made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Asked {
    Name(String),
    Sid(Sid),
}

/// A stated failure, which an [`io::Error`] (not `Clone`) is made from at each ask.
#[derive(Clone)]
struct Failure {
    code: Option<i32>,
    kind: ErrorKind,
    message: String,
}

impl Failure {
    fn of(error: &io::Error) -> Self {
        Self {
            code: error.raw_os_error(),
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn error(&self) -> io::Error {
        match self.code {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::new(self.kind, self.message.clone()),
        }
    }
}

#[derive(Default)]
struct Script {
    by_name: HashMap<String, std::result::Result<Account, Failure>>,
    by_sid: HashMap<Sid, std::result::Result<Account, Failure>>,
    asked: Vec<Asked>,
    grants: Vec<(Sid, Right)>,
}

/// The test [`Accounts`]. Clones share one script, so a test keeps a handle after the manager takes one.
#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<Script>>);

/// Poison is recovered from: the `Fake` panics for an unstated ask, and what is asked after it must still be
/// recorded.
fn lock(script: &Mutex<Script>) -> MutexGuard<'_, Script> {
    script.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fake {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The account `name` is, by name and by SID, of `kind`: `by_name(name)` answers `sid` with `name` as
    /// asked, and `by_sid(sid)` answers `name`.
    pub(crate) fn account(self, name: &str, sid: &Sid, kind: AccountKind) -> Self {
        let account = Account {
            sid: sid.clone(),
            name: name.to_string(),
            kind,
        };
        {
            let mut script = lock(&self.0);
            script.by_name.insert(name.to_string(), Ok(account.clone()));
            script.by_sid.insert(sid.clone(), Ok(account));
        }
        self
    }

    /// [`Self::account`] of a user.
    pub(crate) fn user(self, name: &str, sid: &Sid) -> Self {
        self.account(name, sid, AccountKind::User)
    }

    /// `by_name(name)` finds no account.
    pub(crate) fn no_such_account(self, name: &str) -> Self {
        self.fails(name, io::Error::from_raw_os_error(ERROR_NONE_MAPPED as i32))
    }

    /// `by_sid(sid)` finds no account.
    pub(crate) fn no_such_sid(self, sid: &Sid) -> Self {
        self.fails_sid(sid, io::Error::from_raw_os_error(ERROR_NONE_MAPPED as i32))
    }

    /// `by_name(name)` fails with `error`.
    pub(crate) fn fails(self, name: &str, error: io::Error) -> Self {
        lock(&self.0).by_name.insert(name.to_string(), Err(Failure::of(&error)));
        self
    }

    /// `by_sid(sid)` fails with `error`.
    pub(crate) fn fails_sid(self, sid: &Sid, error: io::Error) -> Self {
        lock(&self.0).by_sid.insert(sid.clone(), Err(Failure::of(&error)));
        self
    }

    /// The lookups made so far, in order, unstated ones included.
    pub(crate) fn asked(&self) -> Vec<Asked> {
        lock(&self.0).asked.clone()
    }

    /// The grants made so far, in order: the SID and the right.
    pub(crate) fn granted(&self) -> Vec<(Sid, Right)> {
        lock(&self.0).grants.clone()
    }
}

impl Accounts for Fake {
    fn by_name(&self, name: &str) -> io::Result<Account> {
        let mut script = lock(&self.0);
        script.asked.push(Asked::Name(name.to_string()));
        match script.by_name.get(name) {
            Some(Ok(account)) => Ok(account.clone()),
            Some(Err(failure)) => Err(failure.error()),
            None => {
                drop(script);
                panic!("no answer stated for the account named {name:?}");
            }
        }
    }

    fn by_sid(&self, sid: &Sid) -> io::Result<Account> {
        let mut script = lock(&self.0);
        script.asked.push(Asked::Sid(sid.clone()));
        match script.by_sid.get(sid) {
            Some(Ok(account)) => Ok(account.clone()),
            Some(Err(failure)) => Err(failure.error()),
            None => {
                drop(script);
                panic!("no answer stated for the account with SID {sid:?}");
            }
        }
    }

    fn grant(&self, user: &UserSid, right: Right) -> Result<()> {
        lock(&self.0).grants.push((user.sid().clone(), right));
        Ok(())
    }
}
