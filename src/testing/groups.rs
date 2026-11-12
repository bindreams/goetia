//! Test groups: the tests that change real system state, gated by environment variables.
//!
//! - `GOETIA_TEST_<GROUP>`: unset or `1` runs the group's tests, `0` ignores them (skuld names the
//!   variable), any other value fails them.
//! - `GOETIA_TEST_<GROUP>_CONSENT`: a test in a group that runs fails unless this is exactly `1`
//!   (unset is `0`); any value but `0` and `1` fails it too. Set it only in a sandbox: a container,
//!   a VM or CI.
//! - A consented group on a host that is not elevated fails; it never skips.
//!
//! A test joins a group by taking its fixture: `#[fixture(scm_group)] _group: &Group`.

use super::elevation::ELEVATED;

/// Witness that a group fixture passed; only the group fixtures build one.
pub struct Group(());

/// A group's variable and what its tests do (for the consent message).
struct Spec {
    env: &'static str,
    what: &'static str,
}

const SCM: Spec = Spec {
    env: "GOETIA_TEST_SCM",
    what: "install, start, stop and remove real Windows services, write the real registry (service keys and \
           scratch keys under HKLM\\SOFTWARE), create local user accounts, write under %ProgramFiles% and \
           %ProgramData%, take the machine-wide install lock, and read the Windows Event Log",
};

const SYSTEMD: Spec = Spec {
    env: "GOETIA_TEST_SYSTEMD",
    what: "write unit files and drop-ins under /etc/systemd/system, enable, start, stop and remove real systemd \
           units, and run systemctl as root",
};

const LAUNCHD: Spec = Spec {
    env: "GOETIA_TEST_LAUNCHD",
    what: "write plists under /Library/LaunchDaemons and bootstrap, kickstart and boot out real launchd jobs as root",
};

/// Reads a group variable: `Ok(None)` if unset, `Ok(Some(flag))` for `1`/`0`, `Err` naming the variable and value otherwise.
fn switch(name: &str, var: &impl Fn(&str) -> Option<String>) -> Result<Option<bool>, String> {
    match var(name).as_deref() {
        None => Ok(None),
        Some("1") => Ok(Some(true)),
        Some("0") => Ok(Some(false)),
        Some(other) => Err(format!("{name} is {other:?}: set it to 0 or 1, or leave it unset")),
    }
}

/// The group variable and the consent variable of `group_var`, each read by [`switch`]: an invalid
/// value of either is an error whatever the other says.
fn read_group(group_var: &str, var: &impl Fn(&str) -> Option<String>) -> Result<(Option<bool>, Option<bool>), String> {
    Ok((switch(group_var, var)?, switch(&format!("{group_var}_CONSENT"), var)?))
}

/// Every variable of every group goetia knows, whether or not this OS has tests in the group.
/// `Err` names the first invalid one.
pub(crate) fn validate_all(var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    [SCM, SYSTEMD, LAUNCHD]
        .iter()
        .try_for_each(|spec| read_group(spec.env, &var).map(drop))
}

/// [`validate_all`] on this process's environment.
pub(crate) fn validate_environment() {
    if let Err(why) = validate_all(process_var) {
        eprintln!("error: {why}");
        std::process::exit(1);
    }
}

/// The rule behind every group: `Ok(false)` when `group_var` is `0`, `Ok(true)` when it is on and
/// consent is given, `Err` for an invalid value of either variable or for missing consent.
pub(crate) fn check_group(group_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<bool, String> {
    let (group, consent) = read_group(group_var, &var)?;
    if group == Some(false) {
        return Ok(false);
    }
    if consent == Some(true) {
        return Ok(true);
    }
    Err(format!(
        "these tests change real system state and run only in a sandbox (a container, VM or CI); set \
         {group_var}_CONSENT=1 there to consent, or {group_var}=0 to turn the group off"
    ))
}

/// A group's `requires`: `Err` only when `group_var` is `0`. An invalid value is left to fail the
/// test at setup, not skip it.
pub(crate) fn require_enabled(group_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    match switch(group_var, &var) {
        Ok(Some(false)) => Err(format!("{group_var}=0")),
        _ => Ok(()),
    }
}

/// A group's setup: [`check_group`]'s rule, then the host must be elevated. `what` says what the
/// group's tests do.
pub(crate) fn enter(
    group_var: &str,
    what: &str,
    var: impl Fn(&str) -> Option<String>,
    elevated: impl FnOnce() -> bool,
) -> Result<Group, String> {
    match check_group(group_var, var) {
        // skuld runs the body of a test whose `requires` failed when ignored tests are included
        // (`--run-ignored only`), so a group that is off refuses here as well.
        Ok(false) => Err(format!("{group_var}=0")),
        Ok(true) if elevated() => Ok(Group(())),
        Ok(true) => Err(format!(
            "the group {group_var} needs an elevated token (root, or an Administrator shell), which this \
             host's process lacks; set {group_var}=0 to turn it off"
        )),
        Err(why) => Err(format!("the group {group_var} runs tests that {what}: {why}")),
    }
}

/// The environment of this process. A value that is not Unicode is lossily decoded, so it is an
/// invalid value rather than an unset one.
fn process_var(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

/// Declares the fixture `$fixture` of the group `$spec`, ignored by `$enabled` when the group is `0`.
/// Every group needs an elevated token, so its tests carry the [`ELEVATED`] label.
macro_rules! group {
    ($fixture:ident, $enabled:ident, $spec:expr) => {
        fn $enabled() -> Result<(), String> {
            require_enabled($spec.env, process_var)
        }

        #[skuld::fixture(requires = [$enabled], labels = [ELEVATED])]
        pub fn $fixture() -> Result<Group, String> {
            enter($spec.env, $spec.what, process_var, cosca::elevation::is_elevated)
        }
    };
}

group!(scm_group, scm_enabled, SCM);
group!(systemd_group, systemd_enabled, SYSTEMD);
group!(launchd_group, launchd_enabled, LAUNCHD);

#[cfg(test)]
#[path = "groups_tests.rs"]
mod groups_tests;
