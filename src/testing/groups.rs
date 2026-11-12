//! Test groups: the tests that change real system state, gated by environment variables.
//!
//! - `GOETIA_TEST_<GROUP>`: unset or `1` runs the group's tests, `0` ignores them (skuld names the
//!   variable), any other value fails them.
//! - `GOETIA_TEST_<GROUP>_CONSENT`: a test in a group that runs fails unless this is exactly `1`
//!   (unset is `0`); any value but `0` and `1` fails it too. Set it only in a sandbox: a container,
//!   a VM or CI.
//! - A consented group on a host that lacks what the group needs (elevation) fails; it never skips.
//!
//! A test joins a group by taking its fixture: `#[fixture(scm_group)] _group: &Group`.

/// Proof that the group is enabled, consented to and supported by the host.
pub struct Group;

/// A group's variable, what its tests do (for the consent message) and whether they need an
/// elevated token.
struct Spec {
    env: &'static str,
    what: &'static str,
    elevated: bool,
}

const SCM: Spec = Spec {
    env: "GOETIA_TEST_SCM",
    what: "install, start, stop and remove real Windows services, write the real registry (service keys and \
           scratch keys under HKLM\\SOFTWARE), create local user accounts, write under %ProgramFiles% and \
           %ProgramData%, take the machine-wide install lock, and read the Windows Event Log",
    elevated: true,
};

const SYSTEMD: Spec = Spec {
    env: "GOETIA_TEST_SYSTEMD",
    what: "write unit files and drop-ins under /etc/systemd/system, enable, start, stop and remove real systemd \
           units, and run systemctl as root",
    elevated: true,
};

const LAUNCHD: Spec = Spec {
    env: "GOETIA_TEST_LAUNCHD",
    what: "write plists under /Library/LaunchDaemons and bootstrap, kickstart and boot out real launchd jobs as root",
    elevated: true,
};

/// The platform's own service manager, read and not written, for a test that runs on every platform:
/// the platform's group variable, and no elevation.
const NATIVE_WHAT: &str = "read the real service manager's services and unit or plist files";
#[cfg(windows)]
const NATIVE: Spec = Spec {
    what: NATIVE_WHAT,
    elevated: false,
    ..SCM
};
#[cfg(target_os = "macos")]
const NATIVE: Spec = Spec {
    what: NATIVE_WHAT,
    elevated: false,
    ..LAUNCHD
};
#[cfg(not(any(windows, target_os = "macos")))]
const NATIVE: Spec = Spec {
    what: NATIVE_WHAT,
    elevated: false,
    ..SYSTEMD
};

/// Whether `name` is on, off or unset in the manner of a group variable: `Ok(None)` for unset,
/// `Ok(Some(flag))` for `1` and `0`, `Err` naming the variable and the value for any other.
fn switch(name: &str, var: &impl Fn(&str) -> Option<String>) -> Result<Option<bool>, String> {
    match var(name).as_deref() {
        None => Ok(None),
        Some("1") => Ok(Some(true)),
        Some("0") => Ok(Some(false)),
        Some(other) => Err(format!("{name} is {other:?}: set it to 0 or 1, or leave it unset")),
    }
}

/// The rule behind every group: `Ok(false)` when `group_var` is `0`, `Ok(true)` when it is on and
/// consent is given, `Err` for an invalid value of either variable or for missing consent.
pub(crate) fn check_group(group_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<bool, String> {
    if switch(group_var, &var)? == Some(false) {
        return Ok(false);
    }
    let consent = format!("{group_var}_CONSENT");
    if switch(&consent, &var)? == Some(true) {
        return Ok(true);
    }
    Err(format!(
        "these tests change real system state and run only in a sandbox (a container, VM or CI); set \
         {consent}=1 there to consent, or {group_var}=0 to turn the group off"
    ))
}

/// A group's `requires`: `Err` only when `group_var` is `0`. An invalid value is left to fail the
/// test at setup, not skip it.
pub(crate) fn require_enabled(group_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if var(group_var).as_deref() == Some("0") {
        return Err(format!("{group_var}=0"));
    }
    Ok(())
}

/// A group's setup: [`check_group`]'s rule, then `precondition`, which names what the host lacks.
/// `what` says what the group's tests do.
pub(crate) fn enter(
    group_var: &str,
    what: &str,
    var: impl Fn(&str) -> Option<String>,
    precondition: impl FnOnce() -> Result<(), String>,
) -> Result<Group, String> {
    match check_group(group_var, var) {
        // skuld runs the body of a test whose `requires` failed when ignored tests are included
        // (`--run-ignored only`), so a group that is off refuses here as well.
        Ok(false) => Err(format!("{group_var}=0")),
        Ok(true) => precondition()
            .map(|()| Group)
            .map_err(|lacks| format!("the group {group_var} needs {lacks}; set {group_var}=0 to turn it off")),
        Err(why) => Err(format!("the group {group_var} runs tests that {what}: {why}")),
    }
}

/// The environment of this process. A value that is not Unicode is lossily decoded, so it is an
/// invalid value rather than an unset one.
fn process_var(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

/// The host's token is elevated, where the group needs it.
fn host_is_elevated(needed: bool) -> Result<(), String> {
    if !needed || cosca::elevation::is_elevated() {
        Ok(())
    } else {
        Err("an elevated token (root, or an Administrator shell), which this host's process lacks".to_string())
    }
}

/// Declares the fixture `$fixture` of the group `$spec`, ignored by `$enabled` when the group is `0`.
macro_rules! group {
    ($fixture:ident, $enabled:ident, $spec:expr) => {
        fn $enabled() -> Result<(), String> {
            require_enabled($spec.env, process_var)
        }

        #[skuld::fixture(requires = [$enabled])]
        pub fn $fixture() -> Result<Group, String> {
            enter($spec.env, $spec.what, process_var, || {
                host_is_elevated($spec.elevated)
            })
        }
    };
}

group!(scm_group, scm_enabled, SCM);
group!(systemd_group, systemd_enabled, SYSTEMD);
group!(launchd_group, launchd_enabled, LAUNCHD);
group!(native_group, native_enabled, NATIVE);

#[cfg(test)]
#[path = "groups_tests.rs"]
mod groups_tests;
