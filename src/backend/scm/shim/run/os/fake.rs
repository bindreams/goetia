//! A test OS: tables of stated replies, with no process behind them and no named file: `open_log`
//! returns an anonymous [`tempfile::tempfile`], which is real because a child needs a real handle
//! to redirect its output into. It models no operating system; the rules it encodes are that a
//! successful kill ends the process it killed, and that a process keeps the status of its first
//! ending.
//!
//! A test states the replies it expects, in the order each operation will be called. A call with no
//! stated reply is recorded and panics; the [`FakeGuard`] then fails the test at its end for that
//! call, for every reply never consumed, for every failure or warning line never read and for every
//! waiter thread whose end was never read, so a panic swallowed on a thread the supervisor made
//! still fails the test.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io;
use std::ops::Deref;
use std::os::windows::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use cosca::Containment;

use super::{Channels, Os, Severity, Spawned};
use crate::backend::scm::shim::run::logging::Log;

/// The message of the usual refused thread: `io::Error::other(NO_THREAD)`.
pub(crate) const NO_THREAD: &str = "no thread may be made (fake)";
const KILL_REFUSED: &str = "the kill was refused (fake)";
const WAIT_FAILED: &str = "the wait failed (fake)";
/// The error a failing append fails with.
pub(crate) const APPEND_FAILED: &str = "the append failed (fake)";
/// The error a failing [`Os::open_log`] fails with.
pub(crate) const OPEN_LOG_FAILED: &str = "the log could not be opened (fake)";
/// A refused [`Spawned::kill`].
pub(crate) fn kill_refused() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(KILL_REFUSED))
}

/// A refused [`Spawned::send_kill_tree`]: a `Containment` over [`kill_refused`], so a report that
/// prints only the top layer fails the tests. Like every fake refusal it carries no OS code.
pub(crate) fn tree_kill_refused() -> cosca::error::Error {
    cosca::error::Error::Containment {
        detail: "TerminateJobObject could not terminate the job".to_owned(),
        source: Some(Arc::new(kill_refused())),
    }
}

pub(crate) fn wait_failed() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(WAIT_FAILED))
}

pub(crate) fn wait_failed_with_code(code: i32) -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::from_raw_os_error(code))
}

/// The default log directory of [`Fake::log`]. The `Fake` writes no file there.
const FAKE_LOGS: &str = "fake-logs";

/// Poison is recovered from: a panicking thread must not make every later call on the `Fake` panic.
/// Nothing here panics with a lock held.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An operation the `Fake` counts, whether or not it was given a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Op {
    Spawn,
    SendKillTree,
    OpenLog,
}

/// What a [`Spawned::wait`] does.
pub(crate) enum WaitReply {
    /// Return at once: the process has exited with this code, or with the status it ended with
    /// before.
    Exits(i32),
    /// Block until the process is ended, by a successful kill or [`FakeProcess::end`], and return
    /// its status.
    Blocks,
    /// Fail, with this OS error code ([`wait_failed_with_code`]) or, with none, [`wait_failed`].
    Fails(Option<i32>),
    /// Panic, ending the waiter thread without it reporting the child done.
    Panics,
}

/// What a [`Spawned::kill`] or [`Spawned::send_kill_tree`] does.
pub(crate) enum KillReply {
    /// Succeed. The process is ended.
    Ends,
    /// Fail with [`kill_refused`] for a kill, [`tree_kill_refused`] for a tree kill. The process is
    /// not ended.
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
    send_kill_tree_replies: VecDeque<KillReply>,
    exit: Option<ExitStatus>,
    killed: bool,
    dropped: bool,
}

fn status(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code as u32)
}

impl Proc {
    /// End the process with `status`, unless it ended already: the first ending wins. The status it
    /// ended with.
    fn end(&self, status: ExitStatus) -> ExitStatus {
        let first = *lock(&self.state).exit.get_or_insert(status);
        self.ended.notify_all();
        first
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
                send_kill_tree_replies: VecDeque::new(),
                exit: None,
                killed: false,
                dropped: false,
            }),
            ended: Condvar::new(),
        }))
    }

    /// A daemon that runs until a tree kill ends it: the common case of a stop.
    pub(crate) fn running() -> FakeProcess {
        FakeProcess::new()
            .wait(WaitReply::Blocks)
            .send_kill_tree(KillReply::Ends)
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

    pub(crate) fn send_kill_tree(self, reply: KillReply) -> FakeProcess {
        self.with(|state| state.send_kill_tree_replies.push_back(reply))
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

    /// Whether the supervisor's instance of it was dropped.
    pub(crate) fn dropped(&self) -> bool {
        lock(&self.0.state).dropped
    }
}

/// The process as the supervisor holds it.
struct Instance {
    proc: Arc<Proc>,
    fake: Arc<Shared>,
}

impl Instance {
    /// A call with no stated reply: recorded for the `FakeGuard`, then a panic.
    fn unstated(&self, what: &str) -> ! {
        lock(&self.fake.state).unstated.push(format!("{what} on a process"));
        panic!("unstated call: {what} on a process");
    }

    fn kill_with(
        &self,
        what: &str,
        refused: fn() -> cosca::error::Error,
        reply: Option<KillReply>,
    ) -> Result<(), cosca::error::Error> {
        match reply {
            None => self.unstated(what),
            Some(KillReply::Ends) => {
                lock(&self.proc.state).killed = true;
                self.proc.end(status(1));
                Ok(())
            }
            Some(KillReply::Refuses) => Err(refused()),
            Some(KillReply::Panics) => panic!("the {what} panicked (fake)"),
        }
    }
}

impl Spawned for Instance {
    fn containment(&self) -> Containment {
        lock(&self.proc.state).containment
    }

    fn wait(&self) -> Result<ExitStatus, cosca::error::Error> {
        let reply = lock(&self.proc.state).waits.pop_front();
        match reply {
            None => self.unstated("wait"),
            Some(WaitReply::Exits(code)) => Ok(self.proc.end(status(code))),
            Some(WaitReply::Blocks) => Ok(self.proc.wait_ended()),
            Some(WaitReply::Fails(Some(code))) => Err(wait_failed_with_code(code)),
            Some(WaitReply::Fails(None)) => Err(wait_failed()),
            Some(WaitReply::Panics) => panic!("the wait panicked (fake)"),
        }
    }

    fn kill(&self) -> Result<(), cosca::error::Error> {
        let reply = lock(&self.proc.state).kills.pop_front();
        self.kill_with("kill", kill_refused, reply)
    }

    fn send_kill_tree(&self) -> Result<(), cosca::error::Error> {
        self.fake.count(Op::SendKillTree);
        let reply = lock(&self.proc.state).send_kill_tree_replies.pop_front();
        self.kill_with("send_kill_tree", tree_kill_refused, reply)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        lock(&self.proc.state).dropped = true;
    }
}

// Fake ----------------------------------------------------------------------------------------------------------------

enum SpawnReply {
    Process(FakeProcess),
    Refuses(cosca::error::Error),
}

enum ThreadReply {
    Makes,
    Refuses(io::Error),
}

enum ThreadEnd {
    Running,
    Ended(WaiterEnd),
    Read,
    /// The OS refused the thread after the `Fake` allowed it: it has no end to read.
    NeverSpawned,
}

#[derive(Default)]
struct State {
    spawns: VecDeque<SpawnReply>,
    threads: VecDeque<ThreadReply>,
    open_log_failures: Vec<PathBuf>,
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
pub(crate) struct FakeGuard {
    fake: Fake,
}

impl FakeGuard {
    pub(crate) fn new() -> FakeGuard {
        FakeGuard {
            fake: Fake {
                shared: Arc::new(Shared {
                    state: Mutex::new(State::default()),
                    thread_ended: Condvar::new(),
                }),
            },
        }
    }
}

impl Deref for FakeGuard {
    type Target = Fake;

    fn deref(&self) -> &Fake {
        &self.fake
    }
}

impl Drop for FakeGuard {
    fn drop(&mut self) {
        let processes = lock(&self.fake.shared.state).processes.clone();
        for process in &processes {
            process.end(status(1));
        }
        if !std::thread::panicking() {
            self.fake.assert_spent(&processes);
        }
    }
}

impl Fake {
    pub(crate) fn os(&self) -> Arc<dyn Os> {
        Arc::new(self.clone())
    }

    pub(crate) fn channels(&self) -> Arc<dyn Channels> {
        Arc::new(self.clone())
    }

    /// A `Log` over this `Fake`'s channels.
    pub(crate) fn log(&self) -> Arc<Log> {
        Arc::new(Log::new(self.channels(), PathBuf::from(FAKE_LOGS)))
    }

    // Tables ----------------------------------------------------------------------------------------------------------

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
        lock(&self.shared.state).threads.push_back(ThreadReply::Makes);
    }

    /// The next `thread` fails with `error`, [`NO_THREAD`] being the usual one. A refused thread has
    /// no end to read.
    pub(crate) fn will_refuse_thread(&self, error: io::Error) {
        lock(&self.shared.state).threads.push_back(ThreadReply::Refuses(error));
    }

    /// The next `open_log` of `path` fails with [`OPEN_LOG_FAILED`]. Any other path opens a
    /// temporary file that is deleted when closed.
    pub(crate) fn will_fail_open_log(&self, path: &Path) {
        lock(&self.shared.state).open_log_failures.push(path.to_path_buf());
    }

    /// The next `append` to `path` fails with [`APPEND_FAILED`].
    pub(crate) fn will_fail_append(&self, path: &Path) {
        lock(&self.shared.state).append_failures.push(path.to_path_buf());
    }

    // Records ---------------------------------------------------------------------------------------------------------

    /// How many times `op` was called, replied to or not. Readable from inside a callback of the
    /// code under test.
    pub(crate) fn calls(&self, op: Op) -> usize {
        lock(&self.shared.state).calls.get(&op).copied().unwrap_or(0)
    }

    /// Whether thread `n` has ended, without waiting for it. Its end is still read with
    /// [`Fake::waiter_end`].
    pub(crate) fn waiter_has_ended(&self, n: usize) -> bool {
        matches!(
            lock(&self.shared.state).thread_ends.get(n),
            Some(ThreadEnd::Ended(_) | ThreadEnd::Read)
        )
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
            Some(ThreadEnd::NeverSpawned) => {
                drop(state);
                panic!("thread {n} was never spawned: the OS refused it");
            }
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
            if !state.open_log_failures.is_empty() {
                problems.push(format!(
                    "stated open_log failures never used: {:?}",
                    state.open_log_failures
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
                .filter(|&n| !matches!(state.thread_ends[n], ThreadEnd::Read | ThreadEnd::NeverSpawned))
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
                ("send_kill_tree", state.send_kill_tree_replies.len()),
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
                }))
            }
        }
    }

    fn thread(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>> {
        let index = {
            let mut state = lock(&self.shared.state);
            match state.threads.pop_front() {
                None => {
                    state.unstated.push("thread".to_string());
                    None
                }
                Some(ThreadReply::Refuses(error)) => return Err(error),
                Some(ThreadReply::Makes) => {
                    state.thread_ends.push(ThreadEnd::Running);
                    Some(state.thread_ends.len() - 1)
                }
            }
        };
        let Some(index) = index else {
            panic!("unstated call: thread")
        };
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || {
            let _end = ThreadExit { shared, index };
            f();
        });
        if spawned.is_err() {
            lock(&self.shared.state).thread_ends[index] = ThreadEnd::NeverSpawned;
            self.shared.thread_ended.notify_all();
        }
        spawned
    }

    fn open_log(&self, path: &Path) -> io::Result<File> {
        let mut state = lock(&self.shared.state);
        *state.calls.entry(Op::OpenLog).or_default() += 1;
        match state.open_log_failures.iter().position(|failing| failing == path) {
            Some(i) => {
                state.open_log_failures.remove(i);
                Err(io::Error::other(OPEN_LOG_FAILED))
            }
            None => {
                drop(state);
                tempfile::tempfile()
            }
        }
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
        state.channel_calls.push(ChannelCall::EventLog {
            severity,
            line: line.to_string(),
        });
    }
}
