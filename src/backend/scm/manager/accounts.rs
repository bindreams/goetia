//! The one boundary to the machine's account database. [`Accounts`] is the whole of what goetia asks
//! of it: [`Lsa`] answers from Windows (`LookupAccountNameW`, `LookupAccountSidW`, the local security
//! policy), and the test `fake::Fake` from a table of stated answers.
//!
//! The lookups answer with the OS error itself. What the user reads of a failure is the work of
//! [`identity`](super::identity), which knows the manifest field a value came from; and what the
//! logic decides from the answers (a user, a built-in, a SID written as text) is `identity`'s too.

#[cfg(test)]
pub(crate) mod fake;
mod lsa;

pub(crate) use lsa::Lsa;
use windows_sys::Win32::Security::{
    SidTypeAlias, SidTypeComputer, SidTypeDeletedAccount, SidTypeDomain, SidTypeGroup, SidTypeLabel,
    SidTypeLogonSession, SidTypeUser, SidTypeWellKnownGroup,
};

use super::identity::{Right, UserSid};
use crate::backend::scm::sid::Sid;
use crate::error::Result;

/// The machine's accounts, as the Windows backend asks about them.
pub(crate) trait Accounts: Send + Sync {
    /// The account `name` names, with `name` as asked; a leading `.\` is stripped before the lookup. Fails
    /// with the OS error: `ERROR_NONE_MAPPED` when no account has the name.
    fn by_name(&self, name: &str) -> std::io::Result<Account>;

    /// The account `sid` is, named `DOMAIN\Name` (or the bare name of a well-known one). Fails with the OS
    /// error: `ERROR_NONE_MAPPED` when no account has the SID (a deleted account's SID still appears in ACLs).
    fn by_sid(&self, sid: &Sid) -> std::io::Result<Account>;

    /// Grants `user` `right`. `user` is the account [`require_user_account`](super::identity::require_user_account)
    /// checked; the grant does no lookup of its own. Granting it again changes nothing.
    fn grant(&self, user: &UserSid, right: Right) -> Result<()>;
}

/// What the account database says of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    pub sid: Sid,
    pub name: String,
    pub kind: AccountKind,
}

/// What an account is: Windows's `SID_NAME_USE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountKind {
    User,
    Group,
    Domain,
    /// A local group (`SidTypeAlias`).
    LocalGroup,
    WellKnownGroup,
    DeletedAccount,
    Computer,
    IntegrityLabel,
    LogonSession,
    /// Any other kind.
    Other,
}

impl AccountKind {
    /// The kind a `SID_NAME_USE` names.
    #[allow(non_upper_case_globals)] // windows-sys spells `SID_NAME_USE` values in CamelCase.
    pub(super) fn of(name_use: i32) -> Self {
        match name_use {
            SidTypeUser => Self::User,
            SidTypeGroup => Self::Group,
            SidTypeDomain => Self::Domain,
            SidTypeAlias => Self::LocalGroup,
            SidTypeWellKnownGroup => Self::WellKnownGroup,
            SidTypeDeletedAccount => Self::DeletedAccount,
            SidTypeComputer => Self::Computer,
            SidTypeLabel => Self::IntegrityLabel,
            SidTypeLogonSession => Self::LogonSession,
            _ => Self::Other,
        }
    }

    /// What the kind is, for the refusal of an account that is not a user.
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::User => "a user",
            Self::Group => "a group",
            Self::Domain => "a domain",
            Self::LocalGroup => "a local group",
            Self::WellKnownGroup => "a well-known group",
            Self::DeletedAccount => "a deleted account",
            Self::Computer => "a computer",
            Self::IntegrityLabel => "an integrity label",
            Self::LogonSession => "a logon session",
            Self::Other => "of no known kind",
        }
    }
}
