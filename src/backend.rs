//! Per-platform backends.
//!
//! Each backend is two halves. The *pure* half turns a [`DaemonSpec`] plus a
//! pre-resolved [`Identity`] into artifact text; it does no I/O, needs no
//! privileges, and is deliberately not `#[cfg]`-gated, so the systemd
//! generator compiles and its tests run on Windows. The *effectful* half
//! writes that artifact and talks to the platform's service manager, and is
//! the only `cfg`-gated, elevation-requiring code in the crate.
//!
//! [`DaemonSpec`]: crate::spec::DaemonSpec

#[cfg(unix)]
pub(crate) mod bounded;
#[cfg(unix)]
pub(crate) mod client;
mod contract;
pub(crate) mod kill;
pub mod launchd;
pub mod scm;
pub(crate) mod standby;
pub mod systemd;
#[cfg(test)]
pub(crate) mod test_allowance;
mod unreaped;

pub(crate) use contract::{
    Backend, Decoded, InstalledDefinition, InstalledService, Marker, NativeRestart, OutputSource, ReadOutcome,
    RunHandle, Scan, ScanEntry, Unnamed, WriteOutcome,
};

/// A platform account, already resolved from [`crate::spec::User`] by the
/// effectful install path.
///
/// Resolution is I/O — launchd has no numeric-UID key, and a Windows SID
/// needs `LookupAccountSid` — so it happens before generation rather than
/// inside it. That is what keeps the generators pure, and therefore keeps
/// `generate(extract(artifact)) == artifact` checkable on any host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The account as the target platform names it: `"0"` for root on
    /// systemd, `"root"` on launchd, an account name on SCM.
    pub user: String,
}

/// What a task's run prints first, followed by the run's id: `run` finds its run's output in a file or
/// stream that several runs share by the line `<RUN_MARKER> <id>`.
pub(crate) const RUN_MARKER: &str = "goetia-run";

/// Refuse an `env` of a task that sets one of `resets`, the variables the `/bin/sh` the task starts
/// through resets or removes on its way to the program: it would not see the value. `task` says which
/// tasks start through `sh`, as the message names them.
pub(crate) fn refuse_env_sh_resets(
    common: &crate::spec::DaemonFields,
    resets: &[&str],
    task: &str,
) -> Result<(), crate::error::Error> {
    match resets.iter().find(|name| common.env.contains_key(**name)) {
        Some(name) => Err(crate::error::Error::Invalid {
            daemon: common.id.to_string(),
            message: format!(
                "`env` sets `{name}`, but {task} starts through /bin/sh, which resets it, so the program \
                 would not see the value"
            ),
            source: None,
        }),
        None => Ok(()),
    }
}

/// Escape the three characters XML element content cannot contain unescaped. Applied to every
/// emitted value (argv, paths, environment keys and values, the account name), since
/// `ServiceSpec::new` does not reject `&`/`<`/`>`. Shared by the XML generators (launchd's plist, Task
/// Scheduler's task).
///
/// XML 1.0 excludes U+FFFE and U+FFFF entirely, so no escape can represent them. `ServiceSpec::new`
/// rejects them (and the other Unicode noncharacters); this panics on them as a backstop rather
/// than emit XML no parser can load.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        assert!(
            !matches!(c, '\u{FFFE}' | '\u{FFFF}'),
            "value contains {c:?} (U+FFFE/U+FFFF), which XML 1.0 cannot represent at all; \
             ServiceSpec::new must reject this upstream, before it ever reaches a generator"
        );
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// The refusal of a schedule whose day-of-month and day-of-week fields must both match
/// ([`crate::spec::DaySelection::Both`]) by a `manager` that matches the two as either-or.
pub(crate) fn refuse_day_fields_and(id: &crate::spec::Id, manager: &str, text: &str) -> crate::error::Error {
    crate::spec::invalid(
        id,
        &format!(
            "{manager} matches a day of the month and a day of the week as either-or, so it cannot hold `{text}`: \
             one of its day fields starts with `*` (both must match) while both day fields are restricted; use \
             `*` for one of the two"
        ),
    )
}

/// What a backend answers for a task until its task support lands: only the Windows service manager,
/// whose task half is still to come, is left to say it.
#[cfg(windows)]
pub(crate) fn cannot_run_tasks(backend: &str, id: &crate::spec::Id) -> crate::error::Error {
    crate::spec::invalid(id, &format!("{backend} cannot run tasks yet"))
}
