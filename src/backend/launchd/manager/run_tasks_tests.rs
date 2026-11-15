//! A task's `run` over a scripted `launchctl` and real child processes: each child stands for the
//! process a launch is (the pid `kickstart -p` prints), and the kernel's account of it, which `run` reads,
//! is the child's own. A launch our `kickstart` made is spawned by the call that names it, so that it
//! started after the `debug` before it; an older launch is spawned before `begin_run`. A held launch is a
//! stopped child, and the scripted `SIGCONT` lets it go, so the watch `run` follows is a real kqueue's.
//! Nothing here touches the system's own directories or services.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::super::interrupts::interrupts_tests::{StandIn, seen, serial};
use super::super::manager_tests::launchd_over;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, task_spec};
use super::super::watch::test_hook;
use super::super::watch::watch_tests::{Held, pending};
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded::{self, Role};
use crate::backend::client::ClientError;
use crate::backend::client::fake::{Entry, Fake, exited, fail, reply, reply_at};
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
    TaskSpec::new(TaskFields { common, schedule: None })
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
    let fake = Fake::new([print(IDLE), debug(), kick, sigcont_ending(&launched)]);
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
        e.report()
            .to_string()
            .contains("interrupted by SIGINT, SIGTERM or SIGHUP"),
        "{what}: {}",
        e.report()
    );
}

fn assert_unfollowed(e: &Error, pid: u32) {
    assert!(
        matches!(e, Error::RunUnrecorded { id, reason }
        if id == ID
            && *reason == format!(
                "the `launchctl kickstart` goetia made for it launched process {pid} with no hold on it, \
                 so goetia does not follow it"
            )),
        "expected the unfollowed launch of {pid}, got {e:?}"
    );
    assert_eq!(
        ExitCode::of(e),
        ExitCode::Indeterminate,
        "exit 4, as `RunUnrecorded` is"
    );
}

// The order of a run ==================================================================================================

/// `debug` arms the hold before `kickstart -p` starts the launch; the pid is watched before `SIGCONT` lets
/// it go: the child exits at the `SIGCONT`, and its exit is still seen, which only a watch registered
/// before it could.
#[skuld::test]
fn a_run_arms_debug_before_kickstart_and_resumes_after_registering_the_watch() {
    let (fake, _launched) = held_at_once(3);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    fake.assert_spent("the load, one pass and the resume");
    assert_eq!(
        fake.calls(),
        [
            call(&["print", &target()]),
            call(&["debug", &target(), "--start-suspended"]),
            call(&["kickstart", "-p", &target()]),
            call(&["kill", "SIGCONT", &target()]),
        ]
    );
    assert_eq!(run.wait().expect("the exit"), Exit::Code(3));
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
        let fake = Fake::new([print(IDLE), debug(), kick, sigcont_ending(&launched)]);
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
    let fake = Fake::new([print(IDLE), debug_exiting(37), kick, sigcont_ending(&launched)]);
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
        sigcont().then(move || {
            older.finish();
        }),
        debug_exiting(37),
        kick,
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let run = result.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the older launch, then our own");
    assert_eq!(notes, NOTE);
    assert_eq!(
        verbs(&fake),
        ["print", "debug", "kickstart", "kill", "debug", "kickstart", "kill"]
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
        debug_exiting(37),
        kick,
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
        ["print", "debug", "kickstart", "debug", "kickstart", "kill"]
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
        debug_exiting(37),
        kick,
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
    let fake = Fake::new([print(IDLE), debug(), kick]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let e = result.err().expect("the launch is not followed");
    assert_unfollowed(&e, launched.pid());
    fake.assert_spent("nothing was sent after the kickstart");
    assert_eq!(notes, "", "nothing was waited for");
}

/// The pid `kickstart -p` named is gone by the time it is registered: it ended, and the run starts over,
/// with a note for nothing.
#[skuld::test]
fn a_launch_that_is_gone_at_the_registration_starts_the_run_over() {
    let mut gone = Held::new(0);
    let a = gone.pid();
    gone.finish();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), debug(), kick, sigcont_ending(&ours)]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the second pass followed the first");
    assert_eq!(notes, "", "an ended launch is not waited for");
    assert_eq!(calls_of(&fake, "kill").len(), 1, "only our own launch is resumed");
}

/// A launch that has exited and is not reaped is no process either way it is read, and the run starts over.
#[skuld::test]
fn a_launch_that_exited_unreaped_starts_the_run_over() {
    let mut zombie = Held::new(0);
    let a = zombie.pid();
    zombie.exit_unreaped();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), debug(), kick, sigcont_ending(&ours)]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the second pass followed the first");
    assert_eq!(notes, "");
}

/// A launch whose program is `xpcproxy` is waited for its exec before it is read. It is let go as the call
/// that names it returns, so its exec comes after, or before, the registration: either way the run goes on
/// to its own launch.
#[skuld::test]
fn an_xpcproxy_launch_that_execs_is_read_after_the_exec_and_the_run_goes_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let proxy = Launched::of(Held::at_xpcproxy(dir.path(), "read _; exec /bin/sh -c 'read _'"));
    let a = proxy.pid();
    // The hook lets it go and drops its own handle; this one keeps the process until the test ends.
    let _alive = proxy.clone();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a).then(move || proxy.release()),
        debug_exiting(37),
        kick,
        sigcont_ending(&ours),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    fake.assert_spent("the second pass followed the first");
    assert_eq!(calls_of(&fake, "kill").len(), 1, "only our own launch is resumed");
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

// Reservations ========================================================================================================

/// The first pass draws on the reservation `run` made, and a later one on one of its own: the handed one
/// holds exactly the first pass's calls, so a second pass that drew on it would trip the reservation's
/// assertion.
#[skuld::test]
fn a_later_pass_makes_its_own_reservation_and_the_first_draws_only_what_it_was_handed() {
    let mut gone = Held::new(0);
    let a = gone.pid();
    gone.finish();
    let (kick, ours) = our_launch(0);
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), debug(), kick, sigcont_ending(&ours)]);
    let tree = Tree::new();
    // The load's read, `debug` and `kickstart -p`: three calls, two requests.
    let mut handed = Reservation::make(2, 3 * Role::COUNTED_FILES).expect("a reservation");

    let (run, _) = begin_on(&fake, snapshot(&tree, None), &mut handed);

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
    let mut gone = Held::new(0);
    let a = gone.pid();
    gone.finish();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a)]);
    let tree = Tree::new();
    let mut handed = Reservation::make(2, 3 * Role::COUNTED_FILES).expect("a reservation");
    let _short = bounded::test_hook::temp_files(0);

    let (run, _) = begin_on(&fake, snapshot(&tree, None), &mut handed);

    let e = run.err().expect("the second pass cannot reserve");
    assert_stranded(&e, "a reservation that failed");
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::Reservation { .. })),
        "{e:?}"
    );
    fake.assert_spent("the first pass went out before the reservation failed");
}

/// The longest first pass: the load that is refused and reads again (three calls, one request), then
/// `debug`, `kickstart -p` and `SIGCONT`. It stays within what `run` reserves for a start, and uses 6 calls
/// and 4 requests of it.
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
        sigcont_ending(&ours),
    ]);
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");

    let (run, _) = begin_on(&fake, plist, &mut handed);

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the longest first pass");
    assert_eq!(
        bounded::test_hook::left(&handed),
        (1, 7 * Role::COUNTED_FILES),
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
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), debug_exiting(37), kick, sigcont()]);
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
    let (kick, _launched) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
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
    let mut gone = Held::new(0);
    let a = gone.pid();
    gone.finish();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        debug(),
        reply(&["kickstart", "-p", &target()], exited(1, "", "Could not kickstart\n")),
    ]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

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
    let mut gone = Held::new(0);
    let a = gone.pid();
    gone.finish();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), debug_exiting(5)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

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
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::StartUnconfirmed { .. })),
        "{e:?}"
    );
}

/// A `SIGCONT` that may have been sent is in doubt: the run may have started and be going unfollowed, and
/// the error says so, still naming what ends the hold.
#[skuld::test]
fn a_sigcont_that_ends_in_doubt_says_the_run_may_have_started() {
    let (kick, _launched) = our_launch(0);
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kick,
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

/// SIGINT, SIGTERM and SIGHUP are held from before the first `debug` to after the `SIGCONT`, and
/// re-raised once `begin_run` returns: the window is `begin_run` itself.
#[skuld::test]
fn an_interrupt_during_begin_run_is_held_until_it_returns() {
    let stand_in = StandIn::install();
    let (kick, launched) = our_launch(0);
    let ending = launched.clone();
    let fake = Fake::new([
        print(IDLE),
        debug().then(interrupt),
        kick,
        sigcont().then(move || {
            assert_eq!(seen(), 0, "still held at the resume");
            ending.finish();
        }),
    ]);
    let tree = Tree::new();
    assert_eq!(stand_in.seen(), 0);

    let run = begun(&fake, snapshot(&tree, None));

    assert_eq!(stand_in.seen(), 1, "re-raised once, after the resume");
    assert!(stand_in.saw(libc::SIGINT));
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
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
    let fake = Fake::new([print(IDLE), debug(), kickstart(proxy.pid()).then(interrupt)]);
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
    let fake = Fake::new([print(IDLE), debug(), kickstart(scheduled.pid())]);
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
    let fake = Fake::new([print(IDLE), debug(), kickstart(scheduled.pid())]);
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
    let fake = Fake::new([print(IDLE), debug(), kickstart(older.pid()), sigcont().then(interrupt)]);
    let tree = Tree::new();

    let (result, notes) = begin(&fake, snapshot(&tree, None));

    let e = result.err().expect("the wait ended on the interrupt");
    assert_interrupted(&e, "an interrupt at a resumed launch");
    fake.assert_spent("nothing was sent after the interrupt");
    assert_eq!(notes, NOTE);
    assert_eq!(calls_of(&fake, "debug").len(), 1, "no second pass");
    assert_eq!(stand_in.seen(), 1);
}
