//! A task's `run` over a scripted `launchctl` and real child processes: each child stands for the
//! process a launch is (the pid `kickstart -p` prints), and the kernel's account of it, which `run` reads,
//! is the child's own. A launch our `kickstart` made is spawned by the call that names it, so that it
//! started after the `debug` before it; an older launch is spawned before `begin_run`. A held launch is a
//! stopped child, and the scripted `SIGCONT` lets it go, so the watch `run` follows is a real kqueue's.
//! Nothing here touches the system's own directories or services.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::super::interrupts::interrupts_tests::{StandIn, ignore, seen, send_to_process, serial};
use super::super::manager_tests::launchd_over;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, task_spec};
use super::super::watch::test_hook;
use super::super::watch::watch_tests::{Held, pending};
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded::{self, Role};
use crate::backend::client::ClientError;
use crate::backend::client::fake::{Entry, Fake, exited, fail, reply, reply_at};
#[cfg(feature = "cli")]
use crate::cli::ExitCode;
use crate::spec::{DaemonSpec, TaskFields, TaskSpec};
use crate::testing::base_fields;

// Fixtures ============================================================================================================

/// A `print` of a loaded task's job with no process.
const IDLE: &str = "\tstate = not running\n";

/// The note a run that waited for a scheduled one writes.
const NOTE: &str = "note: a scheduled run of `goetia-task-unit` is in progress; waiting for it to end before \
                    starting this run\n";

fn target() -> String {
    format!("system/{ID}")
}

fn print(body: &str) -> Entry {
    reply(&["print", &target()], exited(0, body, ""))
}

fn not_loaded() -> Entry {
    reply(&["print", &target()], exited(113, "", ""))
}

fn debug() -> Entry {
    debug_exiting(0)
}

/// `debug`, which exits 37 for a hold that is already pending.
fn debug_exiting(code: i32) -> Entry {
    reply(
        &["debug", &target(), "--start-suspended"],
        exited(code, "", "Configuration failed: 37: Operation already in progress\n"),
    )
}

/// `kickstart -p`, which names the pid of a launch that is there already: an older one, or one that has
/// ended.
fn kickstart(pid: u32) -> Entry {
    reply(&["kickstart", "-p", &target()], exited(0, &format!("{pid}\n"), ""))
}

fn sigcont() -> Entry {
    reply(&["kill", "SIGCONT", &target()], exited(0, "", ""))
}

/// What `launchctl procinfo <pid>` prints of a launch of the job: the process, then launchd's block for the
/// job, which has the pid.
fn block_of(pid: u32) -> String {
    format!(
        "program path = /usr/libexec/xpcproxy\n\
         \n\
         {target} = {{\n\
         \tstate = running\n\
         \tpid = {pid}\n\
         }}\n",
        target = target()
    )
}

/// `procinfo` of `pid`, which it names as the job's launch.
fn procinfo(pid: u32) -> Entry {
    reply(&["procinfo", &pid.to_string()], exited(0, &block_of(pid), ""))
}

/// A child process a script's hook can reach: it is made, and lets go, inside the calls of the table.
#[derive(Clone, Default)]
struct Launched(Arc<Mutex<Option<Held>>>);

impl Launched {
    fn of(held: Held) -> Launched {
        Launched(Arc::new(Mutex::new(Some(held))))
    }

    fn pid(&self) -> u32 {
        self.with(|held| held.pid())
    }

    /// Lets the process go, as a `SIGCONT` does a stopped one: it runs to its end, and is reaped.
    fn finish(&self) {
        self.with(|held| {
            held.finish();
        });
    }

    /// Closes the process's stdin and goes on.
    fn release(&self) {
        self.with(Held::release);
    }

    /// Answers the process's first `read`, and leaves it blocked on its next.
    fn advance(&self) {
        self.with(Held::advance);
    }

    fn with<T>(&self, then: impl FnOnce(&mut Held) -> T) -> T {
        then(self.0.lock().unwrap().as_mut().expect("the process was made"))
    }
}

/// `kickstart -p` launching the process `make` makes, and naming its pid: made at the call, after the
/// `debug` before it.
fn kickstart_launching(make: impl FnOnce() -> Held + Send + 'static) -> (Entry, Launched) {
    let launched = Launched::default();
    let (made, named) = (launched.clone(), launched.clone());
    let entry = reply_at(&["kickstart", "-p", &target()], move || {
        exited(0, &format!("{}\n", named.pid()), "")
    })
    .then(move || *made.0.lock().unwrap() = Some(make()));
    (entry, launched)
}

/// `kickstart -p` launching our own launch: stopped, as the one `debug` held, and exiting `code`.
fn our_launch(code: i32) -> (Entry, Launched) {
    kickstart_launching(move || Held::new(code).stopped())
}

/// `procinfo` of the process `launched` is, which the call before it made: it names it as the job's launch.
fn procinfo_of(launched: &Launched) -> Entry {
    let named = launched.clone();
    reply_at(&["procinfo"], move || exited(0, &block_of(named.pid()), ""))
}

/// `procinfo` of the process `launched` is, which is not the job's launch (a stranger's pid): exit 0 all the
/// same, and no job block.
fn stranger_of(launched: &Launched) -> Entry {
    let asked = launched.clone();
    reply_at(&["procinfo"], move || {
        let pid = asked.pid();
        exited(
            0,
            &format!("program path = /bin/sleep\n(pid {pid} is not managed by launchd)\n"),
            "",
        )
    })
}

/// A `SIGCONT` that lets `launched` go, as the real one would: it runs to its end.
fn sigcont_ending(launched: &Launched) -> Entry {
    let launched = launched.clone();
    sigcont().then(move || launched.finish())
}

/// Appends `text` to `path` when the call it is attached to is made: a program writing to its log.
fn grow(path: &Path, text: &'static str) -> impl FnOnce() + Send + 'static {
    let path = path.to_path_buf();
    move || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open the log");
        file.write_all(text.as_bytes()).expect("append to the log");
    }
}

/// An interrupt, as a terminal sends it.
fn interrupt() {
    // SAFETY: `raise` takes a valid signal number, and the window's handler, or a stand-in, is in place.
    assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
}

/// A termination, as a service manager sends it.
fn terminate() {
    // SAFETY: as for `interrupt`.
    assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0, "raise SIGTERM");
}

/// A writer for the notes that runs `on_write` when the first one is written: the moment `run` is about to
/// block on a launch it found in flight, and so the place for a test to end that launch or interrupt.
struct Noting {
    written: Vec<u8>,
    on_write: Option<Box<dyn FnOnce() + Send>>,
    closed: bool,
}

impl Noting {
    fn on_write(action: impl FnOnce() + Send + 'static) -> Noting {
        Noting {
            written: Vec::new(),
            on_write: Some(Box::new(action)),
            closed: false,
        }
    }

    /// The same, and every write fails.
    fn closed(self) -> Noting {
        Noting { closed: true, ..self }
    }

    fn text(&self) -> String {
        String::from_utf8(self.written.clone()).expect("UTF-8 notes")
    }
}

impl std::io::Write for Noting {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(action) = self.on_write.take() {
            action();
        }
        if self.closed {
            return Err(std::io::Error::other("the output is closed"));
        }
        self.written.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A scheduled run in flight, which ends as `run` begins to wait for it: its pid, and the notes writer that
/// ends it. A pass over it ends, and another is made.
fn in_flight_ending() -> (u32, Noting) {
    let mut scheduled = Held::new(0);
    let pid = scheduled.pid();
    (
        pid,
        Noting::on_write(move || {
            scheduled.finish();
        }),
    )
}

fn call(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

fn calls_of(fake: &Fake, verb: &str) -> Vec<Vec<String>> {
    fake.calls().into_iter().filter(|call| call[0] == verb).collect()
}

/// The verb of every call so far.
fn verbs(fake: &Fake) -> Vec<String> {
    fake.calls().into_iter().map(|call| call[0].clone()).collect()
}

fn task_logging_to(path: &Path) -> DaemonSpec {
    let mut common = base_fields(ID, vec!["/bin/true".to_string()]).common;
    common.logs = Some(path.to_path_buf());
    TaskSpec::new(TaskFields {
        common,
        schedule: None,
        stop_action: Default::default(),
    })
    .unwrap_or_else(|e| panic!("not a valid task: {e}"))
    .into()
}

/// The snapshot `read` makes of an installed task that logs to `logs`, or to nothing.
fn snapshot(tree: &Tree, logs: Option<&Path>) -> Plist {
    let spec = match logs {
        Some(path) => task_logging_to(path),
        None => task_spec(None),
    };
    tree.put(false, ID, plists_of(&spec).job);
    marked(tree.read())
}

/// `begin_run` of `plist` over `fake`, drawing on `handed` and writing its notes to `notes`. It changes the
/// process's signal dispositions, so it takes its turn.
fn begin_noting(fake: &Fake, plist: Plist, handed: &mut Reservation, notes: &mut dyn std::io::Write) -> Result<Run> {
    let _turn = serial();
    launchd_over(fake.clone()).begin_run(plist, handed, notes)
}

/// `begin_run` of `plist` over `fake`, drawing on a reservation made for [`Step::Start`], as `run` does, and
/// writing its notes to `notes`.
fn begin_with(fake: &Fake, plist: Plist, notes: &mut dyn std::io::Write) -> Result<Run> {
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");
    begin_noting(fake, plist, &mut handed, notes)
}

/// `begin_run` of `plist` over `fake`, drawing on `handed`.
fn begin_on(fake: &Fake, plist: Plist, handed: &mut Reservation) -> (Result<Run>, String) {
    let mut notes = Vec::new();
    let begun = begin_noting(fake, plist, handed, &mut notes);
    (begun, String::from_utf8(notes).expect("UTF-8 notes"))
}

fn begin(fake: &Fake, plist: Plist) -> (Result<Run>, String) {
    let mut notes = Vec::new();
    let begun = begin_with(fake, plist, &mut notes);
    (begun, String::from_utf8(notes).expect("UTF-8 notes"))
}

fn begun(fake: &Fake, plist: Plist) -> Run {
    match begin(fake, plist).0 {
        Ok(run) => run,
        Err(e) => panic!("begin_run failed: {e}"),
    }
}

fn failed(fake: &Fake, plist: Plist) -> Error {
    match begin(fake, plist).0 {
        Ok(_) => panic!("begin_run succeeded"),
        Err(e) => e,
    }
}

/// The script of our own launch that is held at once and exits `code`: the load, one pass, and the
/// `SIGCONT`, which ends it.
fn held_at_once(code: i32) -> (Fake, Launched) {
    let (kick, launched) = our_launch(code);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched),
        sigcont_ending(&launched),
    ]);
    (fake, launched)
}

/// What `run`'s output is read from, taken apart: the log's path, its offset and its marker's line.
fn log_of(run: &Run) -> (std::path::PathBuf, u64, String) {
    match run.output() {
        OutputSource::LogFile {
            path,
            offset,
            marker: Some(marker),
        } => (path, offset, marker.line),
        _ => panic!("a task with logs follows its log file, marked"),
    }
}

fn assert_stranded(e: &Error, what: &str) {
    assert!(
        matches!(e, Error::Step { what: said, .. }
            if said.starts_with(&format!("`launchctl debug` held the next launch of `{ID}`"))),
        "{what}: expected the stranded hold, got {e:?}"
    );
}

/// The stranded hold whose cause is an interrupt that ended a wait.
fn assert_interrupted(e: &Error, what: &str) {
    assert_stranded(e, what);
    assert!(
        matches!(e, Error::Step { source, .. } if matches!(**source, Error::Os { .. })),
        "{what}: {e:?}"
    );
    assert!(
        e.report().to_string().contains("interrupted by SIGINT, received"),
        "{what}: {}",
        e.report()
    );
}

/// The in-doubt error of a launch that was gone before goetia could watch it, inside the stranded hold:
/// the hold is pending, and whose launch it was is not known.
fn assert_vanished(e: &Error, pid: u32) {
    assert_stranded(e, "a launch that vanished");
    assert!(
        matches!(e, Error::Step { source, .. }
        if matches!(&**source, Error::RequestInDoubt { request, reached: true, note: Some(note), .. }
            if request == &format!("launchctl kickstart -p system/{ID}")
                && note.starts_with(&format!(
                    "the launch it named, process {pid}, was gone, or its pid was another process's, before goetia could watch it"
                )))),
        "{e:?}"
    );
    assert!(e.is_indeterminate(), "exit 4: the run may have started: {e:?}");
}

/// The unrecorded run of a launch of ours that ended before goetia could follow it: one error whichever read
/// found it gone, never the stranded hold, and exit 4.
fn assert_ended_unfollowed(e: &Error) {
    assert!(
        matches!(e, Error::RunUnrecorded { id, reason, .. }
            if id == ID && reason.starts_with("its launch ended before goetia could follow it")),
        "expected the launch that ended before goetia could follow it, got {e:?}"
    );
    #[cfg(feature = "cli")]
    assert_eq!(
        ExitCode::of(e),
        ExitCode::Indeterminate,
        "exit 4, as `RunUnrecorded` is"
    );
}

fn assert_unfollowed(e: &Error, pid: u32) {
    assert!(
        matches!(e, Error::RunUnrecorded { id, reason, .. }
        if id == ID
            && *reason == format!(
                "the `launchctl kickstart` goetia made for it launched process {pid} with no hold on it, \
                 so goetia does not follow it"
            )),
        "expected the unfollowed launch of {pid}, got {e:?}"
    );
    #[cfg(feature = "cli")]
    assert_eq!(
        ExitCode::of(e),
        ExitCode::Indeterminate,
        "exit 4, as `RunUnrecorded` is"
    );
}

// The order of a run ==================================================================================================

/// `debug` arms the hold before `kickstart -p` starts the launch; the pid is watched before `SIGCONT` lets
/// it go: the child exits at the `SIGCONT`, and its exit is still seen, which only a watch registered
/// before it could. The pid is asked of `launchctl procinfo` after the kickstart named it and before any
/// signal.
#[skuld::test]
fn a_run_arms_debug_before_kickstart_and_resumes_after_registering_the_watch() {
    let (fake, launched) = held_at_once(3);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    fake.assert_spent("the load, one pass and the resume");
    assert_eq!(
        fake.calls(),
        [
            call(&["print", &target()]),
            call(&["debug", &target(), "--start-suspended"]),
            call(&["kickstart", "-p", &target()]),
            call(&["procinfo", &launched.pid().to_string()]),
            call(&["kill", "SIGCONT", &target()]),
        ]
    );
    assert_eq!(run.wait().expect("the exit"), Exit::Code(3));
}

/// An exit the kernel reported without a status ended the run, and its result is not known: the run is
/// unrecorded, and the error says it ended, not that it may still be going.
#[skuld::test]
fn a_run_whose_exit_came_without_a_status_is_unrecorded() {
    let _unreported = test_hook::statuses(0);
    let (fake, _launched) = held_at_once(3);
    let tree = Tree::new();
    let run = begun(&fake, snapshot(&tree, None));

    let e = run.wait().expect_err("an exit with no status");

    assert!(
        matches!(&e, Error::RunUnrecorded { id, reason, .. } if id == ID && !reason.contains("may")),
        "{e:?}"
    );
    assert!(e.is_indeterminate(), "exit 4, as `RunUnrecorded` is");
}

/// The log's length is read while the launch is held, and before `SIGCONT`: the file grows at the
/// `kickstart` (before the read) and at the `SIGCONT` (after it). The run's offset is the length with the
/// first, not an earlier one, and not the later one.
#[skuld::test]
fn the_log_offset_is_taken_while_the_launch_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    std::fs::write(&log, "older").expect("seed the log");
    let (before_read, after_resume) = (grow(&log, "abcdefg"), grow(&log, "0123456789"));
    let (kick, launched) = kickstart_launching(move || {
        before_read();
        Held::new(0).stopped()
    });
    let ending = launched.clone();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched),
        sigcont().then(move || {
            after_resume();
            ending.finish();
        }),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, Some(&log)));

    let (path, offset, _) = log_of(&run);
    assert_eq!(path, log);
    assert_eq!(
        offset,
        ("older".len() + "abcdefg".len()) as u64,
        "the length while the launch was held"
    );
    assert_eq!(
        std::fs::metadata(&log).unwrap().len(),
        offset + 10,
        "the SIGCONT's write is after it"
    );
}

/// How a held launch ended is how the run ended: an exit code, or a signal.
#[skuld::test]
fn a_held_launch_is_followed_and_its_exit_returned() {
    fn followed(make: impl FnOnce() -> Held + Send + 'static) -> Exit {
        let (kick, launched) = kickstart_launching(make);
        let fake = Fake::new([
            print(IDLE),
            debug(),
            kick,
            procinfo_of(&launched),
            sigcont_ending(&launched),
        ]);
        let tree = Tree::new();
        begun(&fake, snapshot(&tree, None)).wait().expect("the exit")
    }

    assert_eq!(followed(|| Held::new(3).stopped()), Exit::Code(3));
    assert_eq!(followed(|| Held::signalled(15).stopped()), Exit::Signal(15));
}

/// A hold already pending answers 37, which is not an error: the launch `kickstart -p` starts consumes it.
#[skuld::test]
fn debug_exit_37_proceeds_and_consumes_the_pending_hold() {
    let (kick, launched) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug_exiting(37),
        kick,
        procinfo_of(&launched),
        sigcont_ending(&launched),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    fake.assert_spent("the hold was pending, and the launch was held all the same");
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// What `debug` answers is the time read after it returned, whether the hold was new or pending.
#[skuld::test]
fn debug_answers_the_time_after_it_whether_the_hold_was_new_or_pending() {
    let fake = Fake::new([debug(), debug_exiting(37)]);
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");

    let before = proc::now();
    let first = mgr.launchctl.debug_start_suspended(ID, &mut handed).expect("a hold");
    let second = mgr
        .launchctl
        .debug_start_suspended(ID, &mut handed)
        .expect("a pending hold");
    let after = proc::now();

    assert!(
        before <= first && first <= second && second <= after,
        "{before} <= {first} <= {second} <= {after}"
    );
}

// What the kernel shows of the launch `kickstart -p` named ============================================================

/// Stopped, and started after the `debug`: the launch it held, followed. Covered by every test above; this
/// one names the class, and that nothing was waited for.
#[skuld::test]
fn a_stopped_launch_started_after_debug_is_followed_with_no_note() {
    let (fake, _launched) = held_at_once(0);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    assert_eq!(notes, "");
    assert_eq!(calls_of(&fake, "debug").len(), 1);
}

/// Stopped, and started before the `debug`: an older launch that took a hold. It is resumed by label and
/// waited for (the note says so), and the run starts over: a fresh `debug`, which finds the pending hold
/// (37), and our own launch, followed.
#[skuld::test]
fn an_older_launch_that_took_a_hold_is_released_and_waited_for_and_the_run_starts_over() {
    let mut older = Held::new(0).stopped();
    let a = older.pid();
    let (kick, ours) = our_launch(5);
    let fake = Fake::new([
        print(IDLE),
        debug_exiting(37),
        kickstart(a),
        procinfo(a),
        sigcont().then(move || {
            older.finish();
        }),
        debug_exiting(37),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let run = result.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the older launch, then our own");
    assert_eq!(notes, NOTE);
    assert_eq!(
        verbs(&fake),
        [
            "print",
            "debug",
            "kickstart",
            "procinfo",
            "kill",
            "debug",
            "kickstart",
            "procinfo",
            "kill"
        ]
    );
    assert_eq!(run.wait().expect("the exit"), Exit::Code(5));
}

/// Not stopped, and started before the `debug`: a scheduled run in flight. The note says it is waited for,
/// it ends while the wait blocks, and the run starts over. One kqueue serves both passes, and the log's
/// offset is the one taken while our own launch was held.
#[skuld::test]
fn a_run_in_flight_is_waited_for_and_the_run_starts_over() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let mut scheduled = Held::new(0);
    let a = scheduled.pid();
    let scheduled_run_writes = grow(&log, "scheduled run\n");
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        procinfo(a),
        debug_exiting(37),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();
    let _one_kqueue = test_hook::queues(1);
    // The run it waits for writes and ends as the note is written, which is as the wait begins.
    let mut notes = Noting::on_write(move || {
        scheduled_run_writes();
        scheduled.finish();
    });

    let run =
        begin_with(&fake, snapshot(&tree, Some(&log)), &mut notes).unwrap_or_else(|e| panic!("begin_run failed: {e}"));

    fake.assert_spent("two passes");
    assert_eq!(notes.text(), NOTE, "said once, before the wait");
    assert_eq!(
        verbs(&fake),
        [
            "print",
            "debug",
            "kickstart",
            "procinfo",
            "debug",
            "kickstart",
            "procinfo",
            "kill"
        ]
    );
    assert!(calls_of(&fake, "kill").len() == 1, "only our own launch is resumed");
    let (_, offset, marker) = log_of(&run);
    assert_eq!(
        offset,
        "scheduled run\n".len() as u64,
        "the length while our own launch was held"
    );
    assert_eq!(
        marker,
        format!("goetia-run {:08x}", ours.pid()),
        "our own launch is the one followed"
    );
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// A note that cannot be written does not end the run: it is a warning, and the wait goes on.
#[skuld::test]
fn a_note_that_cannot_be_written_does_not_end_the_run() {
    let mut scheduled = Held::new(0);
    let a = scheduled.pid();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        procinfo(a),
        debug_exiting(37),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();
    let mut notes = Noting::on_write(move || {
        scheduled.finish();
    })
    .closed();

    let run = begin_with(&fake, snapshot(&tree, None), &mut notes)
        .unwrap_or_else(|e| panic!("a note that failed ended the run: {e}"));

    fake.assert_spent("the run went on to its second pass");
    assert_eq!(notes.text(), "", "nothing was written");
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// Not stopped, and started after the `debug`: our own `kickstart` launched it with no hold, which is not
/// followed, nothing is resumed or waited for, and no pass follows it.
#[skuld::test]
fn an_unheld_launch_started_after_debug_is_run_unfollowed() {
    let (kick, launched) = kickstart_launching(|| Held::new(0));
    let fake = Fake::new([print(IDLE), debug(), kick, procinfo_of(&launched)]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let e = result.err().expect("the launch is not followed");
    assert_unfollowed(&e, launched.pid());
    fake.assert_spent("nothing was sent after the kickstart");
    assert_eq!(notes, "", "nothing was waited for");
}

/// The pid `kickstart -p` named is gone by the time it is registered: goetia never saw it, so it cannot say
/// whose launch it was (this run's, which may have failed to start, or a scheduled run's). The run ends in
/// doubt, in the stranded hold, and no pass follows: the script holds no second `debug`.
#[skuld::test]
fn a_launch_that_is_gone_at_the_registration_is_in_doubt_and_starts_no_second_pass() {
    let mut gone = Held::new(0);
    let a = gone.pid();
    gone.finish();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a)]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    assert_vanished(&run.err().expect("the launch vanished"), a);
    fake.assert_spent("nothing was sent after the kickstart");
    assert_eq!(notes, "", "an ended launch is not waited for");
}

/// A launch that has exited and is not reaped is no process to the watch or to a read: the same.
#[skuld::test]
fn a_launch_that_exited_unreaped_is_in_doubt_and_starts_no_second_pass() {
    let mut zombie = Held::new(0);
    let a = zombie.pid();
    zombie.exit_unreaped();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a)]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    assert_vanished(&run.err().expect("the launch vanished"), a);
    fake.assert_spent("nothing was sent after the kickstart");
    assert_eq!(notes, "");
}

/// Our own launch, which started after the `debug`, is no different when it is gone at the registration:
/// it is in doubt too.
#[skuld::test]
fn our_launch_that_is_gone_at_the_registration_is_in_doubt_too() {
    let (kick, launched) = kickstart_launching(|| {
        let mut ended = Held::new(0);
        ended.finish();
        ended
    });
    let fake = Fake::new([print(IDLE), debug(), kick]);
    let tree = Tree::new();

    let (run, _) = begin(&fake, snapshot(&tree, None));

    assert_vanished(&run.err().expect("the launch vanished"), launched.pid());
    fake.assert_spent("nothing was sent after the kickstart");
}

/// A launch whose program is `xpcproxy` is waited for its exec before it is read. It is answered as the
/// call that names it returns, so its exec comes after, or before, the registration, and the program it
/// execs goes on running until the note says the run waits for it: either way it is a run in flight, and
/// the run goes on to its own launch.
#[skuld::test]
fn an_xpcproxy_launch_that_execs_is_read_after_the_exec_and_the_run_goes_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let proxy = Launched::of(Held::at_xpcproxy(dir.path(), "read _; exec /bin/sh -c 'read _'"));
    let a = proxy.pid();
    let (answered, ended) = (proxy.clone(), proxy.clone());
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a).then(move || answered.advance()),
        procinfo(a),
        debug_exiting(37),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();
    let mut notes = Noting::on_write(move || ended.release());

    let run = begin_with(&fake, snapshot(&tree, None), &mut notes).unwrap_or_else(|e| panic!("begin_run failed: {e}"));

    fake.assert_spent("the second pass followed the first");
    assert_eq!(notes.text(), NOTE, "the exec'd program is a run in flight");
    assert_eq!(calls_of(&fake, "kill").len(), 1, "only our own launch is resumed");
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// A pid `launchctl procinfo` does not name as the job's launch is another process's, whatever the kernel
/// shows of it: here a stopped one that started before the `debug` (an older launch that took a hold, if it
/// were the label's). The run ends in doubt in the stranded hold, and nothing is sent after the `procinfo`:
/// no `SIGCONT` of a process that may be a stranger's, and no second pass.
#[skuld::test]
fn a_pid_procinfo_does_not_name_is_in_doubt_and_nothing_is_sent_after() {
    let stranger = Launched::of(Held::new(0).stopped());
    let a = stranger.pid();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), stranger_of(&stranger)]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    assert_vanished(&run.err().expect("the pid is not the launch"), a);
    fake.assert_spent("nothing was sent after the procinfo");
    assert_eq!(notes, "", "a stranger is not waited for");
}

/// The same for a stopped process that started after the `debug`, which would be the held launch: the
/// `procinfo`'s answer is the decision, so it is not resumed.
#[skuld::test]
fn our_stopped_pid_procinfo_does_not_name_is_in_doubt_and_is_not_resumed() {
    let (kick, launched) = our_launch(0);
    let fake = Fake::new([print(IDLE), debug(), kick, stranger_of(&launched)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_vanished(&e, launched.pid());
    fake.assert_spent("nothing was sent after the procinfo");
}

/// A `procinfo` that could not be run says nothing of the pid: the stranded hold, with its cause, and
/// nothing sent after it.
#[skuld::test]
fn a_procinfo_that_cannot_be_run_is_the_stranded_hold() {
    let (kick, _launched) = our_launch(0);
    let not_run = ClientError::NotRun(std::io::Error::other("no launchctl").into());
    let fake = Fake::new([print(IDLE), debug(), kick, fail(&["procinfo"], not_run)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_stranded(&e, "a procinfo that was not run");
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::Spawn { .. })),
        "the cause stays under it: {e:?}"
    );
    fake.assert_spent("nothing was sent after the failure");
}

/// The `procinfo` is a read by pid like the others: it counts only if the watch shows no exit of the pid
/// after it. The launch here is named, and ends during the call, so the pid is gone at the read: ours, so
/// it ended before goetia could follow it, and it is not resumed.
#[skuld::test]
fn a_launch_that_ends_during_the_procinfo_is_gone_at_the_read() {
    let (kick, launched) = our_launch(0);
    let ending = launched.clone();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched).then(move || ending.finish()),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_ended_unfollowed(&e);
    assert!(
        matches!(&e, Error::RunUnrecorded { reason, .. } if reason.contains("(the process's status: exit code 0)")),
        // `finish` reaps the child inside the procinfo, so its exit event is queued before the check reads
        // the watch: the status is certain here, as it is not for an exit that comes after the check.
        "the exit the watch kept is shown, though the read that raced it was discarded: {e:?}"
    );
    fake.assert_spent("nothing was sent after the procinfo");
}

/// A launch of ours of a task without `logs`, whose program cannot be started, exits in `xpcproxy` with no
/// exec: it ended before goetia could follow it, whichever of the exit and the read of the pid came first,
/// and its exit is no hold left behind: nothing is stranded, and nothing is resumed.
#[skuld::test]
fn our_launch_that_exits_before_its_exec_ended_before_goetia_could_follow_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let at = dir.path().to_path_buf();
    let (kick, launched) = kickstart_launching(move || Held::at_xpcproxy(&at, "read _; exit 3"));
    let releasing = launched.clone();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched).then(move || releasing.release()),
    ]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    assert_ended_unfollowed(&run.err().expect("the launch ended"));
    fake.assert_spent("nothing was sent after the procinfo");
    assert_eq!(notes, "", "nothing was waited for");
}

// Reservations ========================================================================================================

/// The first pass draws on the reservation `run` made, and a later one on one of its own: the handed one
/// holds exactly the first pass's calls, so a second pass that drew on it would trip the reservation's
/// assertion.
#[skuld::test]
fn a_later_pass_makes_its_own_reservation_and_the_first_draws_only_what_it_was_handed() {
    let (a, mut notes) = in_flight_ending();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        procinfo(a),
        debug(),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();
    // The load's read, `debug`, `kickstart -p` and `procinfo`: four calls, two requests.
    let mut handed = Reservation::make(2, 4 * Role::COUNTED_FILES).expect("a reservation");

    let run = begin_noting(&fake, snapshot(&tree, None), &mut handed, &mut notes);

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    assert_eq!(
        bounded::test_hook::left(&handed),
        (0, 0),
        "the first pass spends what it was handed"
    );
}

/// A later pass that cannot reserve is the stranded hold: the first pass's `debug` left one pending.
#[skuld::test]
fn a_later_pass_that_cannot_reserve_returns_the_stranded_hold_error() {
    let (a, mut notes) = in_flight_ending();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), procinfo(a)]);
    let tree = Tree::new();
    let mut handed = Reservation::make(2, 4 * Role::COUNTED_FILES).expect("a reservation");
    let _short = bounded::test_hook::temp_files(0);

    let run = begin_noting(&fake, snapshot(&tree, None), &mut handed, &mut notes);

    let e = run.err().expect("the second pass cannot reserve");
    assert_stranded(&e, "a reservation that failed");
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::Reservation { .. })),
        "{e:?}"
    );
    fake.assert_spent("the first pass went out before the reservation failed");
}

/// The longest first pass: the load that is refused and reads again (three calls, one request), then
/// `debug`, `kickstart -p`, `procinfo` and `SIGCONT`. It stays within what `run` reserves for a start, and
/// uses 7 calls and 4 requests of it.
#[skuld::test]
fn begin_run_draws_no_more_than_a_start_reserves() {
    let (kick, ours) = our_launch(0);
    let tree = Tree::new();
    let plist = snapshot(&tree, None);
    let job = plist.job().unwrap().location.path.clone();
    let fake = Fake::new([
        not_loaded(),
        reply(
            &["bootstrap", "system", job.to_str().expect("a UTF-8 path")],
            exited(5, "", "Bootstrap failed: 5: Input/output error\n"),
        ),
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");

    let (run, _) = begin_on(&fake, plist, &mut handed);

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the longest first pass");
    assert_eq!(
        bounded::test_hook::left(&handed),
        (1, 6 * Role::COUNTED_FILES),
        "a start reserves 5 requests and 13 calls"
    );
}

/// The kqueue is made before anything is sent, so failing to make one holds no launch.
#[skuld::test]
fn a_kqueue_that_cannot_be_made_fails_before_debug_and_strands_no_hold() {
    let fake = Fake::new([]);
    let tree = Tree::new();
    let _none = test_hook::queues(0);

    let e = failed(&fake, snapshot(&tree, None));

    assert!(e.to_string().contains("kqueue"), "{e}");
    assert_eq!(fake.calls(), Vec::<Vec<String>>::new(), "nothing may be sent");
}

// What a run follows ==================================================================================================

/// A task with `logs` follows its log from the marker of the pid that was watched, written as 8 hex
/// digits; one without has no output to follow.
#[skuld::test]
fn the_marker_names_the_followed_pid_as_8_hex_digits_and_a_task_without_logs_has_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let (fake, launched) = held_at_once(0);
    let tree = Tree::new();

    let with_logs = begun(&fake, snapshot(&tree, Some(&log)));

    let (path, _, line) = log_of(&with_logs);
    assert_eq!(path, log);
    assert_eq!(line, format!("goetia-run {:08x}", launched.pid()));
    match with_logs.output() {
        OutputSource::LogFile {
            marker: Some(marker), ..
        } => {
            assert_eq!(
                (marker.prefix, marker.id_len, marker.earlier_start_possible),
                ("goetia-run", 8, false)
            );
        }
        _ => panic!("a marked log file"),
    }

    let (fake, _) = held_at_once(0);
    let without = begun(&fake, snapshot(&Tree::new(), None));
    assert!(matches!(without.output(), OutputSource::None));
}

/// A scheduled run that was registered and then ended leaves no event on the queue when the next pass
/// begins: the exit it queued is deleted with its registration, so the followed launch cannot be handed it.
#[skuld::test]
fn a_registration_left_by_an_ended_run_is_deleted_before_the_next_pass() {
    let mut scheduled = Held::new(0);
    let a = scheduled.pid();
    let (kick, ours) = our_launch(7);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        procinfo(a),
        debug_exiting(37),
        kick,
        procinfo_of(&ours),
        sigcont(),
    ]);
    let tree = Tree::new();
    let mut notes = Noting::on_write(move || {
        scheduled.finish();
    });

    let run = begin_with(&fake, snapshot(&tree, None), &mut notes).unwrap_or_else(|e| panic!("begin_run failed: {e}"));

    assert_eq!(
        pending(&run.registered),
        0,
        "the scheduled run's exit is not left queued"
    );
    ours.finish();
    assert_eq!(
        run.wait().expect("the exit"),
        Exit::Code(7),
        "and the exit followed is ours"
    );
}

// What strands a hold, and what does not ==============================================================================

/// A `SIGCONT` that reports no process: the held launch was gone by the time it could be resumed, which is
/// unrecorded, and nothing is stranded.
#[skuld::test]
fn a_sigcont_that_reports_no_process_is_run_unrecorded_with_the_reason() {
    let (kick, launched) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched),
        reply(
            &["kill", "SIGCONT", &target()],
            exited(3, "", "No process to signal.\n"),
        ),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(
        matches!(&e, Error::RunUnrecorded { reason, .. } if reason == "it was ended before goetia could resume it"),
        "{e:?}"
    );
}

/// A failure after `debug` held the next launch says so, and what ends the hold.
#[skuld::test]
fn a_kickstart_failure_after_debug_names_the_stranded_hold() {
    let fake = Fake::new([
        print(IDLE),
        debug(),
        reply(&["kickstart", "-p", &target()], exited(1, "", "Could not kickstart\n")),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_stranded(&e, "a kickstart that failed");
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::Command { .. })),
        "the cause stays under it: {e:?}"
    );
    fake.assert_spent("nothing was sent after the failure");
}

/// A failure on a later pass is the stranded hold, as on the first: the hold is pending, and nothing
/// resumed.
#[skuld::test]
fn a_kickstart_failure_on_a_later_pass_names_the_stranded_hold() {
    let (a, mut notes) = in_flight_ending();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        procinfo(a),
        debug(),
        reply(&["kickstart", "-p", &target()], exited(1, "", "Could not kickstart\n")),
    ]);
    let tree = Tree::new();

    let e = begin_with(&fake, snapshot(&tree, None), &mut notes)
        .err()
        .expect("the second pass's kickstart failed");

    assert_stranded(&e, "a later pass's kickstart that failed");
    fake.assert_spent("nothing was sent after the failure");
}

/// A `debug` that failed on the first pass held nothing: its own error, with no mention of a hold.
#[skuld::test]
fn a_debug_failure_other_than_37_is_its_error_and_nothing_was_held() {
    let fake = Fake::new([print(IDLE), debug_exiting(5)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(matches!(&e, Error::Command { .. }), "{e:?}");
    fake.assert_spent("nothing was sent after the failed debug");
}

/// A `debug` that failed on a later pass is the stranded hold: the first pass armed one.
#[skuld::test]
fn a_debug_failure_on_a_later_pass_names_the_stranded_hold() {
    let (a, mut notes) = in_flight_ending();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), procinfo(a), debug_exiting(5)]);
    let tree = Tree::new();

    let e = begin_with(&fake, snapshot(&tree, None), &mut notes)
        .err()
        .expect("the second pass's debug failed");

    assert_stranded(&e, "a later pass's debug that failed");
    fake.assert_spent("nothing was sent after the failed debug");
}

/// A `kickstart -p` that prints no whole pid cannot be followed, and it may have started a launch: the
/// stranded hold, with the start unconfirmed under it.
#[skuld::test]
fn a_kickstart_that_names_no_pid_is_the_stranded_hold_with_the_start_unconfirmed() {
    let fake = Fake::new([
        print(IDLE),
        debug(),
        reply(&["kickstart", "-p", &target()], exited(0, "not a pid\n", "")),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_stranded(&e, "a kickstart with no pid");
    assert!(e.is_indeterminate(), "the start may have happened: {e:?}");
    assert!(
        matches!(&e, Error::Step { source, .. }
            if matches!(&**source, Error::Step { source: unconfirmed, .. }
                if matches!(**unconfirmed, Error::StartUnconfirmed { .. }))),
        "{e:?}"
    );
}

/// A `SIGCONT` that may have been sent is in doubt: the run may have started and be going unfollowed, and
/// the error says so, still naming what ends the hold.
#[skuld::test]
fn a_sigcont_that_ends_in_doubt_says_the_run_may_have_started() {
    let (kick, launched) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched),
        fail(
            &["kill", "SIGCONT", &target()],
            ClientError::MayHaveRun(cosca::error::Error::Containment {
                detail: "forced".into(),
                source: None,
            }),
        ),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(e.is_indeterminate(), "{e:?}");
    assert!(
        e.to_string().contains("and the run may have started:") && e.to_string().contains("goetia daemon stop"),
        "{e}"
    );
}

// Interrupts ==========================================================================================================

/// Nothing is started after an interrupt: one recorded at any time before the `SIGCONT` that resumes our
/// launch ends the run as the stranded hold, and no `SIGCONT` is sent, whichever call it came in. One that
/// comes after that `SIGCONT` is held back until `begin_run` returns, and re-raised then, once: the run is
/// followed, and the interrupt is the caller's.
#[skuld::test]
fn an_interrupt_before_the_resume_strands_and_one_after_it_is_held() {
    let stand_in = StandIn::install();
    let (kick, launched) = our_launch(0);
    let fake = Fake::new([print(IDLE), debug().then(interrupt), kick, procinfo_of(&launched)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_interrupted(&e, "an interrupt before the resume");
    assert!(
        e.report()
            .to_string()
            .contains("before goetia resumed the launch it held; goetia sent nothing after acting on it; that launch waits stopped"),
        "{}",
        e.report()
    );
    fake.assert_spent("no SIGCONT was sent after the interrupt");
    assert_eq!(calls_of(&fake, "kill").len(), 0);
    assert_eq!(stand_in.seen(), 1, "re-raised once begin_run returns");

    let (kick, launched) = our_launch(0);
    let ending = launched.clone();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
        procinfo_of(&launched),
        sigcont().then(move || {
            interrupt();
            assert_eq!(seen(), 1, "held at the resume: not re-raised yet");
            ending.finish();
        }),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    assert_eq!(stand_in.seen(), 2, "re-raised once, after begin_run returned");
    assert!(stand_in.saw(libc::SIGINT));
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// The same rule for the `SIGCONT` that releases an older launch which took the hold: an interrupt recorded
/// before it ends the run, and the launch is not released.
#[skuld::test]
fn an_interrupt_before_the_release_of_an_older_launch_strands_and_sends_nothing() {
    let stand_in = StandIn::install();
    let older = Held::new(0).stopped();
    let fake = Fake::new([
        print(IDLE),
        debug().then(interrupt),
        kickstart(older.pid()),
        procinfo(older.pid()),
    ]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let e = result.err().expect("the interrupt ended the run");
    assert_interrupted(&e, "an interrupt before the release");
    assert!(
        e.report()
            .to_string()
            .contains("before goetia released the launch that took the hold"),
        "{}",
        e.report()
    );
    fake.assert_spent("no SIGCONT was sent");
    assert_eq!(notes, "", "nothing was waited for");
    assert_eq!(stand_in.seen(), 1);
}

/// The error names the signals the one snapshot the decision was made on holds, which are the ones recorded
/// by then and no others: two signals recorded before the decision are both named, in order.
#[skuld::test]
fn the_interrupt_error_names_the_signals_recorded_by_the_decision() {
    let stand_in = StandIn::install();
    let (kick, launched) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug().then(|| {
            terminate();
            interrupt();
        }),
        kick,
        procinfo_of(&launched),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert_stranded(&e, "two interrupts before the resume");
    assert!(
        e.report()
            .to_string()
            .contains("interrupted by SIGINT and SIGTERM, received before goetia resumed the launch it held"),
        "{}",
        e.report()
    );
    assert_eq!(stand_in.seen(), 2, "both re-raised once begin_run returns");
}

/// A signal the user set to be ignored (`nohup` does so for SIGHUP) stays ignored: it is no interrupt, so a
/// wait for a scheduled run goes on to the run's own launch.
#[skuld::test]
fn an_ignored_signal_during_a_wait_does_not_interrupt_it() {
    let stand_in = StandIn::install();
    ignore(libc::SIGHUP);
    let mut scheduled = Held::new(0);
    let a = scheduled.pid();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        procinfo(a),
        debug(),
        kick,
        procinfo_of(&ours),
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();
    let mut notes = Noting::on_write(move || {
        send_to_process(libc::SIGHUP);
        scheduled.finish();
    });

    let run = begin_with(&fake, snapshot(&tree, None), &mut notes).unwrap_or_else(|e| panic!("begin_run failed: {e}"));

    fake.assert_spent("the second pass followed the first");
    assert_eq!(notes.text(), NOTE);
    assert_eq!(stand_in.seen(), 0, "an ignored signal is not re-raised either");
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// An interrupt recorded while the job loads ends the run before the first `debug`: no hold exists, so the
/// error is the interrupt itself and not the stranded hold, and nothing is sent after it.
#[skuld::test]
fn an_interrupt_during_the_load_ends_the_run_before_any_hold() {
    let stand_in = StandIn::install();
    let fake = Fake::new([print(IDLE).then(interrupt)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(matches!(&e, Error::Os { .. }), "no hold is stranded: {e:?}");
    assert!(
        e.report().to_string().contains(
            "interrupted by SIGINT, received before goetia armed a hold; goetia sent \
                 nothing after acting on it; no hold was armed"
        ),
        "{}",
        e.report()
    );
    fake.assert_spent("nothing was sent after the interrupt");
    assert_eq!(calls_of(&fake, "debug").len(), 0, "no hold was armed");
    assert_eq!(stand_in.seen(), 1, "the interrupt is re-raised once begin_run returns");
}

/// A `run` that fails puts the signal handlers back too.
#[skuld::test]
fn a_failed_begin_run_restores_the_signal_handlers() {
    let stand_in = StandIn::install();
    let fake = Fake::new([print(IDLE), debug_exiting(5)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(matches!(&e, Error::Command { .. }), "{e:?}");
    stand_in.assert_back();
}

/// An interrupt while the run waits for `xpcproxy`'s exec ends the wait: the stranded hold, with nothing
/// resumed and nothing started after it. The interrupt arrives before the registration, so it is the
/// window's record that the wait reads first.
#[skuld::test]
fn an_interrupt_while_waiting_for_xpcproxy_is_the_stranded_hold_and_starts_nothing() {
    let stand_in = StandIn::install();
    let dir = tempfile::tempdir().expect("tempdir");
    let proxy = Held::at_xpcproxy(dir.path(), "read _");
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(proxy.pid()).then(interrupt),
        procinfo(proxy.pid()),
    ]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let e = result.err().expect("the wait ended on the interrupt");
    assert_interrupted(&e, "an interrupt at xpcproxy");
    fake.assert_spent("nothing was sent after the interrupt");
    assert_eq!(notes, "");
    assert_eq!(stand_in.seen(), 1, "the interrupt is re-raised once begin_run returns");
}

/// An interrupt while the run waits for a run in flight ends the wait, and the run starts nothing after it:
/// no second `debug`, no second launch.
#[skuld::test]
fn an_interrupt_while_waiting_for_a_run_in_flight_is_the_stranded_hold_and_starts_nothing() {
    let stand_in = StandIn::install();
    let scheduled = Held::new(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(scheduled.pid()),
        procinfo(scheduled.pid()),
    ]);
    let tree = Tree::new();
    let mut notes = Noting::on_write(interrupt);

    let e = begin_with(&fake, snapshot(&tree, None), &mut notes)
        .err()
        .expect("the wait ended on the interrupt");

    assert_interrupted(&e, "an interrupt at a run in flight");
    fake.assert_spent("nothing was sent after the interrupt");
    assert_eq!(notes.text(), NOTE, "the note was said before the wait");
    assert_eq!(calls_of(&fake, "debug").len(), 1, "no second pass");
    assert_eq!(stand_in.seen(), 1);
}

/// An interrupt that arrives together with the end of the run waited for starts nothing either.
#[skuld::test]
fn an_interrupt_with_the_end_of_the_run_waited_for_starts_nothing() {
    let stand_in = StandIn::install();
    let mut scheduled = Held::new(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(scheduled.pid()),
        procinfo(scheduled.pid()),
    ]);
    let tree = Tree::new();
    // Both are due as the wait begins: the run ends, and the interrupt arrives.
    let mut notes = Noting::on_write(move || {
        interrupt();
        scheduled.finish();
    });

    let e = begin_with(&fake, snapshot(&tree, None), &mut notes)
        .err()
        .expect("the interrupt ended the run");

    assert_interrupted(&e, "an interrupt with the end of the run");
    fake.assert_spent("nothing was sent after the interrupt");
    assert_eq!(calls_of(&fake, "debug").len(), 1, "no second pass");
    assert_eq!(stand_in.seen(), 1);
}

/// An interrupt while the run waits for an older launch it resumed ends the wait the same way.
#[skuld::test]
fn an_interrupt_while_waiting_for_a_resumed_launch_is_the_stranded_hold_and_starts_nothing() {
    let stand_in = StandIn::install();
    let older = Held::new(0).stopped();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(older.pid()),
        procinfo(older.pid()),
        sigcont().then(interrupt),
    ]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let e = result.err().expect("the wait ended on the interrupt");
    assert_interrupted(&e, "an interrupt at a resumed launch");
    fake.assert_spent("nothing was sent after the interrupt");
    assert_eq!(notes, NOTE);
    assert_eq!(calls_of(&fake, "debug").len(), 1, "no second pass");
    assert_eq!(stand_in.seen(), 1);
}
