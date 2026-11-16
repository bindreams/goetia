//! What names a task and one of its runs: the same on every host, so the pure code that reads and
//! writes them is tested everywhere.

use crate::spec::Id;

/// The Task Scheduler folder every goetia task lives in, so that `scan` lists it alone and no other
/// task is read as an id.
pub(crate) const TASK_FOLDER: &str = r"\Goetia";

/// The path of a task, `\Goetia\<id>`.
///
/// Task Scheduler matches paths case-insensitively, as the SCM matches service names, so two paths
/// are equal when their text is equal ignoring ASCII case (an id is ASCII). The path keeps the case
/// it was made with.
#[derive(Debug, Clone)]
pub(crate) struct TaskPath(String);

impl TaskPath {
    pub(crate) fn of(id: &Id) -> Self {
        Self(format!(r"{TASK_FOLDER}\{id}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The task's name in [`TASK_FOLDER`]: its id as it was written.
    pub(crate) fn name(&self) -> &str {
        self.0.rsplit('\\').next().unwrap_or(&self.0)
    }

    /// The id a task named `name` in [`TASK_FOLDER`] stands for: `None` for a name that is not a
    /// valid id, which is not goetia's.
    pub(crate) fn id(name: &str) -> Option<Id> {
        Id::try_from(name).ok()
    }
}

impl PartialEq for TaskPath {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for TaskPath {}

/// The account a task runs as: the XML's principal and the registration's logon type, kept in one
/// value so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskAccount {
    /// `SYSTEM`, `LocalService` or `NetworkService`, by well-known SID: no password.
    Service(String),
    /// A user account, which logs on with its password.
    User(String),
}

/// The identity of one run of a task: the GUID Task Scheduler gives its instance, as its 16
/// digit-pairs in the order the GUID is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct InstanceId([u8; 16]);

impl InstanceId {
    /// The GUID's 32 digits, lowercase, with no braces or dashes: how a run's record and log marker
    /// name it.
    pub(crate) fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The first line a task's shim writes to the log of this run, so that a reader knows where the
    /// run's own output begins: `goetia-run` and the [`hex`](Self::hex) digits.
    pub(crate) fn marker(&self) -> String {
        format!("goetia-run {}", self.hex())
    }

    /// A GUID as COM writes it (`{6F9619FF-8B86-D011-B42D-00C04FC964FF}`), or as [`hex`](Self::hex)
    /// does, or dashed without braces. Digits of either case; `None` for anything else.
    pub(crate) fn parse(braced_or_plain: &str) -> Option<Self> {
        let unbraced = match (braced_or_plain.strip_prefix('{'), braced_or_plain.strip_suffix('}')) {
            (Some(rest), Some(_)) => rest.strip_suffix('}')?,
            (None, None) => braced_or_plain,
            _ => return None,
        };
        let digits = if unbraced.len() == 36 {
            let dashes = [8, 13, 18, 23];
            if !dashes.iter().all(|&at| unbraced.as_bytes()[at] == b'-') {
                return None;
            }
            unbraced.split('-').collect::<String>()
        } else {
            unbraced.to_owned()
        };
        if digits.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (byte, pair) in bytes.iter_mut().zip(digits.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).ok()?;
            if !pair.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

#[cfg(test)]
#[path = "instance_tests.rs"]
mod instance_tests;
