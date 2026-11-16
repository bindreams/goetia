//! The parent directories `logs` and `cwd` need, which a unit fails at start without, made by
//! [`CreatedDirs`] and judged for the one thing systemd does as root with them.

use std::path::Path;

use crate::backend::account::{Account, find_uid, resolve_account};
use crate::backend::created_dirs::{CreatedDirs, Owner, refuse_writable_log_dir};
use crate::error::{Error, Result};
use crate::spec::{AccountId, DaemonSpec, User};

/// Make the parent of `logs` and `cwd` where they are missing, refuse a log that root's `append:` open
/// could be led astray in, and hand `cwd` to a non-root account, all before this returns: the unit can
/// run as soon as the write that follows starts, and finds every directory as it is to be. systemd opens
/// the file before it drops privileges and follows a symlink, so a directory an account can write is a
/// way for it to make root append to any file. The log directory is therefore never handed over, a log
/// in or under `cwd` is refused (it is to be the account's), and so is an existing directory the
/// account can write, and an existing log that is a link or the account's.
///
/// Everything is judged first, on what [`CreatedDirs::plan`] read, and nothing is made until it all
/// passes, except one thing only what is made can show: a log's directory that is `cwd`, or in it, is
/// found when [`CreatedDirs::make`] reaches `cwd` (a file system that ignores case makes two names
/// one directory), and what was made is then taken back. Runs while still elevated.
///
/// A `user` that is a number is refused on every install, as `numeric_account` says, before anything is
/// planned. A `user` that is a name is looked up only when `cwd` is to be made and handed to it: an
/// install that hands nothing over does not need it to exist yet (`sysusers` or a package may make it
/// later), and an unknown one then fails before anything is made.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    let common = spec.common();
    let numeric = numeric_account(&common.user)?;
    let logs_parent = common
        .logs
        .as_deref()
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty());
    let log = logs_parent.map(|parent| created.plan(parent)).transpose()?;
    let cwd = common.cwd.as_deref().map(|cwd| created.plan(cwd)).transpose()?;

    if let (Some(logs), Some(log)) = (&common.logs, &log) {
        refuse_writable_log_dir(logs, log)?;
    }
    let owner = match &cwd {
        Some(cwd) if cwd.makes() => owner_of(&common.user, numeric)?,
        _ => None,
    };

    created.make(
        log.map(|log| (log, None))
            .into_iter()
            .chain(cwd.map(|cwd| (cwd, owner)))
            .collect(),
    )
}

/// Whom `cwd` is handed to for `user`, whose account is `numeric` if it is a number. Root, by any
/// spelling (uid 0 is root whatever names it), is handed nothing: the directory is already root's.
fn owner_of(user: &User, numeric: Option<Account>) -> Result<Option<Owner>> {
    if matches!(user, User::Root) {
        return Ok(None);
    }
    match numeric {
        Some(account) => account.owner(),
        None => resolve_account(user)?.owner(),
    }
}

/// The account `user` is, if it is a number: a uid, or a name that is the canonical decimal spelling of
/// one (digits only, no sign, no leading zero except `0`), which systemd reads as a uid and never looks
/// up as the name of an account made of digits. Any other spelling is a name, so `None`.
///
/// An error where systemd will not run a unit as that uid (measured on systemd 255): one with no passwd
/// entry fails every start with `217/USER`, and 65535 and 4294967295, which `uid_is_valid` refuses, do
/// not load.
fn numeric_account(user: &User) -> Result<Option<Account>> {
    let uid = match user {
        User::Id(AccountId::Uid(uid)) => *uid,
        User::Name(name) => match canonical_uid(name) {
            Some(uid) => uid,
            None => return Ok(None),
        },
        User::Root | User::Id(AccountId::Sid(_)) => return Ok(None),
    };
    let wont_run = |why: &str| Error::AccountLookup {
        detail: format!("systemd will not run a unit as uid {uid}: {why}"),
        source: None,
    };
    if uid == 0xFFFF || uid == u32::MAX {
        return Err(wont_run("it is not a valid uid"));
    }
    find_uid(uid)?
        .map(Some)
        .ok_or_else(|| wont_run("no passwd entry has it, and every start of the unit would fail"))
}

fn canonical_uid(name: &str) -> Option<u32> {
    let canonical = name.bytes().all(|b| b.is_ascii_digit()) && (name == "0" || !name.starts_with('0'));
    canonical.then(|| name.parse().ok()).flatten()
}

#[cfg(test)]
#[path = "dirs_tests.rs"]
mod dirs_tests;
