//! What the Windows task suites share: the task specs they install, and the guard that ends a run for
//! a test that fails while a `run` thread waits for it.

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use goetia::backend::scm::manager::Scm;
use goetia::manager::{ServiceManager as _, State, Status, TaskStatus};
use goetia::spec::{DaemonSpec, Id, TaskSpec, User};
use goetia::testing::{remove_task, task};

use super::{WorldReadableExe, shim_manager_at, wait};

/// What a task of an account that cannot run the build tree needs (`LocalService`, a local user): a copy
/// of this binary and a copy of the shim, each executable by every account. Declare it before the guard
/// of the task, so the copies are removed after the task is.
pub struct ServiceAccountHost {
    pub exe: WorldReadableExe,
    shim: WorldReadableExe,
}

impl ServiceAccountHost {
    /// The copies, named after the task `id`.
    pub fn new(id: &Id) -> Self {
        Self {
            exe: WorldReadableExe::new(id.as_str()),
            shim: WorldReadableExe::of(
                Path::new(env!("CARGO_BIN_EXE_goetia-shim")),
                &format!("{}-shim", id.as_str()),
            ),
        }
    }

    /// A manager whose tasks run the shim copy.
    pub fn manager(&self) -> Scm {
        shim_manager_at(self.shim.path_str().into())
    }

    /// The path of the shim copy, for a child process that makes its own manager.
    pub fn shim_path(&self) -> String {
        self.shim.path_str()
    }
}

/// A copy of this test binary that an install accepts as a service host: it carries the version marker, which a
/// test binary built from the library alone does not. What it does when SCM starts it is the test's.
pub fn this_binary_as_a_service_host(name: &str) -> WorldReadableExe {
    use std::io::Write as _;

    let host = WorldReadableExe::new(name);
    std::fs::OpenOptions::new()
        .append(true)
        .open(host.path_str())
        .and_then(|mut file| file.write_all(&goetia::testing::host_marker()))
        .unwrap_or_else(|e| panic!("append the host marker to {}: {e}", host.path_str()));
    host
}

/// A manager whose `protocol: plain` services run `host` with `args`.
pub fn manager_hosted_by(host: &WorldReadableExe, args: Vec<String>) -> Scm {
    Scm::with_shim(
        goetia::shim::StandaloneShim::new(host.path_str().into(), args),
        goetia::shim::CheckPermissions::Disable,
    )
}

/// The task `id`, run by `user`, that starts `command` on `schedule` (`None`: only a test starts it)
/// and sends its output to `logs` where it has one.
pub fn task_spec(id: &Id, command: Vec<String>, schedule: Option<&str>, user: User, logs: Option<&Path>) -> DaemonSpec {
    let DaemonSpec::Task(spec) = task(id.as_str(), command, schedule) else {
        unreachable!("`testing::task` builds a task")
    };
    let mut fields = spec.into_fields();
    fields.common.user = user;
    fields.common.logs = logs.map(Path::to_path_buf);
    TaskSpec::new(fields).map(DaemonSpec::from).expect("a valid task")
}

/// The status of the task `id`; a service of that id is a failed test.
pub fn task_status(mgr: &Scm, id: &Id) -> TaskStatus {
    match mgr.status(id).expect("status of a task") {
        Status::Task(status) => status,
        Status::Service(status) => panic!("`{id}` reads as a service: {status:?}"),
    }
}

/// The status of `id` once its run has ended and Task Scheduler has recorded how: idle, with a last
/// result. Read again with no cap, since the run's end is a state of the machine and not a time. For a
/// task that has not run before the one waited for.
pub fn status_once_ended(mgr: &Scm, id: &Id) -> TaskStatus {
    let mut found = None;
    wait::until(|| {
        let status = task_status(mgr, id);
        let ended = status.common.state == State::Idle && status.last_result.is_some();
        found = Some(status);
        ended
    });
    found.expect("a status was read")
}

/// An open handle to a process, which keeps its pid from being given to another, to ask whether it has
/// exited. Task Scheduler's own handle to a shim outlives the shim until it drops the run from its listing
/// (M5), so a process that exited still resolves by its pid: only a handle says it ended.
pub struct ProcessHandle(OwnedHandle);

impl ProcessHandle {
    /// Opens the process `pid`, which the caller knows to be running: a process that cannot be opened
    /// fails the test.
    pub fn open(pid: u32) -> Self {
        // SAFETY: a plain call; the handle it returns is checked and then owned.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(
            !raw.is_null(),
            "OpenProcess({pid}): {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `raw` is a handle just returned and nothing else owns.
        Self(unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
    }

    /// Whether the process has exited, read without waiting: a state read, not a wait.
    pub fn has_exited(&self) -> bool {
        // SAFETY: the handle is live with `SYNCHRONIZE` access.
        unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }

    /// Waits until the process has exited, with no cap: its exit is the state waited for, so a process that never
    /// exits hangs the test. Returns at once for one that has.
    pub fn wait_for_exit(&self) {
        // SAFETY: the handle is live with `SYNCHRONIZE` access.
        let waited = unsafe { WaitForSingleObject(self.0.as_raw_handle(), INFINITE) };
        assert_eq!(waited, WAIT_OBJECT_0, "{}", std::io::Error::last_os_error());
    }
}

/// What ends a run for a test that fails while a `run` thread waits for it. A `thread::scope` joins its
/// threads when the body panics, and the `run` thread waits on a live shim, so without this a failing
/// assertion in the scope is a hang that leaves the task and its program running. Made right after the
/// thread is spawned; dropped, it removes the task, which stops its run and waits for the shim to exit,
/// so the thread returns. [`Self::defuse`] once the test has ended the run itself.
pub struct EndsTheRunOnPanic<'a>(Option<&'a Id>);

impl<'a> EndsTheRunOnPanic<'a> {
    pub fn new(id: &'a Id) -> Self {
        Self(Some(id))
    }

    pub fn defuse(mut self) {
        self.0 = None;
    }
}

impl Drop for EndsTheRunOnPanic<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            remove_task(id);
        }
    }
}
