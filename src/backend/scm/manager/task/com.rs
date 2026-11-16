//! [`TaskApi`] over the real Task Scheduler (`ITaskService` and its folders and tasks).

use std::marker::PhantomData;
#[cfg(any(test, feature = "testing"))]
use std::time::SystemTime;

#[cfg(any(test, feature = "testing"))]
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, RPC_E_CHANGED_MODE,
    SCHED_E_TASK_NOT_RUNNING,
};
#[cfg(any(test, feature = "testing"))]
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::TaskScheduler::{
    IRegisteredTask, IRunningTask, ITaskFolder, ITaskService, TASK_CREATE, TASK_ENUM_HIDDEN, TASK_LOGON_NONE,
    TASK_LOGON_PASSWORD, TASK_LOGON_SERVICE_ACCOUNT, TASK_UPDATE, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, HRESULT, IUnknown};

use super::api::{Instance, Registered, Registration, Started, TaskApi, TaskInfo, disabled_run, ole_date};
#[cfg(any(test, feature = "testing"))]
use super::clock::{local_to_system, system_to_local};
use crate::backend::scm::instance::{InstanceId, TASK_FOLDER, TaskAccount, TaskPath};
use crate::error::{Error, Result};

/// Task Scheduler itself, reached through COM.
pub(crate) struct Com;

// The apartment -------------------------------------------------------------------------------------------------------

/// COM initialised on this thread (multithreaded apartment) for as long as the value lives. Not
/// `Send`: the initialisation belongs to the thread that made it.
struct ComScope {
    /// Whether this scope initialised COM and so must uninitialise it: a thread already in another
    /// apartment (`RPC_E_CHANGED_MODE`) keeps its own, and COM is usable in it all the same.
    owns: bool,
    _thread: PhantomData<*const ()>,
}

impl ComScope {
    fn enter() -> Result<Self> {
        // SAFETY: no pointers; paired with `CoUninitialize` in `drop` when this call succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let owns = match hr {
            hr if hr.is_ok() => true,
            hr if hr == RPC_E_CHANGED_MODE => false,
            hr => return Err(Error::os("initialise COM", windows::core::Error::from(hr))),
        };
        Ok(Self {
            owns,
            _thread: PhantomData,
        })
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.owns {
            // SAFETY: balances the `CoInitializeEx` that succeeded in `enter`, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// A connection to Task Scheduler, inside its own apartment. The service is declared before the scope
/// so it releases while COM is still initialised; so must every object taken from it, which the
/// caller keeps shorter-lived than the session.
struct Session {
    service: ITaskService,
    _scope: ComScope,
}

fn connect() -> windows::core::Result<ITaskService> {
    // SAFETY: `TaskScheduler` is the class of `ITaskService`; the arguments are valid for the calls.
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None::<&IUnknown>, CLSCTX_INPROC_SERVER)?;
        let none = VARIANT::default();
        service.Connect(&none, &none, &none, &none)?;
        Ok(service)
    }
}

impl Session {
    fn open() -> Result<Self> {
        let scope = ComScope::enter()?;
        let service = connect().map_err(|e| Error::os("connect to Task Scheduler", e))?;
        Ok(Self { service, _scope: scope })
    }

    fn folder(&self, path: &str) -> windows::core::Result<ITaskFolder> {
        // SAFETY: a call on a live interface with a live BSTR.
        unsafe { self.service.GetFolder(&BSTR::from(path)) }
    }

    /// Runs `f` on the task at `path`: `None` if the lookup finds no such task, and then `f` is not
    /// run. What `f` itself fails with, including a "not found", is `f`'s error: only the lookup says
    /// a task is not there.
    fn with_task<T>(
        &self,
        path: &str,
        what: &str,
        f: impl FnOnce(&IRegisteredTask) -> windows::core::Result<T>,
    ) -> Result<Option<T>> {
        let failed = |e| Error::os(format!("{what} `{path}`"), e);
        // M10: a task or a folder that is not there is one of two "not found" codes.
        // SAFETY: calls on live interfaces with live strings.
        let task = match self
            .folder(r"\")
            .and_then(|root| unsafe { root.GetTask(&BSTR::from(path)) })
        {
            Ok(task) => task,
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(failed(e)),
        };
        f(&task).map(Some).map_err(failed)
    }

    /// Asks to remove the folder `parent`. Task Scheduler refuses a folder that holds a task or a
    /// folder, which is then not this call's to settle, and one that is already gone is done.
    fn remove_folder(&self, parent: &str) -> Result<()> {
        let failed = |e| Error::os(format!("remove the folder `{parent}`"), e);
        // M10: a folder that holds a task is refused with `ERROR_DIR_NOT_EMPTY`; one that is not there
        // reads "not found".
        // SAFETY: a call on a live interface with a live BSTR.
        match self
            .folder(r"\")
            .and_then(|root| unsafe { root.DeleteFolder(&BSTR::from(parent.trim_start_matches('\\')), 0) })
        {
            Err(e) if !not_found(&e) && !is(&e, ERROR_DIR_NOT_EMPTY) => Err(failed(e)),
            _ => Ok(()),
        }
    }

    /// The folder `parent`, made if it is not there.
    fn ensure_folder(&self, parent: &str) -> Result<()> {
        let failed = |e| Error::os(format!("create the folder `{parent}`"), e);
        match self.folder(parent) {
            Ok(_) => return Ok(()),
            Err(e) if not_found(&e) => {}
            Err(e) => return Err(failed(e)),
        }
        // SAFETY: calls on live interfaces with live strings.
        let made = self.folder(r"\").and_then(|root| unsafe {
            root.CreateFolder(&BSTR::from(parent.trim_start_matches('\\')), &VARIANT::default())
        });
        match made {
            Ok(_) => Ok(()),
            // Another writer made it first.
            Err(e) if is(&e, ERROR_ALREADY_EXISTS) => Ok(()),
            Err(e) => Err(failed(e)),
        }
    }
}

fn is(error: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
    error.code() == HRESULT::from_win32(code.0)
}

fn not_found(error: &windows::core::Error) -> bool {
    is(error, ERROR_FILE_NOT_FOUND) || is(error, ERROR_PATH_NOT_FOUND)
}

/// `read` of one item of a listing, as `None` if the item went after the listing was counted: such a
/// listing is a snapshot without it, not a failure. Any other failure is the read's.
///
/// A task that went reads "not found" ([`not_found`]).
fn unless_gone<T>(read: windows::core::Result<T>) -> windows::core::Result<Option<T>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `read` of an instance's `EnginePID`, as `None` if the instance has ended: the entry stays in the
/// listing a moment after the run, and no longer offers its pid. Any other failure is the read's.
///
/// Measured (KB note "A listed instance's pid was always the action's own process, and the entry
/// became unreadable before the pid was freed"): the end of a run is the only failure the read has,
/// always `SCHED_E_TASK_NOT_RUNNING`.
fn unless_ended<T>(read: windows::core::Result<T>) -> windows::core::Result<Option<T>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.code() == SCHED_E_TASK_NOT_RUNNING => Ok(None),
        Err(e) => Err(e),
    }
}

fn not_installed(path: &TaskPath) -> Error {
    Error::NotInstalled {
        id: path.name().to_owned(),
    }
}

/// The folder part and the name of the task at `path`: `\Goetia\x` is (`\Goetia`, `x`).
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('\\').unwrap_or(("", path))
}

/// What each listed instance tells: its GUID text and its process. An instance that ended while the
/// listing was read is not in it ([`unless_ended`]).
fn listed(task: &IRegisteredTask) -> windows::core::Result<Vec<(IRunningTask, String, u32)>> {
    // SAFETY: calls on live interfaces; collection items are numbered from 1.
    unsafe {
        let running = task.GetInstances(0)?;
        let mut found = Vec::new();
        for index in 1..=running.Count()? {
            let instance = running.get_Item(&VARIANT::from(index))?;
            let guid = instance.InstanceGuid()?.to_string();
            if let Some(pid) = unless_ended(instance.EnginePID())? {
                found.push((instance, guid, pid));
            }
        }
        Ok(found)
    }
}

fn instance_id(guid: &str) -> Result<InstanceId> {
    InstanceId::parse(guid).ok_or_else(|| Error::os("read an instance", format!("`{guid}` is not a GUID")))
}

/// The instance a listing entry names. L1: a listed instance always had a process, so a pid of 0 is
/// a failed read, never a run that is alive.
fn instance_of(guid: &str, pid: u32) -> Result<Instance> {
    if pid == 0 {
        return Err(Error::os(
            "read an instance",
            format!("the listing gives `{guid}` no process"),
        ));
    }
    Ok(Instance {
        id: instance_id(guid)?,
        pid,
    })
}

/// The process a `Run` started: `None` for a `Run` that started nothing (an action that cannot start
/// returns a GUID and no process, M7), read as a pid of 0 or as the instance not running.
fn started_pid(read: windows::core::Result<u32>) -> windows::core::Result<Option<u32>> {
    Ok(unless_ended(read)?.filter(|&pid| pid != 0))
}

// The trait -----------------------------------------------------------------------------------------------------------

impl Com {
    fn register_at(
        &self,
        path: &str,
        xml: &str,
        who: &TaskAccount,
        password: Option<&str>,
        update: bool,
    ) -> Result<Registration> {
        let session = Session::open()?;
        let (parent, _) = split(path);
        debug_assert!(
            parent.matches('\\').count() == 1,
            "`{path}` is in a folder one level below the root"
        );
        if !update {
            session.ensure_folder(parent)?;
        }
        // M3: an update of a user's task with no password asks to keep its stored credentials.
        let (user, secret, logon) = match (who, password) {
            (TaskAccount::Service(name), _) => (name, VARIANT::default(), TASK_LOGON_SERVICE_ACCOUNT),
            (TaskAccount::User(name), Some(password)) => (name, VARIANT::from(password), TASK_LOGON_PASSWORD),
            (TaskAccount::User(name), None) => (name, VARIANT::default(), TASK_LOGON_NONE),
        };
        let flags = if update { TASK_UPDATE } else { TASK_CREATE };
        // SAFETY: calls on live interfaces with live strings and VARIANTs.
        let registered = session.folder(r"\").and_then(|root| unsafe {
            root.RegisterTask(
                &BSTR::from(path),
                &BSTR::from(xml),
                flags.0,
                &VARIANT::from(user.as_str()),
                &secret,
                logon,
                &VARIANT::default(),
            )
        });
        match registered {
            Ok(_) => Ok(Registration::Created),
            // M10: a create of an existing path says it exists; an update of a missing one, that it is not
            // found.
            Err(e) if !update && is(&e, ERROR_ALREADY_EXISTS) => Ok(Registration::Raced),
            Err(e) if update && not_found(&e) => Ok(Registration::Raced),
            Err(e) => Err(Error::os(format!("register the task `{path}`"), e)),
        }
    }

    fn delete_at(&self, path: &str) -> Result<bool> {
        let session = Session::open()?;
        let (parent, name) = split(path);
        let failed = |e| Error::os(format!("delete the task `{path}`"), e);
        let folder = match session.folder(parent) {
            Ok(folder) => folder,
            Err(e) if not_found(&e) => return Ok(false),
            Err(e) => return Err(failed(e)),
        };
        // SAFETY: a call on a live interface with a live BSTR.
        let existed = match unsafe { folder.DeleteTask(&BSTR::from(name), 0) } {
            Ok(()) => true,
            Err(e) if not_found(&e) => false,
            Err(e) => return Err(failed(e)),
        };
        match (existed, session.remove_folder(parent)) {
            (_, Ok(())) => Ok(existed),
            // The task is gone: only the empty folder is left, which is its own error, not the delete's.
            (true, Err(e)) => Err(Error::TaskFolderKept {
                task: path.to_owned(),
                folder: parent.to_owned(),
                source: Box::new(e),
            }),
            (false, Err(e)) => Err(e),
        }
    }

    /// The times `path` is to run in the window `(from, to]`, Task Scheduler's own answer
    /// (`GetRunTimes`). Both ends and every time returned are civil times in the machine's zone, as
    /// every other date of Task Scheduler (M2), converted through the OS's own rules. Production never
    /// asks it; the schedule tests compare it with the cron they translated.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn run_times(&self, path: &TaskPath, from: SystemTime, to: SystemTime) -> Result<Vec<SystemTime>> {
        let range = |time| {
            system_to_local(time).ok_or_else(|| Error::os("convert a time", format!("{time:?} is out of range")))
        };
        // M2: the range and the times it gives are local civil times, as every other date of Task Scheduler.
        let (start, end) = (range(from)?, range(to)?);
        let session = Session::open()?;
        let times = session
            .with_task(path.as_str(), "list the run times of", |task| {
                let mut count = u32::MAX;
                let mut times: *mut SYSTEMTIME = std::ptr::null_mut();
                // SAFETY: the pointers are to live values; the array the call returns holds `count`
                // times and is ours to free.
                unsafe {
                    task.GetRunTimes(&start, &end, &mut count, &mut times)?;
                    let found = if times.is_null() {
                        Vec::new()
                    } else {
                        let found = std::slice::from_raw_parts(times, count as usize).to_vec();
                        CoTaskMemFree(Some(times.cast_const().cast()));
                        found
                    };
                    Ok(found)
                }
            })?
            .ok_or_else(|| not_installed(path))?;
        times
            .iter()
            .map(|time| local_to_system(time).ok_or_else(|| Error::os("convert a run time", format!("{time:?}"))))
            .collect()
    }
}

impl TaskApi for Com {
    fn get(&self, path: &TaskPath) -> Result<Option<Registered>> {
        let session = Session::open()?;
        // A "not found" from the read of a task the lookup found means it went since: the lookup's own
        // answer. The operations (`stop`, `run`, instance reads) stay strict in `with_task`.
        let read = session.with_task(path.as_str(), "read the task", |task| {
            let read = || {
                // SAFETY: calls on live interfaces; `Data` fills the BSTR it is given.
                unsafe {
                    let xml = task.Xml()?.to_string();
                    let mut data = BSTR::new();
                    task.Definition()?.Data(&mut data)?;
                    Ok(Registered {
                        xml,
                        data: data.to_string(),
                    })
                }
            };
            unless_gone(read())
        });
        read.map(Option::flatten)
    }

    fn names(&self) -> Result<Vec<String>> {
        let session = Session::open()?;
        let failed = |e| Error::os(format!("list the tasks in `{TASK_FOLDER}`"), e);
        let folder = match session.folder(TASK_FOLDER) {
            Ok(folder) => folder,
            Err(e) if not_found(&e) => return Ok(Vec::new()),
            Err(e) => return Err(failed(e)),
        };
        // SAFETY: calls on live interfaces; collection items are numbered from 1.
        unsafe {
            let tasks = folder.GetTasks(TASK_ENUM_HIDDEN.0).map_err(failed)?;
            let mut names = Vec::new();
            for index in 1..=tasks.Count().map_err(failed)? {
                // A task deleted since it was counted is not in the listing.
                let name = tasks.get_Item(&VARIANT::from(index)).and_then(|task| task.Name());
                names.extend(unless_gone(name).map_err(failed)?.map(|name| name.to_string()));
            }
            Ok(names)
        }
    }

    fn register(
        &self,
        path: &TaskPath,
        xml: &str,
        who: &TaskAccount,
        password: Option<&str>,
        update: bool,
    ) -> Result<Registration> {
        self.register_at(path.as_str(), xml, who, password, update)
    }

    fn set_enabled(&self, path: &TaskPath, enabled: bool) -> Result<()> {
        let session = Session::open()?;
        session
            .with_task(path.as_str(), "set the enabled flag of", |task| {
                // SAFETY: a call on a live interface.
                unsafe { task.SetEnabled(enabled.into()) }
            })?
            .ok_or_else(|| not_installed(path))
    }

    fn delete(&self, path: &TaskPath) -> Result<bool> {
        self.delete_at(path.as_str())
    }

    fn instances(&self, path: &TaskPath) -> Result<Vec<Instance>> {
        let session = Session::open()?;
        let found = session
            .with_task(path.as_str(), "list the instances of", listed)?
            .ok_or_else(|| not_installed(path))?;
        found
            .into_iter()
            .map(|(_, guid, pid)| instance_of(&guid, pid))
            .collect()
    }

    fn info(&self, path: &TaskPath) -> Result<TaskInfo> {
        let session = Session::open()?;
        session
            .with_task(path.as_str(), "read the runs of", |task| {
                // SAFETY: calls on a live interface.
                unsafe {
                    Ok(TaskInfo {
                        last_run: ole_date(task.LastRunTime()?),
                        last_result: task.LastTaskResult()?,
                        next_run: ole_date(task.NextRunTime()?),
                    })
                }
            })?
            .ok_or_else(|| not_installed(path))
    }

    /// `Run`, never `RunEx` with `TASK_RUN_IGNORE_CONSTRAINTS`: that flag bypasses `IgnoreNew`, which
    /// is what keeps a second instance from starting beside a live one.
    fn run(&self, path: &TaskPath) -> Result<Started> {
        let session = Session::open()?;
        let (guid, pid) = session
            .with_task(path.as_str(), "start", |task| {
                // SAFETY: calls on live interfaces; no parameters for the action.
                unsafe {
                    let instance = task.Run(&VARIANT::default())?;
                    Ok((instance.InstanceGuid()?.to_string(), started_pid(instance.EnginePID())?))
                }
            })
            .map_err(|e| disabled_run(e, path.name()))?
            .ok_or_else(|| not_installed(path))?;
        Ok(Started {
            id: instance_id(&guid)?,
            pid,
        })
    }

    fn stop(&self, path: &TaskPath, instance: &InstanceId) -> Result<()> {
        let session = Session::open()?;
        session
            .with_task(path.as_str(), "stop an instance of", |task| {
                for (running, guid, _) in listed(task)? {
                    if InstanceId::parse(&guid).as_ref() == Some(instance) {
                        // SAFETY: a call on a live interface.
                        unsafe { running.Stop()? };
                    }
                }
                Ok(())
            })?
            .ok_or_else(|| not_installed(path))
    }
}

#[cfg(test)]
#[path = "com_tests.rs"]
mod com_tests;
