//! What the Windows task suites share: the task specs they install, and the guard that ends a run for
//! a test that fails while a `run` thread waits for it.

use std::path::Path;

use goetia::backend::scm::manager::Scm;
use goetia::spec::{DaemonSpec, Id, TaskSpec, User};
use goetia::testing::{remove_task, task};

use super::{WorldReadableExe, shim_manager_at};

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
