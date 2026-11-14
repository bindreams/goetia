//! A task's `run` over a scripted `launchctl` and real child processes: each child stands for the
//! process a launch is (the pid `kickstart -p` prints and `print` shows), and the scripted `SIGCONT` lets
//! it go, so the watch `run` follows is a real kqueue's. Nothing here touches the system's own
//! directories or services.

use std::path::Path;

use super::super::DebugOutcome;
use super::super::interrupts::interrupts_tests::{StandIn, seen, serial};
use super::super::manager_tests::launchd_over;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, task_spec};
use super::super::watch::test_hook;
use super::super::watch::watch_tests::{Held, pending};
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded;
use crate::backend::client::ClientError;
use crate::backend::client::fake::{Entry, Fake, exited, fail, reply};
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

/// A `print` of the job's launch `pid`, held by `debug --start-suspended`.
fn held(pid: u32) -> String {
    format!("\tstate = running\n\tpid = {pid}\n\tstarted suspended = 1\n")
}

/// A `print` of a run that was already in flight when the hold was armed: launched unheld.
fn in_flight(pid: u32) -> String {
    format!("\tstate = running\n\tpid = {pid}\n\tstarted suspended = 0\n")
}

/// A `print` of the job's process `pid` that does not say how it was launched.
fn without_hold_line(pid: u32) -> String {
    format!("\tstate = running\n\tpid = {pid}\n")
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

/// `kickstart -p`, which names the launch's pid.
fn kickstart(pid: u32) -> Entry {
    reply(&["kickstart", "-p", &target()], exited(0, &format!("{pid}\n"), ""))
}

fn sigcont() -> Entry {
    reply(&["kill", "SIGCONT", &target()], exited(0, "", ""))
}

/// A `SIGCONT` that lets `child` go, as the real one would: it runs to its end.
fn sigcont_ending(mut child: Held) -> Entry {
    sigcont().then(move || {
        child.finish();
    })
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

fn call(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

fn calls_of(fake: &Fake, verb: &str) -> Vec<Vec<String>> {
    fake.calls().into_iter().filter(|call| call[0] == verb).collect()
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

/// `begin_run` of `plist` over `fake`, drawing on a reservation made for [`Step::Start`], as `run` does.
fn begin_on(fake: &Fake, plist: Plist, handed: &mut Reservation) -> (Result<Run>, String) {
    let mut notes = Vec::new();
    let begun = begin_noting(fake, plist, handed, &mut notes);
    (begun, String::from_utf8(notes).expect("UTF-8 notes"))
}

fn begin(fake: &Fake, plist: Plist) -> (Result<Run>, String) {
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");
    begin_on(fake, plist, &mut handed)
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

/// The script of a launch that is held at once, whose process is `child`: the load, one attempt, the
/// confirming `print`, and the `SIGCONT`, which ends `child`. The pid is `child`'s.
fn held_at_once(child: Held) -> (Fake, u32) {
    let pid = child.pid();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(pid),
        print(&held(pid)),
        print(&held(pid)),
        sigcont_ending(child),
    ]);
    (fake, pid)
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

// The order of a run ==================================================================================================

/// `debug` arms the hold before `kickstart -p` starts the launch; the pid is watched before `SIGCONT` lets
/// it go: the child exits at the `SIGCONT`, and its exit is still seen, which only a watch registered
/// before it could.
#[skuld::test]
fn a_run_arms_debug_before_kickstart_and_resumes_after_registering_the_watch() {
    let (fake, _pid) = held_at_once(Held::new(3));
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    fake.assert_spent("the load, one attempt, the confirm and the resume");
    assert_eq!(
        fake.calls(),
        [
            call(&["print", &target()]),
            call(&["debug", &target(), "--start-suspended"]),
            call(&["kickstart", "-p", &target()]),
            call(&["print", &target()]),
            call(&["print", &target()]),
            call(&["kill", "SIGCONT", &target()]),
        ]
    );
    assert_eq!(run.wait().expect("the exit"), Exit::Code(3));
}

/// The log's length is read after the confirming `print` showed the launch held, and before `SIGCONT`:
/// the file grows at the pass's `print` and at the confirming one (both before the read), and at the
/// `SIGCONT` (after it). The run's offset is the length with the first two, not an earlier one, and not
/// the later one.
#[skuld::test]
fn the_log_offset_is_taken_while_the_launch_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    std::fs::write(&log, "older").expect("seed the log");
    let child = Held::new(0);
    let pid = child.pid();
    let mut child = Some(child);
    let after_resume = grow(&log, "0123456789");
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(pid),
        print(&held(pid)).then(grow(&log, "abc")),
        print(&held(pid)).then(grow(&log, "defg")),
        sigcont().then(move || {
            after_resume();
            child.take().expect("resumed once").finish();
        }),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, Some(&log)));

    let (path, offset, _) = log_of(&run);
    assert_eq!(path, log);
    assert_eq!(
        offset,
        ("older".len() + "abc".len() + "defg".len()) as u64,
        "the length while the launch was held"
    );
    assert_eq!(
        std::fs::metadata(&log).unwrap().len(),
        offset + 10,
        "the SIGCONT's write is after it"
    );
}

#[skuld::test]
fn a_held_launch_is_followed_and_its_exit_returned() {
    for (child, exit) in [(Held::new(3), Exit::Code(3)), (Held::signalled(15), Exit::Signal(15))] {
        let (fake, _pid) = held_at_once(child);
        let tree = Tree::new();

        let run = begun(&fake, snapshot(&tree, None));

        assert_eq!(run.wait().expect("the exit"), exit);
    }
}

/// A hold already pending answers 37, which is not an error: the launch `kickstart -p` starts consumes it.
#[skuld::test]
fn debug_exit_37_proceeds_and_consumes_the_pending_hold() {
    let child = Held::new(0);
    let pid = child.pid();
    let fake = Fake::new([
        print(IDLE),
        debug_exiting(37),
        kickstart(pid),
        print(&held(pid)),
        print(&held(pid)),
        sigcont_ending(child),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

    fake.assert_spent("the hold was pending, and the launch was held all the same");
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

#[skuld::test]
fn debug_tells_a_new_hold_from_a_pending_one() {
    let fake = Fake::new([debug(), debug_exiting(37)]);
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");

    let first = mgr.launchctl.debug_start_suspended(ID, &mut handed).expect("a hold");
    let second = mgr
        .launchctl
        .debug_start_suspended(ID, &mut handed)
        .expect("a pending hold");

    assert_eq!((first, second), (DebugOutcome::Held, DebugOutcome::AlreadyHeld));
}

// A scheduled run that slips in =======================================================================================

/// `kickstart -p` names the scheduled run already in flight and the hold stays pending: `run` says so
/// once, waits for that run, and starts again, arming `debug` again (37, as the first attempt's hold is
/// still pending). One kqueue serves both runs, and the offset is still the one taken while the second
/// launch was held.
#[skuld::test]
fn a_slipped_in_run_is_waited_for_then_a_fresh_attempt_re_arms_debug() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let (mut scheduled, ours) = (Held::new(0), Held::new(0));
    let (a, b) = (scheduled.pid(), ours.pid());
    let scheduled_run_writes = grow(&log, "scheduled run\n");
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        print(&in_flight(a)),
        // The scheduled run writes and ends after it was watched, as the confirming `print` is read.
        print(&in_flight(a)).then(move || {
            scheduled_run_writes();
            scheduled.finish();
        }),
        debug_exiting(37),
        kickstart(b),
        print(&held(b)),
        print(&held(b)),
        sigcont_ending(ours),
    ]);
    let tree = Tree::new();
    let _one_kqueue = test_hook::queues(1);

    let (result, notes) = begin(&fake, snapshot(&tree, Some(&log)));

    let run = result.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("two attempts");
    assert_eq!(notes, NOTE, "said once, before the wait");
    let verbs: Vec<String> = fake.calls().into_iter().map(|call| call[0].clone()).collect();
    assert_eq!(
        verbs,
        [
            "print",
            "debug",
            "kickstart",
            "print",
            "print",
            "debug",
            "kickstart",
            "print",
            "print",
            "kill"
        ]
    );
    let (_, offset, marker) = log_of(&run);
    assert_eq!(
        offset,
        "scheduled run\n".len() as u64,
        "the length while the second launch was held"
    );
    assert_eq!(
        marker,
        format!("goetia-run {b:08x}"),
        "the second launch is the one followed"
    );
    assert_eq!(run.wait().expect("the exit"), Exit::Code(0));
}

/// A scheduled run that has ended before it can be watched cannot be waited for: it is ended, and the
/// next attempt starts. The note is still said, once.
#[skuld::test]
fn a_slipped_in_run_that_ended_before_it_could_be_watched_retries() {
    let mut scheduled = Held::new(0);
    let a = scheduled.pid();
    scheduled.finish();
    let ours = Held::new(0);
    let b = ours.pid();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        print(&in_flight(a)),
        debug_exiting(37),
        kickstart(b),
        print(&held(b)),
        print(&held(b)),
        sigcont_ending(ours),
    ]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the second attempt followed the first");
    assert_eq!(notes, NOTE);
}

/// The pid `kickstart -p` named is not the job's any more by the time `print` is read: it ended, and the
/// next attempt starts. Nothing is signalled for it.
#[skuld::test]
fn an_attempt_whose_pid_is_gone_by_the_print_retries() {
    let ours = Held::new(0);
    let b = ours.pid();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(99_999),
        print(IDLE),
        debug(),
        kickstart(b),
        print(&held(b)),
        print(&held(b)),
        sigcont_ending(ours),
    ]);
    let tree = Tree::new();

    let (run, notes) = begin(&fake, snapshot(&tree, None));

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the second attempt followed the first");
    assert_eq!(notes, "", "an ended launch is not a slipped-in run");
    assert_eq!(calls_of(&fake, "kill").len(), 1, "only the held launch is resumed");
}

// Reservations ========================================================================================================

/// The first attempt draws on the reservation `run` made, and a later one on one of its own: the handed
/// one holds exactly the first attempt's calls, so a second attempt that drew on it would trip the
/// reservation's assertion.
#[skuld::test]
fn a_later_pass_makes_its_own_reservation_and_the_run_draws_no_spare_it_never_reserved() {
    let ours = Held::new(0);
    let b = ours.pid();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(99_999),
        print(IDLE),
        debug(),
        kickstart(b),
        print(&held(b)),
        print(&held(b)),
        sigcont_ending(ours),
    ]);
    let tree = Tree::new();
    // The load's read, `debug`, `kickstart -p` and `print`: four calls, two requests.
    let mut handed = Reservation::make(2, 4 * Role::COUNTED_FILES).expect("a reservation");

    let (run, _) = begin_on(&fake, snapshot(&tree, None), &mut handed);

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    assert_eq!(
        bounded::test_hook::left(&handed),
        (0, 0),
        "the first attempt spends what it was handed"
    );
}

/// A later attempt that cannot reserve is the stranded hold: the first attempt's `debug` left one pending.
#[skuld::test]
fn a_later_pass_that_cannot_reserve_returns_the_stranded_hold_error() {
    let fake = Fake::new([print(IDLE), debug(), kickstart(99_999), print(IDLE)]);
    let tree = Tree::new();
    let mut handed = Reservation::make(2, 4 * Role::COUNTED_FILES).expect("a reservation");
    let _short = bounded::test_hook::temp_files(0);

    let (run, _) = begin_on(&fake, snapshot(&tree, None), &mut handed);

    let e = run.err().expect("the second attempt cannot reserve");
    assert_stranded(&e, "a reservation that failed");
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::Reservation { .. })),
        "{e:?}"
    );
    fake.assert_spent("the first attempt went out before the reservation failed");
}

/// The longest first attempt: the load that is refused and reads again (three calls, one request), then
/// `debug`, `kickstart -p`, `print`, the confirming `print` and `SIGCONT`. It stays within what `run`
/// reserves for a start, and uses 8 calls and 4 requests of it.
#[skuld::test]
fn begin_run_draws_no_more_than_a_start_reserves() {
    let child = Held::new(0);
    let pid = child.pid();
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
        kickstart(pid),
        print(&held(pid)),
        print(&held(pid)),
        sigcont_ending(child),
    ]);
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");

    let (run, _) = begin_on(&fake, plist, &mut handed);

    run.unwrap_or_else(|e| panic!("begin_run failed: {e}"));
    fake.assert_spent("the longest first attempt");
    assert_eq!(
        bounded::test_hook::left(&handed),
        (1, 5 * Role::COUNTED_FILES),
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
    let child = Held::new(0);
    let (fake, pid) = held_at_once(child);
    let tree = Tree::new();

    let with_logs = begun(&fake, snapshot(&tree, Some(&log)));

    let (path, _, line) = log_of(&with_logs);
    assert_eq!(path, log);
    assert_eq!(line, format!("goetia-run {pid:08x}"));
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

    let (fake, _) = held_at_once(Held::new(0));
    let without = begun(&fake, snapshot(&Tree::new(), None));
    assert!(matches!(without.output(), OutputSource::None));
}

// What strands a hold, and what does not ==============================================================================

/// A pid that is gone by the time it can be watched was ended: the run is unrecorded, nothing is
/// resumed, and no hold is stranded.
#[skuld::test]
fn a_pid_that_cannot_be_added_is_run_unrecorded_with_the_reason() {
    let mut gone = Held::new(0);
    let pid = gone.pid();
    gone.finish();
    let fake = Fake::new([print(IDLE), debug(), kickstart(pid), print(&held(pid))]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(
        matches!(&e, Error::RunUnrecorded { reason, .. }
            if reason == "it was ended before goetia could watch it (a `stop`, a `kill`, or the system)"),
        "{e:?}"
    );
    fake.assert_spent("the attempt, and nothing after the failed add");
    assert!(calls_of(&fake, "kill").is_empty(), "nothing is resumed");
}

/// `print` is read again after the add, and a pid it no longer shows is never resumed.
#[skuld::test]
fn a_print_that_stops_showing_the_pid_after_the_add_sends_no_sigcont() {
    let child = Held::new(0);
    let pid = child.pid();
    let fake = Fake::new([print(IDLE), debug(), kickstart(pid), print(&held(pid)), print(IDLE)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(
        matches!(&e, Error::RunUnrecorded { reason, .. }
            if reason == "it was ended before goetia could watch it (a `stop`, a `kill`, or the system)"),
        "{e:?}"
    );
    fake.assert_spent("the confirming print was the last call");
    assert!(calls_of(&fake, "kill").is_empty());
}

#[skuld::test]
fn a_sigcont_that_reports_no_process_is_run_unrecorded_with_the_reason() {
    let child = Held::new(0);
    let pid = child.pid();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(pid),
        print(&held(pid)),
        print(&held(pid)),
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

/// A `debug` that failed held nothing: its own error, with no mention of a hold.
#[skuld::test]
fn a_debug_failure_other_than_37_is_its_error_and_nothing_was_held() {
    let fake = Fake::new([print(IDLE), debug_exiting(5)]);
    let tree = Tree::new();

    let e = failed(&fake, snapshot(&tree, None));

    assert!(matches!(&e, Error::Command { .. }), "{e:?}");
    fake.assert_spent("nothing was sent after the failed debug");
}

/// A launch `print` shows with no word of how it began cannot be told from the held one or a run in flight,
/// and a read that failed cannot be told from a launch that ended: neither starts another run.
#[skuld::test]
fn a_print_that_cannot_say_which_launch_it_shows_is_an_error_not_another_attempt() {
    let child = Held::new(0);
    let pid = child.pid();
    let bare = format!("\tstate = running\n\tpid = {pid}\n");
    let failed_print = reply(
        &["print", &target()],
        exited(5, "", "Could not print: 5: Input/output error\n"),
    );
    for (what, shown) in [
        ("a pid with no `started suspended` line", print(&bare)),
        ("a failed print", failed_print),
    ] {
        let fake = Fake::new([print(IDLE), debug(), kickstart(pid), shown]);
        let tree = Tree::new();

        let e = failed(&fake, snapshot(&tree, None));

        assert_stranded(&e, what);
        fake.assert_spent(what);
    }
}

/// The confirming `print` before the `SIGCONT` shows the pid alive but not as the held launch: nothing was
/// resumed, and the launch may sit stopped, so this is the stranded hold, not a run that ended.
#[skuld::test]
fn a_confirming_print_that_shows_the_pid_without_the_hold_line_is_the_stranded_hold() {
    for (what, unheld) in [("no `started suspended` line", false), ("shown as unheld", true)] {
        let child = Held::new(0);
        let pid = child.pid();
        let shown = if unheld { in_flight(pid) } else { without_hold_line(pid) };
        let fake = Fake::new([print(IDLE), debug(), kickstart(pid), print(&held(pid)), print(&shown)]);
        let tree = Tree::new();

        let e = failed(&fake, snapshot(&tree, None));

        assert_stranded(&e, what);
        assert!(e.is_indeterminate(), "{what}: the launch may be running or held: {e:?}");
        fake.assert_spent(what);
        assert!(calls_of(&fake, "kill").is_empty(), "{what}: nothing is resumed");
    }
}

/// After a scheduled run that slipped in: a confirm that shows the pid held or unreadable is the stranded
/// hold, and one that shows it gone is the next attempt.
#[skuld::test]
fn a_slipped_in_run_whose_confirming_print_is_not_what_it_was_is_an_error() {
    for (what, as_held) in [("shown as held", true), ("shown with no hold line", false)] {
        let scheduled = Held::new(0);
        let a = scheduled.pid();
        let shown = if as_held { held(a) } else { without_hold_line(a) };
        let fake = Fake::new([print(IDLE), debug(), kickstart(a), print(&in_flight(a)), print(&shown)]);
        let tree = Tree::new();

        let (result, notes) = begin(&fake, snapshot(&tree, None));

        let e = result.err().expect(what);
        assert_stranded(&e, what);
        fake.assert_spent(what);
        assert_eq!(notes, NOTE, "{what}");
    }
}

/// A note that cannot be written is an error after `debug` held the launch: the stranded hold, with the
/// write's failure under it, and the pid that was watched is not waited for.
#[skuld::test]
fn a_note_that_cannot_be_written_is_the_stranded_hold() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the output is closed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let scheduled = Held::new(0);
    let a = scheduled.pid();
    let fake = Fake::new([print(IDLE), debug(), kickstart(a), print(&in_flight(a))]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut handed = mgr.reserve(&[Step::Start], Budget::Immediate).expect("reserve");

    let e = begin_noting(&fake, snapshot(&tree, None), &mut handed, &mut Closed)
        .err()
        .expect("the note cannot be written");

    assert_stranded(&e, "a note that failed");
    assert!(
        matches!(&e, Error::Step { source, .. } if matches!(**source, Error::Os { .. })),
        "{e:?}"
    );
    fake.assert_spent("nothing was sent after the failed note");
}

/// A `SIGCONT` that may have been sent is in doubt: the run may have started and be going unfollowed, and
/// the error says so, still naming what ends the hold.
#[skuld::test]
fn a_sigcont_that_ends_in_doubt_says_the_run_may_have_started() {
    let child = Held::new(0);
    let pid = child.pid();
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(pid),
        print(&held(pid)),
        print(&held(pid)),
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

// Registrations =======================================================================================================

/// A scheduled run that was registered and then shown ended (the confirming `print`) leaves no event on
/// the queue when the next attempt begins: the exit it queued is deleted with its registration, so the
/// followed launch cannot be handed it.
#[skuld::test]
fn a_registration_left_by_an_ended_slipped_in_run_is_deleted_before_the_next_attempt() {
    let (mut scheduled, mut ours) = (Held::new(0), Held::new(7));
    let (a, b) = (scheduled.pid(), ours.pid());
    let fake = Fake::new([
        print(IDLE),
        debug(),
        kickstart(a),
        print(&in_flight(a)),
        // The scheduled run ends after it was registered, and the confirming `print` no longer shows it.
        print(IDLE).then(move || {
            scheduled.finish();
        }),
        debug_exiting(37),
        kickstart(b),
        print(&held(b)),
        print(&held(b)),
        sigcont(),
    ]);
    let tree = Tree::new();

    let run = begun(&fake, snapshot(&tree, None));

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

// Interrupts ==========================================================================================================

/// SIGINT, SIGTERM and SIGHUP are held from before the first `debug` to after the `SIGCONT`, and
/// re-raised once `begin_run` returns: the window is `begin_run` itself.
#[skuld::test]
fn an_interrupt_during_begin_run_is_held_until_it_returns() {
    let stand_in = StandIn::install();
    let child = Held::new(0);
    let pid = child.pid();
    let mut child = Some(child);
    let fake = Fake::new([
        print(IDLE),
        debug().then(|| {
            // SAFETY: `raise` takes a valid signal number, and the stand-in is the handler it falls back to.
            assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        }),
        kickstart(pid),
        print(&held(pid)),
        print(&held(pid)),
        sigcont().then(move || {
            assert_eq!(seen(), 0, "still held at the resume");
            child.take().expect("resumed once").finish();
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
