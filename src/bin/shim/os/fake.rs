//! A test OS: tables of stated replies, with no process, thread pool or file behind them. It models
//! no operating system; the one rule it encodes is that a successful kill ends the process it
//! killed, so a blocked [`WaitReply::Blocks`] returns.
//!
//! A test states the replies it expects, in the order each operation will be called. A call with no
//! stated reply is recorded and panics; the [`FakeGuard`] then fails the test at its end for that
//! call, for every reply never consumed, for every failure or warning line never read and for every
//! waiter thread whose end was never read, so a panic swallowed on a thread the supervisor made
//! still fails the test.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::ops::Deref;
use std::os::windows::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use cosca::Containment;

use super::{Channels, Os, Spawned};
use crate::logging::Severity;

/// The error a refused thread fails with.
pub(crate) const NO_THREAD: &str = "no thread may be made (fake)";
/// The error a refused kill fails with.
pub(crate) const KILL_REFUSED: &str = "the kill was refused (fake)";
/// The error a [`WaitReply::Fails`] with no OS code fails with.
pub(crate) const WAIT_FAILED: &str = "the wait failed (fake)";
/// The error a failing append fails with.
pub(crate) const APPEND_FAILED: &str = "the append failed (fake)";

/// Poison is recovered from: a panicking thread must not make every later call on the `Fake` panic.
/// Nothing here panics with a lock held.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An operation the `Fake` counts, whether or not it was given a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Op {
    Spawn,
    Thread,
    Append,
    EventLog,
    Containment,
    Wait,
    Kill,
    KillTree,
    Detach,
}

/// What a [`Spawned::wait`] does.
pub(crate) enum WaitReply {
    /// Return at once: the process has exited with this code.
    Exits(i32),
    /// Block until the process is ended, by a successful kill or [`FakeProcess::end`], and return
    /// its status.
    Blocks,
    /// Fail, with this OS error code or, with none, [`WAIT_FAILED`].
    Fails(Option<i32>),
    /// Panic, ending the waiter thread without it reporting the child done.
    Panics,
}

/// What a [`Spawned::kill`] or [`Spawned::kill_tree`] does.
pub(crate) enum KillReply {
    /// Succeed. The process is ended.
    Ends,
    /// Fail with [`KILL_REFUSED`]. The process is not ended.
    Refuses,
    Panics,
}

/// How a thread made through [`Os::thread`] ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaiterEnd {
    Returned,
    Panicked,
}

/// One call on the [`Channels`], in the order made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelCall {
    Append { path: PathBuf, line: String, ok: bool },
    EventLog { severity: Severity, line: String },
}

// Process -------------------------------------------------------------------------------------------------------------

/// A process the `Fake` can spawn, with its own stated replies. The handle the test keeps and the
/// one the `Fake` spawns share state, so the test reads what the supervisor did to it.
#[derive(Clone)]
pub(crate) struct FakeProcess(Arc<Proc>);

struct Proc {
    state: Mutex<ProcState>,
    ended: Condvar,
}

struct ProcState {
    containment: Containment,
    waits: VecDeque<WaitReply>,
    kills: VecDeque<KillReply>,
    tree_kills: VecDeque<KillReply>,
    exit: Option<ExitStatus>,
    killed: bool,
    dropped: bool,
    detached: bool,
}

fn status(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code as u32)
}

impl Proc {
    fn end(&self, status: ExitStatus) {
        lock(&self.state).exit.get_or_insert(status);
        self.ended.notify_all();
    }

    /// Block until the process is ended; its status.
    fn wait_ended(&self) -> ExitStatus {
        let state = self
            .ended
            .wait_while(lock(&self.state), |state| state.exit.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.exit.expect("`wait_while` returned with the process not ended")
    }
}

impl FakeProcess {
    /// A job-contained process with no replies stated.
    pub(crate) fn new() -> FakeProcess {
        FakeProcess(Arc::new(Proc {
            state: Mutex::new(ProcState {
                containment: Containment::JobObject,
                waits: VecDeque::new(),
                kills: VecDeque::new(),
                tree_kills: VecDeque::new(),
                exit: None,
                killed: false,
                dropped: false,
                detached: false,
            }),
            ended: Condvar::new(),
        }))
    }

    fn with(self, change: impl FnOnce(&mut ProcState)) -> FakeProcess {
        change(&mut lock(&self.0.state));
        self
    }

    pub(crate) fn containment(self, containment: Containment) -> FakeProcess {
        self.with(|state| state.containment = containment)
    }

    /// The reply to the next `wait` not yet stated for.
    pub(crate) fn wait(self, reply: WaitReply) -> FakeProcess {
        self.with(|state| state.waits.push_back(reply))
    }

    /// A process that exits on its own with `code`: the two waits the supervisor makes on one, the
    /// waiter's and the one that reads the code.
    pub(crate) fn exited(self, code: i32) -> FakeProcess {
        self.wait(WaitReply::Exits(code)).wait(WaitReply::Exits(code))
    }

    pub(crate) fn kill(self, reply: KillReply) -> FakeProcess {
        self.with(|state| state.kills.push_back(reply))
    }

    pub(crate) fn kill_tree(self, reply: KillReply) -> FakeProcess {
        self.with(|state| state.tree_kills.push_back(reply))
    }

    /// End the process by hand, as it would end on its own: the part of a test that does what the
    /// supervisor deliberately did not.
    pub(crate) fn end(&self, code: i32) {
        self.0.end(status(code));
    }

    pub(crate) fn ended(&self) -> bool {
        lock(&self.0.state).exit.is_some()
    }

    /// Whether a kill or tree kill of it succeeded.
    pub(crate) fn killed(&self) -> bool {
        lock(&self.0.state).killed
    }

    /// Whether the supervisor's instance of it was dropped, without being detached.
    pub(crate) fn dropped(&self) -> bool {
        lock(&self.0.state).dropped
    }

    pub(crate) fn detached(&self) -> bool {
        lock(&self.0.state).detached
    }
}

/// The process as the supervisor holds it.
struct Instance {
    proc: Arc<Proc>,
    fake: Arc<Shared>,
    detached: bool,
}

impl Instance {
    /// A call with no stated reply: recorded for the `FakeGuard`, then a panic.
    fn unstated(&self, what: &str) -> ! {
        lock(&self.fake.state).unstated.push(format!("{what} on a process"));
        panic!("unstated call: {what} on a process");
    }

    fn kill_with(&self, what: &str, reply: Option<KillReply>) -> Result<(), cosca::error::Error> {
        match reply {
            None => self.unstated(what),
            Some(KillReply::Ends) => {
                lock(&self.proc.state).killed = true;
                self.proc.end(status(1));
                Ok(())
            }
            Some(KillReply::Refuses) => Err(cosca::error::Error::Io(io::Error::other(KILL_REFUSED))),
            Some(KillReply::Panics) => panic!("the {what} panicked (fake)"),
        }
    }
}

impl Spawned for Instance {
    fn containment(&self) -> Containment {
        self.fake.count(Op::Containment);
        lock(&self.proc.state).containment
    }

    fn wait(&self) -> Result<ExitStatus, cosca::error::Error> {
        self.fake.count(Op::Wait);
        let reply = lock(&self.proc.state).waits.pop_front();
        match reply {
            None => self.unstated("wait"),
            Some(WaitReply::Exits(code)) => {
                self.proc.end(status(code));
                Ok(status(code))
            }
            Some(WaitReply::Blocks) => Ok(self.proc.wait_ended()),
            Some(WaitReply::Fails(Some(code))) => Err(cosca::error::Error::Io(io::Error::from_raw_os_error(code))),
            Some(WaitReply::Fails(None)) => Err(cosca::error::Error::Io(io::Error::other(WAIT_FAILED))),
            Some(WaitReply::Panics) => panic!("the wait panicked (fake)"),
        }
    }

    fn kill(&self) -> Result<(), cosca::error::Error> {
        self.fake.count(Op::Kill);
        let reply = lock(&self.proc.state).kills.pop_front();
        self.kill_with("kill", reply)
    }

    fn kill_tree(&self) -> Result<(), cosca::error::Error> {
        self.fake.count(Op::KillTree);
        let reply = lock(&self.proc.state).tree_kills.pop_front();
        self.kill_with("kill_tree", reply)
    }

    fn detach(mut self: Box<Self>) {
        self.fake.count(Op::Detach);
        self.detached = true;
        lock(&self.proc.state).detached = true;
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.detached {
            lock(&self.proc.state).dropped = true;
        }
    }
}

// Fake ----------------------------------------------------------------------------------------------------------------

enum SpawnReply {
    Process(FakeProcess),
    Refuses(cosca::error::Error),
}

enum ThreadEnd {
    Running,
    Ended(WaiterEnd),
    Read,
}

#[derive(Default)]
struct State {
    spawns: VecDeque<SpawnReply>,
    /// `true` allows the thread, `false` refuses it.
    threads: VecDeque<bool>,
    append_failures: Vec<PathBuf>,
    calls: BTreeMap<Op, usize>,
    channel_calls: Vec<ChannelCall>,
    processes: Vec<Arc<Proc>>,
    /// One entry per thread made, in order.
    thread_ends: Vec<ThreadEnd>,
    unstated: Vec<String>,
}

struct Shared {
    state: Mutex<State>,
    thread_ended: Condvar,
}

impl Shared {
    fn count(&self, op: Op) {
        *lock(&self.state).calls.entry(op).or_default() += 1;
    }
}

/// The test's `Os` and `Channels`. Clones share one table and one record, so the supervisor and a
/// `Log` hold handles and the test keeps one.
#[derive(Clone)]
pub(crate) struct Fake {
    shared: Arc<Shared>,
}

/// The test's owner of a [`Fake`]: not `Clone`, so its drop is the end of the test. It ends every
/// process, so a waiter thread the supervisor detached returns, and fails the test on whatever
/// [`Fake`] says went unstated or unread, unless the test is already panicking.
pub(crate) struct FakeGuard(Fake);

impl FakeGuard {
    pub(crate) fn new() -> FakeGuard {
        FakeGuard(Fake {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                thread_ended: Condvar::new(),
            }),
        })
    }
}

impl Deref for FakeGuard {
    type Target = Fake;

    fn deref(&self) -> &Fake {
        &self.0
    }
}

impl Drop for FakeGuard {
    fn drop(&mut self) {
        let processes = lock(&self.0.shared.state).processes.clone();
        for process in &processes {
            process.end(status(1));
        }
        if !std::thread::panicking() {
            self.0.assert_spent(&processes);
        }
    }
}

impl Fake {
    pub(crate) fn os(&self) -> Arc<dyn Os> {
        Arc::new(self.clone())
    }

    // Tables -----------------------------------------------------------------------------------------------------

    /// The next `spawn` succeeds with `process`.
    pub(crate) fn will_spawn(&self, process: FakeProcess) {
        lock(&self.shared.state).spawns.push_back(SpawnReply::Process(process));
    }

    /// The next `spawn` fails with `error`.
    pub(crate) fn will_refuse_spawn(&self, error: cosca::error::Error) {
        lock(&self.shared.state).spawns.push_back(SpawnReply::Refuses(error));
    }

    /// The next `thread` runs its closure on a real thread. Its end is read with [`Fake::waiter_end`],
    /// numbered in the order threads are made.
    pub(crate) fn will_make_thread(&self) {
        lock(&self.shared.state).threads.push_back(true);
    }

    /// The next `thread` fails with [`NO_THREAD`]. A refused thread has no end to read.
    pub(crate) fn will_refuse_thread(&self) {
        lock(&self.shared.state).threads.push_back(false);
    }

    /// The next `append` to `path` fails with [`APPEND_FAILED`].
    pub(crate) fn will_fail_append(&self, path: &Path) {
        lock(&self.shared.state).append_failures.push(path.to_path_buf());
    }

    // Records ----------------------------------------------------------------------------------------------------

    /// How many times `op` was called, replied to or not. Readable from inside a callback of the
    /// code under test.
    pub(crate) fn calls(&self, op: Op) -> usize {
        lock(&self.shared.state).calls.get(&op).copied().unwrap_or(0)
    }

    /// Block until thread `n` has ended, and say how. Everything that thread did happens-before
    /// this returns.
    pub(crate) fn waiter_end(&self, n: usize) -> WaiterEnd {
        let mut state = self
            .shared
            .thread_ended
            .wait_while(lock(&self.shared.state), |state| {
                matches!(state.thread_ends.get(n), Some(ThreadEnd::Running))
            })
            .unwrap_or_else(PoisonError::into_inner);
        let end = match state.thread_ends.get_mut(n) {
            Some(end @ ThreadEnd::Ended(_)) => std::mem::replace(end, ThreadEnd::Read),
            Some(_) => {
                drop(state);
                panic!("thread {n}'s end was already read");
            }
            None => {
                drop(state);
                panic!("no thread {n} was made");
            }
        };
        let ThreadEnd::Ended(end) = end else {
            unreachable!("matched `Ended` above")
        };
        end
    }

    /// The failure lines reported to the Event Log, oldest first, removed.
    pub(crate) fn take_failures(&self) -> Vec<String> {
        self.take_event_log(Severity::Error)
    }

    /// The warning lines reported to the Event Log, oldest first, removed.
    pub(crate) fn take_warnings(&self) -> Vec<String> {
        self.take_event_log(Severity::Warning)
    }

    fn take_event_log(&self, wanted: Severity) -> Vec<String> {
        let mut taken = Vec::new();
        lock(&self.shared.state).channel_calls.retain(|call| match call {
            ChannelCall::EventLog { severity, line } if *severity == wanted => {
                taken.push(line.clone());
                false
            }
            _ => true,
        });
        taken
    }

    /// Every call on the channels, in the order made, removed.
    pub(crate) fn take_channel_calls(&self) -> Vec<ChannelCall> {
        std::mem::take(&mut lock(&self.shared.state).channel_calls)
    }

    fn assert_spent(&self, processes: &[Arc<Proc>]) {
        let mut problems = Vec::new();
        {
            let state = lock(&self.shared.state);
            if !state.unstated.is_empty() {
                problems.push(format!("calls with no stated reply: {:?}", state.unstated));
            }
            if !state.spawns.is_empty() {
                problems.push(format!("{} stated spawn replies never used", state.spawns.len()));
            }
            if !state.threads.is_empty() {
                problems.push(format!("{} stated thread replies never used", state.threads.len()));
            }
            if !state.append_failures.is_empty() {
                problems.push(format!(
                    "stated append failures never used: {:?}",
                    state.append_failures
                ));
            }
            let unread: Vec<&ChannelCall> = state
                .channel_calls
                .iter()
                .filter(|call| matches!(call, ChannelCall::EventLog { .. }))
                .collect();
            if !unread.is_empty() {
                problems.push(format!("Event Log lines never read: {unread:?}"));
            }
            let unread_ends: Vec<usize> = (0..state.thread_ends.len())
                .filter(|&n| !matches!(state.thread_ends[n], ThreadEnd::Read))
                .collect();
            if !unread_ends.is_empty() {
                problems.push(format!("waiter threads whose end was never read: {unread_ends:?}"));
            }
        }
        for (n, process) in processes.iter().enumerate() {
            let state = lock(&process.state);
            for (what, left) in [
                ("wait", state.waits.len()),
                ("kill", state.kills.len()),
                ("kill_tree", state.tree_kills.len()),
            ] {
                if left > 0 {
                    problems.push(format!("process {n}: {left} stated {what} replies never used"));
                }
            }
        }
        assert!(problems.is_empty(), "the Fake was not spent: {problems:#?}");
    }
}

impl Os for Fake {
    fn spawn(&self, _cmd: cosca::Command) -> Result<Box<dyn Spawned>, cosca::error::Error> {
        let reply = {
            let mut state = lock(&self.shared.state);
            *state.calls.entry(Op::Spawn).or_default() += 1;
            let reply = state.spawns.pop_front();
            if reply.is_none() {
                state.unstated.push("spawn".to_string());
            }
            reply
        };
        match reply {
            None => panic!("unstated call: spawn"),
            Some(SpawnReply::Refuses(error)) => Err(error),
            Some(SpawnReply::Process(FakeProcess(proc))) => {
                lock(&self.shared.state).processes.push(Arc::clone(&proc));
                Ok(Box::new(Instance {
                    proc,
                    fake: Arc::clone(&self.shared),
                    detached: false,
                }))
            }
        }
    }

    fn thread(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>> {
        let index = {
            let mut state = lock(&self.shared.state);
            *state.calls.entry(Op::Thread).or_default() += 1;
            match state.threads.pop_front() {
                None => {
                    state.unstated.push("thread".to_string());
                    None
                }
                Some(false) => return Err(io::Error::other(NO_THREAD)),
                Some(true) => {
                    state.thread_ends.push(ThreadEnd::Running);
                    Some(state.thread_ends.len() - 1)
                }
            }
        };
        let Some(index) = index else {
            panic!("unstated call: thread")
        };
        let shared = Arc::clone(&self.shared);
        std::thread::Builder::new().name(name.to_string()).spawn(move || {
            let _end = ThreadExit { shared, index };
            f();
        })
    }
}

/// Held for the whole life of a made thread's closure, so its drop runs after every call the
/// closure can make: it records how the thread ended and wakes [`Fake::waiter_end`].
struct ThreadExit {
    shared: Arc<Shared>,
    index: usize,
}

impl Drop for ThreadExit {
    fn drop(&mut self) {
        let end = if std::thread::panicking() {
            WaiterEnd::Panicked
        } else {
            WaiterEnd::Returned
        };
        lock(&self.shared.state).thread_ends[self.index] = ThreadEnd::Ended(end);
        self.shared.thread_ended.notify_all();
    }
}

impl Channels for Fake {
    fn append(&self, path: &Path, line: &str) -> io::Result<()> {
        let mut state = lock(&self.shared.state);
        *state.calls.entry(Op::Append).or_default() += 1;
        let failing = match state.append_failures.iter().position(|failing| failing == path) {
            Some(i) => {
                state.append_failures.remove(i);
                true
            }
            None => false,
        };
        state.channel_calls.push(ChannelCall::Append {
            path: path.to_path_buf(),
            line: line.to_string(),
            ok: !failing,
        });
        if failing {
            Err(io::Error::other(APPEND_FAILED))
        } else {
            Ok(())
        }
    }

    fn event_log(&self, severity: Severity, line: &str) {
        let mut state = lock(&self.shared.state);
        *state.calls.entry(Op::EventLog).or_default() += 1;
        state.channel_calls.push(ChannelCall::EventLog {
            severity,
            line: line.to_string(),
        });
    }
}
