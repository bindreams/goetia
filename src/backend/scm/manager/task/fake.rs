//! A test Task Scheduler: a table of stated replies and the list of calls made, and nothing else. It
//! models no scheduler (it does not know that a task it was told to register is there to read); a
//! test states what each call it expects answers, in the order each method is called.
//!
//! A call with no stated reply is recorded and panics. [`Fake::finish`] then fails the test for that
//! call and for every reply never used, so a panic swallowed on a thread the code under test made
//! still fails the test.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::api::{Instance, Registered, Registration, Started, TaskApi, TaskInfo};
use crate::backend::scm::instance::{InstanceId, TaskAccount, TaskPath};
use crate::error::Result;

/// One call on the boundary, with the arguments it was made with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Get(String),
    Names,
    Register {
        path: String,
        xml: String,
        who: TaskAccount,
        password: Option<String>,
        update: bool,
    },
    SetEnabled(String, bool),
    Delete(String),
    Instances(String),
    Info(String),
    Run(String),
    Stop(String, InstanceId),
}

#[derive(Default)]
struct Script {
    calls: Vec<Call>,
    unstated: Vec<Call>,
    get: VecDeque<Result<Option<Registered>>>,
    names: VecDeque<Result<Vec<String>>>,
    register: VecDeque<Result<Registration>>,
    set_enabled: VecDeque<Result<()>>,
    delete: VecDeque<Result<bool>>,
    instances: VecDeque<Result<Vec<Instance>>>,
    info: VecDeque<Result<TaskInfo>>,
    run: VecDeque<Result<Started>>,
    stop: VecDeque<Result<()>>,
}

/// The test [`TaskApi`]. Clones share one script, so a test keeps a handle after the manager takes one.
#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<Script>>);

/// Poison is recovered from: the `Fake` panics for an unstated call, and the calls after it must
/// still be recorded.
fn lock(script: &Mutex<Script>) -> MutexGuard<'_, Script> {
    script.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fake {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The calls made so far, in order, unstated ones included.
    pub(crate) fn calls(&self) -> Vec<Call> {
        lock(&self.0).calls.clone()
    }

    /// Fails the test if a call had no stated reply or a stated reply was never used.
    pub(crate) fn finish(&self) {
        let script = lock(&self.0);
        assert!(
            script.unstated.is_empty(),
            "calls with no stated reply: {:?}",
            script.unstated
        );
        let unused = [
            ("get", script.get.len()),
            ("names", script.names.len()),
            ("register", script.register.len()),
            ("set_enabled", script.set_enabled.len()),
            ("delete", script.delete.len()),
            ("instances", script.instances.len()),
            ("info", script.info.len()),
            ("run", script.run.len()),
            ("stop", script.stop.len()),
        ];
        let unused: Vec<_> = unused.into_iter().filter(|(_, left)| *left > 0).collect();
        assert!(unused.is_empty(), "stated replies never used: {unused:?}");
    }

    pub(crate) fn on_get(&self, reply: Result<Option<Registered>>) -> &Self {
        lock(&self.0).get.push_back(reply);
        self
    }

    pub(crate) fn on_names(&self, reply: Result<Vec<String>>) -> &Self {
        lock(&self.0).names.push_back(reply);
        self
    }

    pub(crate) fn on_register(&self, reply: Result<Registration>) -> &Self {
        lock(&self.0).register.push_back(reply);
        self
    }

    pub(crate) fn on_set_enabled(&self, reply: Result<()>) -> &Self {
        lock(&self.0).set_enabled.push_back(reply);
        self
    }

    pub(crate) fn on_delete(&self, reply: Result<bool>) -> &Self {
        lock(&self.0).delete.push_back(reply);
        self
    }

    pub(crate) fn on_instances(&self, reply: Result<Vec<Instance>>) -> &Self {
        lock(&self.0).instances.push_back(reply);
        self
    }

    pub(crate) fn on_info(&self, reply: Result<TaskInfo>) -> &Self {
        lock(&self.0).info.push_back(reply);
        self
    }

    pub(crate) fn on_run(&self, reply: Result<Started>) -> &Self {
        lock(&self.0).run.push_back(reply);
        self
    }

    pub(crate) fn on_stop(&self, reply: Result<()>) -> &Self {
        lock(&self.0).stop.push_back(reply);
        self
    }

    /// Records `call` and takes the next reply `queue` holds, or records it as unstated and panics.
    fn answer<T>(&self, call: Call, queue: impl FnOnce(&mut Script) -> &mut VecDeque<Result<T>>) -> Result<T> {
        let mut script = lock(&self.0);
        script.calls.push(call.clone());
        match queue(&mut script).pop_front() {
            Some(reply) => reply,
            None => {
                script.unstated.push(call.clone());
                drop(script);
                panic!("no reply stated for {call:?}");
            }
        }
    }
}

impl TaskApi for Fake {
    fn get(&self, path: &TaskPath) -> Result<Option<Registered>> {
        self.answer(Call::Get(path.as_str().to_owned()), |s| &mut s.get)
    }

    fn names(&self) -> Result<Vec<String>> {
        self.answer(Call::Names, |s| &mut s.names)
    }

    fn register(
        &self,
        path: &TaskPath,
        xml: &str,
        who: &TaskAccount,
        password: Option<&str>,
        update: bool,
    ) -> Result<Registration> {
        let call = Call::Register {
            path: path.as_str().to_owned(),
            xml: xml.to_owned(),
            who: who.clone(),
            password: password.map(str::to_owned),
            update,
        };
        self.answer(call, |s| &mut s.register)
    }

    fn set_enabled(&self, path: &TaskPath, enabled: bool) -> Result<()> {
        self.answer(Call::SetEnabled(path.as_str().to_owned(), enabled), |s| {
            &mut s.set_enabled
        })
    }

    fn delete(&self, path: &TaskPath) -> Result<bool> {
        self.answer(Call::Delete(path.as_str().to_owned()), |s| &mut s.delete)
    }

    fn instances(&self, path: &TaskPath) -> Result<Vec<Instance>> {
        self.answer(Call::Instances(path.as_str().to_owned()), |s| &mut s.instances)
    }

    fn info(&self, path: &TaskPath) -> Result<TaskInfo> {
        self.answer(Call::Info(path.as_str().to_owned()), |s| &mut s.info)
    }

    fn run(&self, path: &TaskPath) -> Result<Started> {
        self.answer(Call::Run(path.as_str().to_owned()), |s| &mut s.run)
    }

    fn stop(&self, path: &TaskPath, instance: &InstanceId) -> Result<()> {
        self.answer(Call::Stop(path.as_str().to_owned(), *instance), |s| &mut s.stop)
    }
}
