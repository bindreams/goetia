//! Helpers the library's unit tests and its integration tests share. An integration test reaches the
//! library only through its public API, so what both need is exported here, under the `testing`
//! feature (and under `cfg(test)`, for the unit tests). Not public API: nothing here is stable.

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::SystemTime;

#[cfg(windows)]
use crate::error::Result;
#[cfg(windows)]
use crate::shim::{FolderCheck, HostCommand, ShimProvider, SweepFailure};
use crate::spec::{
    CronField, DaemonFields, DaemonSpec, DayRule, HashSeed, Id, Protocol, Restart, Schedule, ServiceFields, TaskFields,
    TaskSpec, User,
};

pub use crate::backend::scm::shim::acl::{CHANGE_PERMISSIONS_OF, WRITE_TO};

/// The machine's time zone as goetia reads it for a task's next run, for a test that turns launchd's
/// local dates into instants.
#[cfg(target_os = "macos")]
pub use crate::backend::launchd::next::system_zone;

/// Runs every skuld test in the calling binary and exits; the `main` of every skuld test binary.
/// Exits with an error first if a `GOETIA_TEST_<GROUP>` or `GOETIA_TEST_<GROUP>_CONSENT` variable, or
/// `GOETIA_TEST_DEBUG_PRIVILEGE` or `GOETIA_TEST_ROOT_DIRS`, is invalid.
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

/// The machine's `%ProgramData%`, as the library locates it for the shim's log and the run records.
/// A machine that cannot name it fails the calling test.
#[cfg(windows)]
pub fn program_data() -> PathBuf {
    crate::backend::scm::folder::program_data()
        .unwrap_or_else(|e| panic!("cannot locate %ProgramData%: {}", e.report()))
}

/// The version marker every service host carries and an install checks for, which a program that is no host
/// (a test binary a test installs as one) is given by appending it.
#[cfg(windows)]
pub fn host_marker() -> Vec<u8> {
    crate::backend::scm::shim::marker::MARKER.to_vec()
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

/// Whether the account `sid` holds the logon right `right` (`"SeBatchLogonRight"`), as the local security
/// policy says (`LsaEnumerateAccountRights`). A failure to ask fails the calling test.
#[cfg(windows)]
pub fn account_holds_right(sid: &[u8], right: &str) -> bool {
    use windows_sys::Win32::Security::Authentication::Identity::{
        LSA_UNICODE_STRING, LsaClose, LsaEnumerateAccountRights, LsaFreeMemory, LsaNtStatusToWinError,
        POLICY_LOOKUP_NAMES,
    };
    use windows_sys::Win32::Security::PSID;

    const ERROR_FILE_NOT_FOUND: u32 = 2;

    let policy = open_policy(POLICY_LOOKUP_NAMES as u32);

    let mut rights: *mut LSA_UNICODE_STRING = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: `policy` was opened above; `sid` is a SID buffer live for the call; the out-params are valid.
    let status = unsafe { LsaEnumerateAccountRights(policy, sid.as_ptr() as PSID, &mut rights, &mut count) };
    let mut held = false;
    if status == 0 {
        // SAFETY: on success `rights` points at `count` strings, each `Length` bytes of UTF-16 at `Buffer`.
        held = unsafe { std::slice::from_raw_parts(rights, count as usize) }
            .iter()
            .any(|name| {
                let units = unsafe { std::slice::from_raw_parts(name.Buffer, name.Length as usize / 2) };
                String::from_utf16_lossy(units) == right
            });
        // SAFETY: `rights` was allocated by `LsaEnumerateAccountRights` and is freed once.
        unsafe { LsaFreeMemory(rights.cast()) };
    } else {
        // SAFETY: `LsaNtStatusToWinError` takes a plain `NTSTATUS`.
        let win32 = unsafe { LsaNtStatusToWinError(status) };
        // An account with no rights at all is not found by the policy: it holds none.
        assert_eq!(
            win32, ERROR_FILE_NOT_FOUND,
            "enumerate the rights of the account: NTSTATUS {status:#x}"
        );
    }
    // SAFETY: `policy` was opened above and is closed once, after its last use.
    unsafe { LsaClose(policy) };
    held
}

/// Takes the logon right `right` away from the account `sid`, so that a test can show an install grants it again.
/// A right the account does not hold is done. A failure fails the calling test.
#[cfg(windows)]
pub fn revoke_right(sid: &[u8], right: &str) {
    use windows_sys::Win32::Security::Authentication::Identity::{
        LSA_UNICODE_STRING, LsaClose, LsaNtStatusToWinError, LsaRemoveAccountRights, POLICY_CREATE_ACCOUNT,
        POLICY_LOOKUP_NAMES,
    };
    use windows_sys::Win32::Security::PSID;

    const ERROR_FILE_NOT_FOUND: u32 = 2;

    let policy = open_policy((POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32);
    let mut text: Vec<u16> = right.encode_utf16().collect();
    let bytes = (text.len() * 2) as u16;
    let name = LSA_UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: text.as_mut_ptr(),
    };
    // SAFETY: `policy` was opened above; `sid` is a SID buffer and `name` points into `text`, both live for the call.
    let status = unsafe { LsaRemoveAccountRights(policy, sid.as_ptr() as PSID, false, &name, 1) };
    // SAFETY: `policy` was opened above and is closed once, after its last use.
    unsafe { LsaClose(policy) };
    if status != 0 {
        // SAFETY: `LsaNtStatusToWinError` takes a plain `NTSTATUS`.
        let win32 = unsafe { LsaNtStatusToWinError(status) };
        assert_eq!(win32, ERROR_FILE_NOT_FOUND, "remove {right}: NTSTATUS {status:#x}");
    }
}

/// The local security policy opened for `access`. A failure fails the calling test.
#[cfg(windows)]
fn open_policy(access: u32) -> isize {
    use windows_sys::Win32::Security::Authentication::Identity::{LSA_OBJECT_ATTRIBUTES, LsaOpenPolicy};

    let mut policy: isize = 0;
    let attributes = LSA_OBJECT_ATTRIBUTES::default();
    // SAFETY: `attributes` is a valid, zeroed `LSA_OBJECT_ATTRIBUTES`; a null system name is the local one.
    let status = unsafe { LsaOpenPolicy(std::ptr::null(), &attributes, access, &mut policy) };
    assert_eq!(status, 0, "open the local security policy: NTSTATUS {status:#x}");
    policy
}

/// When Task Scheduler itself says the task `id` runs between `from` and `to`: its `GetRunTimes`, the
/// oracle the schedule tests compare the cron they translated with.
///
/// A disarmed task has no triggers, so there is nothing to ask. Its schedule's triggers are written for
/// the question with the task's own flag false, which no start can come of (`GetRunTimes` lists a
/// disabled task's times, M2), and the task is registered again as it was after; an armed task is asked as
/// it is. A task that logs on with a password is registered again with `GOETIA_SERVICE_PASSWORD`, as the
/// verbs do, and is [`PasswordRequired`](crate::error::Error::PasswordRequired) without it, before anything is written.
#[cfg(windows)]
pub fn task_run_times(id: &Id, from: SystemTime, to: SystemTime) -> Result<Vec<SystemTime>> {
    use crate::backend::scm::instance::TaskPath;
    use crate::backend::scm::manager::task::{Com, TaskApi as _, registration_password, stored_account};
    use crate::backend::scm::task_xml::{TaskXml, extract_from_data};
    use crate::error::Error;

    let path = TaskPath::of(id);
    let registered = Com
        .get(&path)?
        .ok_or_else(|| Error::NotInstalled { id: id.to_string() })?;
    if TaskXml::armed(&registered.xml) {
        return Com.run_times(&path, from, to);
    }
    let blob = extract_from_data(&registered.data)?.ok_or_else(|| Error::blob("the task's `Data` is not goetia's"))?;
    let account = stored_account(&registered)?;
    let password = registration_password(&account)?;
    let held = TaskXml::with_own_flag(&TaskXml::with_triggers(&registered.xml, &blob.spec)?, false)?;
    Com.register(&path, &held, &account, password.as_deref(), true)?;
    let times = Com.run_times(&path, from, to);
    let back = Com.register(&path, &registered.xml, &account, password.as_deref(), true);
    match (times, back) {
        (Ok(times), Ok(_)) => Ok(times),
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
        // The task is left held, which is what the caller must hear first.
        (Err(times), Err(back)) => Err(Error::both_all([back, times]).expect("two errors")),
    }
}

/// Deletes the task `\Goetia\<id>` and anything of it a test made, retrying a transient failure for
/// about five seconds and then warning. A task that is not there is done. Never panics: it is what a
/// guard's `Drop` calls.
#[cfg(windows)]
pub fn remove_task(id: &Id) {
    crate::backend::scm::manager::task::cleanup::remove_task(id);
}

#[cfg(windows)]
pub use crate::backend::scm::shim::acl::{Ace, Dacl, kernel_object_dacl};

/// The DACL of the file or folder `path`, as an install reads one, not following a link at it.
#[cfg(windows)]
pub fn file_dacl(path: &std::path::Path) -> Result<Dacl> {
    let security = crate::backend::scm::shim::acl::read_path(path)?;
    Ok(Dacl {
        protected: security.protected,
        aces: security.dacl,
    })
}

/// The binary SID with `authority` as its identifier authority and `subauthorities` after it, as an
/// [`Ace`] names its trustee.
#[cfg(windows)]
pub fn sid_of(authority: u8, subauthorities: &[u32]) -> Vec<u8> {
    crate::backend::scm::sid::sid_from_parts(authority, subauthorities)
}
/// The key of the `Goetia` Event Log source an install registers, under `HKEY_LOCAL_MACHINE`.
#[cfg(windows)]
pub const EVENT_SOURCE_KEY: &str = crate::backend::scm::manager::EVENT_SOURCE_KEY;
/// What an install writes as the source's `TypesSupported`.
#[cfg(windows)]
pub const EVENT_TYPES_SUPPORTED: u32 = crate::backend::scm::manager::EVENT_TYPES_SUPPORTED;
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

/// The cron schedules a task's triggers must fire for exactly when the cron says, in three groups: one
/// time and repeated times a day; irregular times of day (several groups of minutes in an hour, a step
/// that does not divide 60, hours in no single run); and a weekday that has a fifth occurrence. The unit tests of the compilation check each against a
/// model of Task Scheduler, and the Windows integration test checks each against Task Scheduler's own run
/// times (see `tests/scm_integration/task_schedule.rs`).
pub const FIRING_SCHEDULES: [&[&str]; 3] = [
    &[
        "30 3 * * *",
        "H 3 * * *",
        "0 9-17 * * 1-5",
        "*/15 9-17 * * 1-5",
        "*/7 * * * *",
        // Days of the month no month has, beside a weekday: the weekdays alone.
        "0 0 30 2 1",
        "0 0 */2 * *",
        "0 0 29-31 * *",
        "0 0 * 6 1",
        "0 3 * 3-5 *",
        "* * * * *",
        "5,35 */3 * * *",
    ],
    &[
        // Several groups in an hour: a lone :00 beside a repetition or a lone minute, or a repetition
        // beside a lone minute (`* * * * *` and `*/7 * * * *` above are the first kind too).
        "0,15,30,45 9-11,14-16 * * *",
        "1,2,4,7 0,1,3,5,7,9,11 * * *",
        "0,15,30,45,7 9-17 * * 1-5",
        "0,1 * * * *",
        "0,1 0,1,2,3,5,6,8,10,11,13,14,16,17,19,21,22 * * *",
        "0,10,20 22,23,0 * * *",
        // A step that does not divide 60, in one hour and in a run of hours.
        "*/7 3 * * *",
        "*/7 9-17 * * 1-5",
        // One minute in hours in no run (a lone trigger in each), and in every hour.
        "17 */5 * * *",
        "0 * * * *",
        "0 0-4,7,9-12,14,17,20-23 * * 1",
        "0 0,1,2,3,5,6,8,10,11,13,14,16,17,19,21,22 * * *",
    ],
    // August 2027 has five Mondays, and so does May 2028: `Last` is the fifth in those months.
    &["0 0 * 8 1", "0 0 * 5 1"],
];

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
