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
use std::time::Duration;

use cosca::Containment;
use cosca::containment::TreeDrain;
use cosca::error::ChildFate;

use super::{Channels, LogKind, Os, Severity, Spawned};
use crate::backend::scm::shim::run::logging::Log;

/// The message of the usual refused thread: `io::Error::other(NO_THREAD)`.
pub(crate) const NO_THREAD: &str = "no thread may be made (fake)";
const KILL_REFUSED: &str = "the kill was refused (fake)";
const WAIT_FAILED: &str = "the wait failed (fake)";
const TRY_WAIT_FAILED: &str = "the read of the exit failed (fake)";
const WAIT_TREE_FAILED: &str = "the wait for the tree failed (fake)";
/// The messages the answers of an ask carry, each one's own, so a log line shows which was read.
const ASK_IO: &str = "the proxy could not be started (fake)";
const ASK_MAY_HAVE_STARTED: &str = "the proxy may have run (fake)";
const ASK_UNASSESSABLE: &str = "whether the event was sent is unknown (fake)";
const ASK_UNSUPPORTED: &str = "the daemon has no console yet (fake)";
/// The error a failing append fails with.
pub(crate) const APPEND_FAILED: &str = "the append failed (fake)";
/// The error a failing [`Os::open_log`] fails with.
pub(crate) const OPEN_LOG_FAILED: &str = "the log could not be opened (fake)";

pub(crate) const HOLD_FAILED: &str = "the log could not be held (fake)";

/// A refused [`Spawned::kill`].
pub(crate) fn kill_refused() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(KILL_REFUSED))
}

/// A refused [`Spawned::send_kill_tree`]: a `Containment` over [`kill_refused`], so a report that
/// prints only the top layer fails the tests.
pub(crate) fn tree_kill_refused() -> cosca::error::Error {
    cosca::error::Error::Containment {
        detail: "TerminateJobObject could not terminate the job".to_owned(),
        source: Some(Arc::new(kill_refused())),
    }
}

/// A failed [`Spawned::try_wait`].
pub(crate) fn try_wait_failed() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(TRY_WAIT_FAILED))
}

/// A failed [`Spawned::wait_tree_timeout`].
pub(crate) fn wait_tree_failed() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(WAIT_TREE_FAILED))
}

/// The errors an ask answers, as [`AskReply`] makes them.
pub(crate) fn ask_io() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(ASK_IO))
}

pub(crate) fn ask_unsupported() -> cosca::error::Error {
    cosca::error::Error::Unsupported {
        op: "terminate_tree".to_owned(),
        platform: "windows",
        detail: ASK_UNSUPPORTED.to_owned(),
        source: None,
    }
}

pub(crate) fn ask_may_have_started() -> cosca::error::Error {
    cosca::error::Error::MayHaveStarted {
        fate: ChildFate::Gone,
        wrapper_elevated: false,
        detail: None,
        source: Arc::new(cosca::error::Error::Io(io::Error::other(ASK_MAY_HAVE_STARTED))),
        unreaped: None,
        cleanup: None,
    }
}

pub(crate) fn ask_unassessable() -> cosca::error::Error {
    cosca::error::Error::Unassessable {
        detail: ASK_UNASSESSABLE.to_owned(),
        source: None,
    }
}

pub(crate) fn wait_failed() -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::other(WAIT_FAILED))
}

pub(crate) fn wait_failed_with_code(code: i32) -> cosca::error::Error {
    cosca::error::Error::Io(io::Error::from_raw_os_error(code))
}

/// The default log directory of [`Fake::log`], absolute as a spec's `logs:` must be. The `Fake` writes no
/// file there.
pub(crate) const FAKE_LOGS: &str = r"C:\goetia-shim-test\fake-logs";

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
    /// A [`Spawned::terminate_tree`], the ask.
    Ask,
    WaitTree,
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

/// What a [`Spawned::terminate_tree`] answers, by the contract the supervisor reads (`stop_bus`'s
/// `judge_ask`). A variant names the `cosca::error::Error` it answers with (none: `Ok`), and what else the `Fake`
/// does when it answers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum AskReply {
    Sent,
    /// The daemon exits with this code.
    SentAndExits(i32),
    Io,
    Unsupported,
    /// The daemon exits meanwhile with this code.
    UnsupportedAndExits(i32),
    /// The daemon has exited with this code while the waiter's report of it lags: only a read of the process itself
    /// ([`Spawned::try_wait`]) shows it.
    UnsupportedAndExitedUnreported(i32),
    MayHaveStarted,
    Unassessable,
    /// The wait on the process fails; the daemon is not ended.
    UnsupportedAndWaitFails,
    /// The daemon has exited with this code.
    UnassessableAndExited(i32),
}

/// What a [`Spawned::wait_tree_timeout`] answers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TreeWaitReply {
    /// `AllMembersExited`: the job is empty, so the process has exited: it ends with status 0 unless it
    /// ended before.
    Drained,
    /// `MembersRemain`: the bound passed.
    Remain,
    /// `AllMarkersClosed`: advisory evidence only, which does not end the process.
    MarkersClosed,
    /// Fail with [`wait_tree_failed`].
    Fails,
}

/// A call on a process the supervisor made, in order, with what it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProcessCall {
    Ask,
    WaitTree(Duration),
    SendKillTree,
    Kill,
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
    Append {
        path: PathBuf,
        line: String,
        kind: LogKind,
        ok: bool,
    },
    EventLog {
        severity: Severity,
        line: String,
    },
    HoldDefault {
        path: PathBuf,
    },
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
    asks: VecDeque<AskReply>,
    /// What an ask answers once `asks` is spent.
    asking_always: Option<AskReply>,
    /// Whether [`Spawned::try_wait`] fails.
    try_wait_fails: bool,
    /// Whether a blocked [`Spawned::wait`] is made to fail.
    wait_fails: bool,
    /// Whether the waiter's report of the exit is held until [`Spawned::try_wait`] reads it.
    report_held: bool,
    tree_waits: VecDeque<TreeWaitReply>,
    calls: Vec<ProcessCall>,
    exit: Option<ExitStatus>,
    killed: bool,
    dropped: bool,
    /// What happens in the world as the `Fake` spawns the process ([`FakeProcess::when_spawned`]).
    when_spawned: Option<Box<dyn FnOnce() + Send>>,
    /// What happens in the world as the first failed read of the exit is made ([`FakeProcess::when_the_exit_read_fails`]).
    when_the_exit_read_fails: Option<Box<dyn FnOnce() + Send>>,
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

    /// Let go of a report that is held, so that the waiter can return.
    fn release_report(&self) {
        lock(&self.state).report_held = false;
        self.ended.notify_all();
    }

    /// Block until the process is ended and its report is not held, or the wait is made to fail
    /// ([`AskReply::UnsupportedAndWaitFails`]); its status.
    fn wait_ended(&self) -> Result<ExitStatus, cosca::error::Error> {
        let state = self
            .ended
            .wait_while(lock(&self.state), |state| {
                (state.exit.is_none() || state.report_held) && !state.wait_fails
            })
            .unwrap_or_else(PoisonError::into_inner);
        match state.exit {
            Some(status) if !state.wait_fails => Ok(status),
            _ => Err(wait_failed()),
        }
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
                asks: VecDeque::new(),
                asking_always: None,
                try_wait_fails: false,
                wait_fails: false,
                report_held: false,
                tree_waits: VecDeque::new(),
                calls: Vec::new(),
                exit: None,
                killed: false,
                dropped: false,
                when_spawned: None,
                when_the_exit_read_fails: None,
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

    /// The answer to the next ask not yet stated for.
    pub(crate) fn ask(self, reply: AskReply) -> FakeProcess {
        self.with(|state| state.asks.push_back(reply))
    }

    /// The answer to every ask after the stated ones: for a test that cannot know how many asks the
    /// supervisor makes before something else happens.
    pub(crate) fn asking_always(self, reply: AskReply) -> FakeProcess {
        self.with(|state| state.asking_always = Some(reply))
    }

    /// A process that has exited with `code`, whose report by the waiter lags: it is held until the supervisor reads
    /// the process itself ([`Spawned::try_wait`]), as the real report can lag the exit.
    pub(crate) fn exited_unreported(self, code: i32) -> FakeProcess {
        self.with(|state| {
            state.report_held = true;
            state.exit = Some(status(code));
        })
    }

    /// Every read of whether the process has exited fails.
    pub(crate) fn try_wait_fails(self) -> FakeProcess {
        self.with(|state| state.try_wait_fails = true)
    }

    /// `effect` happens as the first failed read of the exit is made (with [`FakeProcess::try_wait_fails`]), with no lock
    /// held: what the world has done by the time the read is tried, such as the waiter's report of the exit. At most one.
    pub(crate) fn when_the_exit_read_fails(self, effect: impl FnOnce() + Send + 'static) -> FakeProcess {
        self.with(|state| {
            assert!(
                state.when_the_exit_read_fails.replace(Box::new(effect)).is_none(),
                "`when_the_exit_read_fails` was stated twice"
            );
        })
    }

    /// The reply to the next `wait_tree_timeout` not yet stated for.
    pub(crate) fn wait_tree(self, reply: TreeWaitReply) -> FakeProcess {
        self.with(|state| state.tree_waits.push_back(reply))
    }

    /// `effect` happens as the `Fake` spawns the process, before the supervisor holds it and with no lock held: a stop
    /// that is accepted after the last look at the stop and before the program exists. At most one.
    pub(crate) fn when_spawned(self, effect: impl FnOnce() + Send + 'static) -> FakeProcess {
        self.with(|state| {
            assert!(
                state.when_spawned.replace(Box::new(effect)).is_none(),
                "`when_spawned` was stated twice"
            );
        })
    }

    /// Every ask, tree wait and kill made on the process, in order.
    pub(crate) fn calls(&self) -> Vec<ProcessCall> {
        lock(&self.0.state).calls.clone()
    }

    /// End the process by hand, as it would end on its own: the part of a test that does what the
    /// supervisor deliberately did not.
    pub(crate) fn end(&self, code: i32) {
        self.0.end(status(code));
    }

    pub(crate) fn ended(&self) -> bool {
        lock(&self.0.state).exit.is_some()
    }

    /// The status it ended with, if it has ended.
    pub(crate) fn exit_code(&self) -> Option<i32> {
        lock(&self.0.state).exit.and_then(|status| status.code())
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

    fn record(&self, call: ProcessCall) {
        lock(&self.proc.state).calls.push(call);
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
            Some(WaitReply::Blocks) => self.proc.wait_ended(),
            Some(WaitReply::Fails(Some(code))) => Err(wait_failed_with_code(code)),
            Some(WaitReply::Fails(None)) => Err(wait_failed()),
            Some(WaitReply::Panics) => panic!("the wait panicked (fake)"),
        }
    }

    fn try_wait(&self) -> Result<Option<ExitStatus>, cosca::error::Error> {
        let mut state = lock(&self.proc.state);
        if state.try_wait_fails {
            let effect = state.when_the_exit_read_fails.take();
            drop(state);
            if let Some(effect) = effect {
                effect();
            }
            return Err(try_wait_failed());
        }
        let exit = state.exit;
        if exit.is_some() && state.report_held {
            state.report_held = false;
            self.proc.ended.notify_all();
        }
        Ok(exit)
    }

    fn kill(&self) -> Result<(), cosca::error::Error> {
        self.record(ProcessCall::Kill);
        let reply = lock(&self.proc.state).kills.pop_front();
        self.kill_with("kill", kill_refused, reply)
    }

    fn send_kill_tree(&self) -> Result<(), cosca::error::Error> {
        self.fake.count(Op::SendKillTree);
        self.record(ProcessCall::SendKillTree);
        let reply = lock(&self.proc.state).send_kill_tree_replies.pop_front();
        self.kill_with("send_kill_tree", tree_kill_refused, reply)
    }

    fn terminate_tree(&self) -> Result<(), cosca::error::Error> {
        self.fake.count(Op::Ask);
        self.record(ProcessCall::Ask);
        // The proxy takes long enough for the waiter's report to have come in.
        self.proc.release_report();
        let reply = {
            let mut state = lock(&self.proc.state);
            state.asks.pop_front().or(state.asking_always)
        };
        let Some(reply) = reply else {
            self.unstated("terminate_tree")
        };
        match reply {
            AskReply::Sent => Ok(()),
            AskReply::SentAndExits(code) => {
                self.proc.end(status(code));
                Ok(())
            }
            AskReply::Io => Err(ask_io()),
            AskReply::Unsupported => Err(ask_unsupported()),
            AskReply::UnsupportedAndExits(code) => {
                self.proc.end(status(code));
                Err(ask_unsupported())
            }
            AskReply::UnsupportedAndExitedUnreported(code) => {
                let mut state = lock(&self.proc.state);
                state.report_held = true;
                state.exit = Some(status(code));
                Err(ask_unsupported())
            }
            AskReply::UnsupportedAndWaitFails => {
                lock(&self.proc.state).wait_fails = true;
                self.proc.ended.notify_all();
                Err(ask_unsupported())
            }
            AskReply::MayHaveStarted => Err(ask_may_have_started()),
            AskReply::Unassessable => Err(ask_unassessable()),
            AskReply::UnassessableAndExited(code) => {
                self.proc.end(status(code));
                Err(ask_unassessable())
            }
        }
    }

    fn wait_tree_timeout(&self, timeout: Duration) -> Result<TreeDrain, cosca::error::Error> {
        self.fake.count(Op::WaitTree);
        self.record(ProcessCall::WaitTree(timeout));
        let reply = lock(&self.proc.state).tree_waits.pop_front();
        match reply {
            None => self.unstated("wait_tree_timeout"),
            Some(TreeWaitReply::Drained) => {
                self.proc.end(status(0));
                Ok(TreeDrain::AllMembersExited)
            }
            Some(TreeWaitReply::Remain) => Ok(TreeDrain::MembersRemain),
            Some(TreeWaitReply::MarkersClosed) => Ok(TreeDrain::AllMarkersClosed),
            Some(TreeWaitReply::Fails) => Err(wait_tree_failed()),
        }
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
    /// Every log opened, in order: the path asked for, which kind of log it was, and a
    /// handle to what was opened, which is what the code under test writes into.
    opened_logs: Vec<(PathBuf, LogKind, File)>,
    append_failures: Vec<PathBuf>,
    hold_failures: Vec<PathBuf>,
    /// The default logs held, by path: by a successful `hold_default`, or by an `append` that opened one,
    /// as the real channels hold what they open.
    held_defaults: Vec<PathBuf>,
    /// The `append`s to a default log that was not held, which the real channels open and hold.
    default_log_opens: usize,
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
            process.release_report();
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

    /// A task-mode `Log` over this `Fake`'s channels: closed to the default log until it is held.
    pub(crate) fn task_log(&self) -> Arc<Log> {
        Arc::new(Log::new(self.channels(), PathBuf::from(FAKE_LOGS)).closed_until_held())
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

    /// The next `hold_default` of `path` fails with [`HOLD_FAILED`].
    pub(crate) fn will_fail_hold(&self, path: &Path) {
        lock(&self.shared.state).hold_failures.push(path.to_path_buf());
    }

    // Records ---------------------------------------------------------------------------------------------------------

    /// How many times `op` was called, replied to or not. Readable from inside a callback of the
    /// code under test.
    pub(crate) fn calls(&self, op: Op) -> usize {
        lock(&self.shared.state).calls.get(&op).copied().unwrap_or(0)
    }

    /// The path the `n`th log opened (counting from 0) was asked for.
    pub(crate) fn opened_log_path(&self, n: usize) -> PathBuf {
        lock(&self.shared.state).opened_logs[n].0.clone()
    }

    pub(crate) fn opened_log_kind(&self, n: usize) -> LogKind {
        lock(&self.shared.state).opened_logs[n].1
    }

    /// What has been written into the `n`th log opened, whole.
    pub(crate) fn opened_log_text(&self, n: usize) -> String {
        use std::io::{Read as _, Seek as _, SeekFrom};

        let mut file = lock(&self.shared.state).opened_logs[n]
            .2
            .try_clone()
            .expect("clone the log");
        file.seek(SeekFrom::Start(0)).expect("rewind the log");
        let mut text = String::new();
        file.read_to_string(&mut text).expect("read the log");
        text
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

    /// The information lines reported to the Event Log, oldest first, removed.
    pub(crate) fn take_infos(&self) -> Vec<String> {
        self.take_event_log(Severity::Information)
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

    /// How many `append`s to a default log found it not held: the opens the real channels would have made.
    pub(crate) fn default_log_opens(&self) -> usize {
        lock(&self.shared.state).default_log_opens
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
            if !state.hold_failures.is_empty() {
                problems.push(format!("stated hold failures never used: {:?}", state.hold_failures));
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
                ("ask", state.asks.len()),
                ("wait_tree_timeout", state.tree_waits.len()),
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
                let effect = lock(&proc.state).when_spawned.take();
                if let Some(effect) = effect {
                    effect();
                }
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

    fn open_log(&self, path: &Path, kind: LogKind) -> io::Result<File> {
        let mut state = lock(&self.shared.state);
        *state.calls.entry(Op::OpenLog).or_default() += 1;
        match state.open_log_failures.iter().position(|failing| failing == path) {
            Some(i) => {
                state.open_log_failures.remove(i);
                Err(io::Error::other(OPEN_LOG_FAILED))
            }
            None => {
                drop(state);
                let file = tempfile::tempfile()?;
                lock(&self.shared.state)
                    .opened_logs
                    .push((path.to_path_buf(), kind, file.try_clone()?));
                Ok(file)
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
    fn append(&self, path: &Path, line: &str, kind: LogKind) -> io::Result<()> {
        let mut state = lock(&self.shared.state);
        let failing = match state.append_failures.iter().position(|failing| failing == path) {
            Some(i) => {
                state.append_failures.remove(i);
                true
            }
            None => false,
        };
        // As the real channels: each append that finds the default log not held tries to open it, and only an
        // open that succeeds holds it.
        if kind == LogKind::Default && !state.held_defaults.iter().any(|held| held == path) {
            state.default_log_opens += 1;
            if !failing {
                state.held_defaults.push(path.to_path_buf());
            }
        }
        state.channel_calls.push(ChannelCall::Append {
            path: path.to_path_buf(),
            line: line.to_string(),
            kind,
            ok: !failing,
        });
        if failing {
            Err(io::Error::other(APPEND_FAILED))
        } else {
            Ok(())
        }
    }

    fn hold_default(&self, path: &Path) -> io::Result<()> {
        let mut state = lock(&self.shared.state);
        state.channel_calls.push(ChannelCall::HoldDefault {
            path: path.to_path_buf(),
        });
        match state.hold_failures.iter().position(|failing| failing == path) {
            Some(i) => {
                state.hold_failures.remove(i);
                Err(io::Error::other(HOLD_FAILED))
            }
            None => {
                if !state.held_defaults.iter().any(|held| held == path) {
                    state.held_defaults.push(path.to_path_buf());
                }
                Ok(())
            }
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
