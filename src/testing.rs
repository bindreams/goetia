//! Helpers the library's unit tests and its integration tests share. An integration test reaches the
//! library only through its public API, so what both need is exported here, under the `testing`
//! feature (and under `cfg(test)`, for the unit tests). Not public API: nothing here is stable.

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::{Arc, Mutex};

#[cfg(windows)]
use crate::error::Result;
#[cfg(windows)]
use crate::shim::{FolderCheck, HostCommand, ShimProvider, SweepFailure};
use crate::spec::{
    CronField, DaemonFields, DaemonSpec, DayRule, HashSeed, Id, Protocol, Restart, Schedule, ServiceFields, TaskFields,
    TaskSpec, User,
};

pub use crate::backend::scm::shim::acl::{CHANGE_PERMISSIONS_OF, WRITE_TO};

/// Runs every skuld test in the calling binary and exits; the `main` of every skuld test binary.
/// Exits with an error first if a `GOETIA_TEST_<GROUP>` or `GOETIA_TEST_<GROUP>_CONSENT` variable, or
/// `GOETIA_TEST_DEBUG_PRIVILEGE`, is invalid.
///
/// A test is named `module::fn` (a crate-root test keeps its bare name), so two modules may each hold
/// a `fn x`; under skuld's default bare names the binary panics at startup on the second.
pub fn run_all() -> ! {
    groups::validate_environment();
    let mut runner = skuld::TestRunner::new();
    runner.libtest_names();
    runner.run()
}

/// Spawns `program` with `args` as the unix backends spawn `launchctl` and `systemctl`
/// (`backend::bounded::command`: stdin from `/dev/null`, the process tree contained), and gives back the
/// running child. A test of what that spawn does to the child (which process group it leaves it in) calls
/// this, so that it follows the real spawn. Fails the calling test if the child cannot be spawned.
#[cfg(unix)]
pub fn spawn_contained(program: &str, args: &[&str]) -> cosca::Child {
    crate::backend::bounded::command(program, args)
        .unwrap_or_else(|e| panic!("cannot make the command `{program}`: {e}"))
        .spawn()
        .unwrap_or_else(|e| panic!("cannot spawn `{program}`: {e}"))
}

/// `sid` as Windows writes it. A SID Windows cannot show fails the calling test, naming its bytes:
/// a test never takes a placeholder for a SID.
#[cfg(windows)]
pub fn shown_sid(sid: &[u8]) -> String {
    crate::backend::scm::sid::try_sid_display(sid).unwrap_or_else(|| panic!("{sid:?} is not a SID Windows can show"))
}

/// The SID of the local account `account_name`, as the library looks it up when it installs a
/// daemon that runs as it.
#[cfg(windows)]
pub fn sid_for_account_name(account_name: &str) -> Result<Vec<u8>> {
    crate::backend::scm::manager::sid_for_account_name(account_name, None).map(|(sid, _kind)| sid)
}

#[cfg(windows)]
pub use crate::backend::scm::shim::acl::{Ace, Dacl, kernel_object_dacl};
#[cfg(windows)]
pub use crate::backend::scm::wide::{units_until_nul, wide_null};

pub mod cleanup;
pub mod elevation;
pub mod groups;
#[cfg(windows)]
pub mod raw_registry;
#[cfg(windows)]
pub mod scratch;

/// A root, `restart: never` `Protocol::Native` (`protocol: native`) daemon named `id`. Panics on an
/// invalid `Id`.
/// Vary fields with struct-update syntax, then `ServiceSpec::new`.
pub fn base_fields(id: &str, command: Vec<String>) -> ServiceFields {
    ServiceFields {
        common: DaemonFields {
            id: Id::try_from(id).unwrap_or_else(|e| panic!("`{id}` is not a valid Id: {e}")),
            name: id.to_string(),
            command,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            user: User::Root,
            logs: None,
        },
        restart: Restart::Never,
        restart_delay: None,
        protocol: Protocol::Native,
    }
}

/// The fields of the service `spec`, to vary with struct-update syntax and check again with
/// `ServiceSpec::new`.
pub fn service_fields(spec: DaemonSpec) -> ServiceFields {
    let DaemonSpec::Service(service) = spec else {
        panic!("`service_fields` is given a service: the test helpers build services");
    };
    service.into_fields()
}

/// A task named `id`, run as root, that starts `command` on `schedule` (`None`: nothing starts it).
/// `schedule` is parsed with a fixed [`HashSeed`], so an `H` resolves alike on every host. Panics on
/// an invalid `Id`, schedule or field.
pub fn task(id: &str, command: Vec<String>, schedule: Option<&str>) -> DaemonSpec {
    let common = base_fields(id, command).common;
    let schedule = schedule.map(|text| {
        let seed = HashSeed {
            hostname: "testing",
            id: &common.id,
        };
        Schedule::parse(text, &seed).unwrap_or_else(|e| panic!("`{text}` is not a valid schedule: {e}"))
    });
    TaskSpec::new(TaskFields { common, schedule })
        .unwrap_or_else(|e| panic!("`{id}` is not a valid task: {e}"))
        .into()
}

/// Whether `schedule` names the minute whose civil fields are given: `weekday` is 0 to 6, Sunday 0.
/// The rule itself, a plain reading of the five resolved fields and the day rule, kept apart from
/// the code that finds a next fire so a test can check one against the other.
pub fn cron_matches(schedule: &Schedule, minute: u8, hour: u8, day: u8, month: u8, weekday: u8) -> bool {
    let [minutes, hours, days, months, weekdays] = schedule.fields();
    let has = |field: &CronField, value: u8| field.values.contains(&value);
    let day_ok = match schedule.day_rule() {
        DayRule::And => has(days, day) && has(weekdays, weekday),
        DayRule::Or => has(days, day) || has(weekdays, weekday),
    };
    has(minutes, minute) && has(hours, hour) && has(months, month) && day_ok
}

/// A [`ShimProvider`] that records the calls it gets (`"preview"`, `"provide"`, `"sweep"`, in
/// order) and answers with the commands it was given. It touches nothing.
#[cfg(windows)]
pub struct FakeProvider {
    calls: Arc<Mutex<Vec<&'static str>>>,
    preview: HostCommand,
    provide: HostCommand,
}

#[cfg(windows)]
impl FakeProvider {
    /// The provider, answering `preview` with `preview` and `provide` with `provide`, and the
    /// calls it records.
    pub fn new(preview: HostCommand, provide: HostCommand) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::default();
        let provider = Self {
            calls: Arc::clone(&calls),
            preview,
            provide,
        };
        (provider, calls)
    }
}

#[cfg(windows)]
impl ShimProvider for FakeProvider {
    fn preview(&self) -> Result<HostCommand> {
        self.calls.lock().unwrap().push("preview");
        Ok(self.preview.clone())
    }

    /// None, without recording a call: its `sweep` removes nothing, so it covers no folder.
    fn folder(&self) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    fn provide(&self, _folders: &FolderCheck) -> Result<HostCommand> {
        self.calls.lock().unwrap().push("provide");
        Ok(self.provide.clone())
    }

    fn sweep(&self, _referenced: &[PathBuf], _folders: &FolderCheck) -> std::result::Result<(), SweepFailure> {
        self.calls.lock().unwrap().push("sweep");
        Ok(())
    }
}

/// A host that runs `exe` with no arguments.
#[cfg(windows)]
pub fn command(exe: impl Into<PathBuf>) -> HostCommand {
    HostCommand {
        exe: exe.into(),
        args: vec![],
    }
}
