//! A task's run through the journal: `begin_run` finding the run its start produced, `wait` finding
//! its exit, and `JournalOutput::stream` following its output by the stream of the run's marker. Driven through a fake `systemctl` and a
//! scripted journal; none of it needs systemd.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::super::journal::fake::{Fake as JournalFake, Log};
use super::super::journal::{Cursor, Journalctl};
use super::super::journal::{DEACTIVATED, Entry, Journal, MAIN_PROCESS_EXITED, STARTING};
use super::super::systemctl::systemctl_tests::gate_then;
use super::super::task::task_tests::{enqueued, fragment, manager, task_spec};
use super::*;
use crate::backend::Backend;
use crate::backend::bounded::test_hook as files;
use crate::backend::client::fake::{Entry as Reply, Fake, exited, reply};
use crate::backend::standby::test_hook as threads;
use crate::manager::output::output_tests::Streaming;
use crate::manager::{Budget, Exit, Step};
use crate::spec::{DaemonFields, DaemonSpec, TaskFields, TaskSpec};
use crate::testing::base_fields;

// Entries =============================================================================================================

/// "Starting x.service...": the run of job `job` began.
fn starting(job: u32, invocation: &str, cursor: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", STARTING),
        ("JOB_ID", &job.to_string()),
        ("INVOCATION_ID", invocation),
        ("__CURSOR", cursor),
    ])
}

/// "Failed to start x.service": the end of job `job`, which carries the `INVOCATION_ID` of the last run
/// that did start when no run did.
fn job_ended(job: u32, invocation: &str, result: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", "be02cf6855d2428ba40df7e9d022f03d"),
        ("JOB_ID", &job.to_string()),
        ("JOB_RESULT", result),
        ("INVOCATION_ID", invocation),
        ("MESSAGE", "x.service: Failed to start."),
    ])
}

fn main_exited(invocation: &str, code: &str, status: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", MAIN_PROCESS_EXITED),
        ("INVOCATION_ID", invocation),
        ("EXIT_CODE", code),
        ("EXIT_STATUS", status),
    ])
}

fn deactivated(invocation: &str) -> Entry {
    Entry::of(&[("MESSAGE_ID", DEACTIVATED), ("INVOCATION_ID", invocation)])
}

/// One line a run wrote on stdout, as part of stream `stream`. It has no `_SYSTEMD_INVOCATION_ID`: a
/// fast run's lines can lack it.
fn line(cursor: &str, stream: &str, text: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE", text),
        ("__CURSOR", cursor),
        ("_TRANSPORT", "stdout"),
        ("_STREAM_ID", stream),
    ])
}

/// The first line of the run of invocation `inv`, which its `sh` wrapper prints: the first of its stream.
fn marker(cursor: &str, stream: &str, inv: &str) -> Entry {
    line(cursor, stream, &format!("goetia-run {inv}"))
}

// Begin ===============================================================================================================

fn held(mgr: &Systemd) -> Held {
    Backend::reserve(mgr, &[Step::Start], Budget::Immediate).expect("reserve")
}

/// `begin_run` of the task `x` over `journal`, whose start `client` answers with job 7.
fn begin(journal: &JournalFake, client: &Fake) -> Result<Run> {
    let mgr = manager(client, Fake::new([]), journal.clone());
    let mut held = held(&mgr);
    Backend::begin_run(&mgr, fragment(&task_spec("x", "/bin/a", None)), &mut held)
}

fn start() -> Reply {
    enqueued("start", 7)
}

/// The journal of a run of job 7 that began as invocation `inv` at cursor `c1`, then follows `exit` for
/// the wait.
fn running(exit: Vec<Entry>) -> JournalFake {
    JournalFake::new(Vec::new())
        .follows(vec![starting(7, "inv", "c1")])
        .follows(exit)
}

fn begun(journal: &JournalFake) -> Run {
    begin(journal, &gate_then([start()])).expect("begin_run")
}

/// What the output follows: the unit's own lines.
fn stdout_query(unit: &str) -> Vec<String> {
    [format!("_SYSTEMD_UNIT={unit}"), "_TRANSPORT=stdout".to_string()].to_vec()
}

fn call(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// A sync before the request has no reason left: the run's lines are found by its marker's stream,
/// wherever they land.
#[skuld::test]
fn begin_run_sends_the_start_and_then_follows_its_job_with_no_sync() {
    let log = Log::default();
    let journal = JournalFake::new(Vec::new())
        .logging(&log)
        .follows(vec![starting(7, "inv", "c1")]);
    let sent = Arc::clone(&log);
    let client = gate_then([start().then(move || sent.lock().unwrap().push("start".to_string()))]);

    begin(&journal, &client).expect("begin_run");

    assert_eq!(
        *log.lock().unwrap(),
        ["start", "follow UNIT=x.service JOB_ID=7"],
        "the request, then the first follow"
    );
    assert_eq!(journal.syncs(), 0);
}

#[skuld::test]
fn begin_run_parses_the_job_and_follows_its_starting_entry() {
    let journal = JournalFake::new(Vec::new()).follows(vec![starting(7, "inv-1", "c1")]);

    let run = begin(&journal, &gate_then([start()])).expect("begin_run");

    assert_eq!(
        journal.followed(),
        [(call(&["UNIT=x.service", "JOB_ID=7"]), None)],
        "the job the request announced, from the start of the boot"
    );
    assert_eq!(run.invocation, "inv-1");
    assert_eq!(run.start, Cursor::of("c1"));
    let OutputSource::Journal(output) = run.output() else {
        panic!("a task with no `logs` shows its output from the journal");
    };
    assert_eq!(output.unit, "x.service");
    assert_eq!(output.invocation, "inv-1");
    assert_eq!(output.cursor, Cursor::of("c1"));
}

/// A start that joined a run in flight finds that run's Starting entry, which was written before
/// the request and is in the follow's history all the same; entries of the job that decide nothing are
/// passed.
#[skuld::test]
fn a_start_that_joined_an_in_flight_job_follows_that_runs_invocation() {
    let old = Entry::of(&[
        ("MESSAGE_ID", STARTING),
        ("JOB_ID", "7"),
        ("INVOCATION_ID", "inv-in-flight"),
        ("__CURSOR", "c0"),
        ("__REALTIME_TIMESTAMP", "1"),
    ]);
    let journal = JournalFake::new(Vec::new()).follows(vec![
        Entry::of(&[("JOB_ID", "7"), ("MESSAGE", "a line about the job that is neither")]),
        old,
    ]);

    let run = begin(&journal, &gate_then([start()])).expect("begin_run");

    assert_eq!(run.invocation, "inv-in-flight");
    assert_eq!(run.start, Cursor::of("c0"));
}

#[skuld::test]
fn a_job_end_before_a_starting_entry_is_run_not_started_and_never_takes_the_previous_invocation() {
    // The refused start's entries carry the invocation of the last run that did start.
    let journal = JournalFake::new(Vec::new()).follows(vec![job_ended(7, "the-previous-run", "failed")]);

    let e = begin(&journal, &gate_then([start()])).err().expect("no run began");

    match &e {
        Error::RunNotStarted { id, reason } => {
            assert_eq!(id, "x");
            assert!(reason.contains("failed"), "{reason}");
            assert!(!reason.contains("the-previous-run"), "{reason}");
        }
        other => panic!("expected RunNotStarted: {other:?}"),
    }
}

#[skuld::test]
fn a_start_refused_by_systemctl_is_its_error() {
    let journal = JournalFake::new(Vec::new());
    let client = gate_then([reply(
        &["start"],
        exited(1, "", "Failed to start x.service: Unit x.service not found.\n"),
    )]);

    let e = begin(&journal, &client).err().expect("systemctl refused");

    assert!(matches!(e, Error::Command { .. }), "{e:?}");
    assert!(
        journal.followed().is_empty(),
        "no start was sent, so there is no run to follow"
    );
}

#[skuld::test]
fn a_journal_failure_after_the_request_is_in_doubt_and_reached() {
    for (what, journal) in [
        ("it cannot be read", JournalFake::unreadable()),
        (
            "it ends before the run's start is found",
            JournalFake::new(Vec::new()).follows_then_ends(vec![]),
        ),
        (
            "the start has no invocation",
            JournalFake::new(Vec::new()).follows(vec![Entry::of(&[
                ("MESSAGE_ID", STARTING),
                ("JOB_ID", "7"),
                ("__CURSOR", "c1"),
            ])]),
        ),
        (
            "the start has no cursor",
            JournalFake::new(Vec::new()).follows(vec![Entry::of(&[
                ("MESSAGE_ID", STARTING),
                ("JOB_ID", "7"),
                ("INVOCATION_ID", "inv"),
            ])]),
        ),
    ] {
        let e = begin(&journal, &gate_then([start()])).err().expect(what);

        assert!(
            matches!(e, Error::RequestInDoubt { reached: true, .. }),
            "{what}: the request was sent, so the run may have happened: {e:?}"
        );
    }
}

/// Everything the run needs once the request is sent is made before it, so a thread or a file that
/// cannot be made sends nothing, and exactly what a run counts for is enough.
#[skuld::test]
fn a_reservation_that_cannot_be_made_sends_nothing() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let log = dir.path().join("job.log");
    // A run with its output in the journal needs a file each for the follower that finds its start, the
    // one that waits for its exit and the output's two, and the output's thread. One with a `logs` file
    // needs the first two.
    for (what, spec, files_allowed, threads_allowed) in [
        ("no file", task_spec("x", "/bin/a", None), 0, 9),
        ("three of four files", task_spec("x", "/bin/a", None), 3, 9),
        ("no thread", task_spec("x", "/bin/a", None), 9, 0),
        ("one of two files, with logs", logged_task(&log), 1, 9),
    ] {
        let _files = files::temp_files(files_allowed);
        let _threads = threads::threads(threads_allowed);
        let journal = JournalFake::new(Vec::new());
        let client = gate_then([]);
        let mgr = manager(&client, Fake::new([]), journal.clone());
        let mut held = held(&mgr);

        let e = Backend::begin_run(&mgr, fragment(&spec), &mut held).err().expect(what);

        assert!(matches!(e, Error::Reservation { id: Some(_), .. }), "{what}: {e:?}");
        assert!(client.calls().is_empty(), "{what}: {:?}", client.calls());
        assert_eq!(journal.syncs(), 0, "{what}: nothing was run");
    }

    for (what, spec, files_allowed, threads_allowed) in [
        ("journal output", task_spec("x", "/bin/a", None), 4, 1),
        ("logs", logged_task(&log), 2, 0),
    ] {
        let _files = files::temp_files(files_allowed);
        let _threads = threads::threads(threads_allowed);
        let journal = JournalFake::new(Vec::new()).follows(vec![starting(7, "inv", "c1")]);
        let mgr = manager(&gate_then([start()]), Fake::new([]), journal);
        let mut held = held(&mgr);

        Backend::begin_run(&mgr, fragment(&spec), &mut held).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    }
}

fn logged_task(path: &Path) -> DaemonSpec {
    let common = DaemonFields {
        logs: Some(path.to_path_buf()),
        ..base_fields("x", vec!["/bin/a".to_string()]).common
    };
    TaskSpec::new(TaskFields { common, schedule: None })
        .expect("a task with logs")
        .into()
}

fn log_source(run: &Run) -> (std::path::PathBuf, u64) {
    match run.output() {
        OutputSource::LogFile { path, offset, marker } => {
            let marker = marker.expect("a systemd task's runs write a marker to the file too");
            assert_eq!(
                (marker.prefix, marker.line.as_str(), marker.id_len),
                ("goetia-run", "goetia-run inv", 32)
            );
            (path, offset)
        }
        _ => panic!("a task with `logs` shows its log file"),
    }
}

/// The offset is the file's length before the request: a run that is quicker than `begin_run`'s
/// return has already written after it.
#[skuld::test]
fn the_log_offset_is_measured_before_the_request() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("job.log");
    std::fs::write(&path, "an earlier run\n").expect("the log");
    let earlier = std::fs::metadata(&path).expect("the log").len();
    let writes = path.clone();
    let client = gate_then([start().then(move || {
        let mut file = std::fs::OpenOptions::new().append(true).open(&writes).expect("the log");
        file.write_all(b"this run\n").expect("a line");
    })]);
    let journal = JournalFake::new(Vec::new()).follows(vec![starting(7, "inv", "c1")]);
    let mgr = manager(&client, Fake::new([]), journal.clone());
    let mut held = held(&mgr);

    let run = Backend::begin_run(&mgr, fragment(&logged_task(&path)), &mut held).expect("begin_run");

    assert_eq!(log_source(&run), (path.clone(), earlier));
    assert!(std::fs::metadata(&path).unwrap().len() > earlier, "the request wrote");
}

#[skuld::test]
fn a_log_file_that_is_not_there_is_offset_zero() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("not-yet.log");
    let journal = JournalFake::new(Vec::new()).follows(vec![starting(7, "inv", "c1")]);
    let client = gate_then([start()]);
    let mgr = manager(&client, Fake::new([]), journal);
    let mut held = held(&mgr);

    let run = Backend::begin_run(&mgr, fragment(&logged_task(&path)), &mut held).expect("begin_run");

    assert_eq!(log_source(&run), (path, 0));
}

#[skuld::test]
fn a_log_whose_length_cannot_be_read_fails_before_anything_is_sent() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    // A path through a regular file: its metadata is `ENOTDIR`, not "not found".
    let file = dir.path().join("file");
    std::fs::write(&file, "").expect("a file");
    let client = gate_then([]);
    let mgr = manager(&client, Fake::new([]), JournalFake::new(Vec::new()));
    let mut held = held(&mgr);

    let e = Backend::begin_run(&mgr, fragment(&logged_task(&file.join("job.log"))), &mut held)
        .err()
        .expect("the length cannot be read");

    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    assert!(client.calls().is_empty(), "{:?}", client.calls());
}

// wait ================================================================================================================

#[skuld::test]
fn wait_decodes_each_exit() {
    for (what, entries, expected) in [
        ("Deactivated successfully", vec![deactivated("inv")], Ok(Exit::Code(0))),
        ("exited/3", vec![main_exited("inv", "exited", "3")], Ok(Exit::Code(3))),
        (
            "exited/203",
            vec![main_exited("inv", "exited", "203")],
            Ok(Exit::Code(203)),
        ),
        (
            "killed/15, as a stop ends it",
            vec![main_exited("inv", "killed", "15")],
            Ok(Exit::Signal(15)),
        ),
        ("killed/9", vec![main_exited("inv", "killed", "9")], Ok(Exit::Signal(9))),
        (
            "dumped/11 is the signal",
            vec![main_exited("inv", "dumped", "11")],
            Ok(Exit::Signal(11)),
        ),
        (
            "an entry of another invocation is ignored",
            vec![
                main_exited("another", "exited", "9"),
                deactivated("yet-another"),
                main_exited("inv", "exited", "3"),
            ],
            Ok(Exit::Code(3)),
        ),
        (
            "entries that are neither are passed",
            vec![
                Entry::of(&[("MESSAGE", "something"), ("INVOCATION_ID", "inv")]),
                deactivated("inv"),
            ],
            Ok(Exit::Code(0)),
        ),
        ("an unknown EXIT_CODE", vec![main_exited("inv", "weird", "3")], Err(())),
        ("no status", vec![main_exited("inv", "exited", "x")], Err(())),
    ] {
        let journal = running(entries);
        let run = begun(&journal);

        let exit = run.wait();

        match (exit, expected) {
            (Ok(exit), Ok(expected)) => assert_eq!(exit, expected, "{what}"),
            (Err(Error::RunUnrecorded { id, .. }), Err(())) => assert_eq!(id, "x", "{what}"),
            (other, expected) => panic!("{what}: {other:?}, expected {expected:?}"),
        }
    }
}

#[skuld::test]
fn wait_follows_the_unit_after_the_starting_entry() {
    let journal = running(vec![deactivated("inv")]);
    let run = begun(&journal);

    run.wait().expect("the exit");

    assert_eq!(
        journal.followed()[1],
        (call(&["UNIT=x.service"]), Some("c1".to_string())),
        "the manager's entries about the unit, after the run's Starting entry"
    );
}

#[skuld::test]
fn a_wait_whose_journal_ends_first_is_run_unrecorded() {
    let journal = JournalFake::new(Vec::new())
        .follows(vec![starting(7, "inv", "c1")])
        .follows_then_ends(vec![main_exited("another", "exited", "1")]);

    let e = begun(&journal).wait().expect_err("no exit was found");

    assert!(matches!(e, Error::RunUnrecorded { .. }), "{e:?}");
}

#[skuld::test]
fn a_wait_whose_journal_cannot_be_followed_is_run_unrecorded() {
    let run = begun(&running(Vec::new()));
    // The journal is gone by the time the wait asks.
    let run = Run {
        journal: Arc::new(JournalFake::unreadable()),
        ..run
    };

    let e = run.wait().expect_err("the journal cannot be followed");

    assert!(matches!(e, Error::RunUnrecorded { .. }), "{e:?}");
    assert!(e.report().to_string().contains("no journal"), "{e}");
}

// stream ==============================================================================================================

/// The output of the run of invocation `inv` that began at cursor `c1`, over `journal`.
fn output_over(journal: Arc<dyn Journal>) -> JournalOutput {
    JournalOutput {
        unit: "x.service".to_string(),
        invocation: "inv".to_string(),
        cursor: Cursor::of("c1"),
        journal,
        // What `begin_run` made: a file for the follower and one for the read after it.
        draws: Reservation::make(0, 2).expect("temp files"),
        watcher: Standby::new("goetia run output", watch).expect("a thread"),
    }
}

fn shown_after_the_end(journal: &JournalFake) -> (Result<()>, Vec<u8>) {
    shown_after_an_end(journal, true)
}

/// What the stream shows once the run has ended, and whether the wait recorded its exit.
fn shown_after_an_end(journal: &JournalFake, exit_recorded: bool) -> (Result<()>, Vec<u8>) {
    let (end, ended) = crossbeam_channel::bounded::<()>(0);
    drop(end);
    let mut out = Vec::new();
    let result = output_over(Arc::new(journal.clone())).stream(&mut out, &ended, &AtomicBool::new(exit_recorded));
    (result, out)
}

/// The run's output is the lines of the marker's stream, the marker not among them, and another
/// stream's lines are never shown, whether they come first or between the run's own.
/// The manager's entries (a run's Starting entry, its exit) are read from the system journal alone.
#[skuld::test]
fn the_starting_entry_and_the_exit_are_read_in_the_manager_scope() {
    let journal = running(vec![deactivated("inv")]);

    begun(&journal).wait().expect("the exit");

    assert_eq!(
        journal.scopes(),
        [("follow", Scope::Manager), ("follow", Scope::Manager)],
        "the Starting entry, then the exit"
    );
}

/// A run's output (the follow for the marker, the final read after the end) is read from every journal
/// the reader may open, since a unit that runs as a regular uid writes it to a user journal.
#[skuld::test]
fn the_output_is_read_in_the_output_scope() {
    let journal = JournalFake::new(Vec::new())
        .follows(vec![marker("c2", "s1", "inv")])
        .then_reads(vec![line("c3", "s1", "last")]);

    let (result, _) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(
        journal.scopes(),
        [("follow", Scope::Output), ("read_after", Scope::Output)]
    );
}

#[skuld::test]
fn output_is_the_lines_of_the_markers_stream_minus_the_marker() {
    let journal = JournalFake::new(Vec::new())
        .follows(vec![
            line("c2", "s9", "a run that came first"),
            marker("c3", "s1", "inv"),
            line("c4", "s9", "a run in between"),
            line("c5", "s1", "first"),
            line("c6", "s1", "second"),
        ])
        .then_reads(vec![line("c7", "s1", "third")]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(String::from_utf8(out).unwrap(), "first\nsecond\nthird\n");
    assert_eq!(
        journal.followed(),
        [(stdout_query("x.service"), Some("c1".to_string()))],
        "the unit's own lines after the run's Starting entry"
    );
    assert_eq!(
        journal.read_after_calls(),
        [(call(&["_STREAM_ID=s1"]), "c6".to_string())],
        "what is left is read from the stream, after the stream's last line shown"
    );
}

/// After the run's end the rest is read by the stream, so an entry of another stream is not shown.
#[skuld::test]
fn the_rest_of_the_stream_shows_only_the_stream() {
    let journal = JournalFake::new(Vec::new())
        .follows(vec![marker("c2", "s1", "inv")])
        .then_reads(vec![
            line("c3", "s1", "mine"),
            line("c4", "s2", "the next run's"),
            line("c5", "s1", "also mine"),
        ]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(String::from_utf8(out).unwrap(), "mine\nalso mine\n");
}

/// The marker is the first line of its stream only: the same text again is the program's.
#[skuld::test]
fn only_the_first_line_with_the_markers_text_is_the_marker() {
    let journal = JournalFake::new(Vec::new()).follows(vec![
        marker("c2", "s1", "inv"),
        marker("c3", "s1", "inv"),
        line("c4", "s9", "goetia-run inv"),
    ]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(String::from_utf8(out).unwrap(), "goetia-run inv\n");
}

/// A run that printed nothing has its stream all the same: the marker alone, and no note.
#[skuld::test]
fn a_run_that_printed_nothing_shows_nothing_and_no_note() {
    let journal = JournalFake::new(Vec::new()).follows(vec![marker("c2", "s1", "inv")]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(out, b"");
}

/// A marker the follower had not delivered when the run ended is found by the final read of the
/// unit's lines after the run's Starting entry, and the run's lines are the stream's from there.
#[skuld::test]
fn a_marker_the_follower_did_not_deliver_is_found_by_the_final_read() {
    let log = Log::default();
    let journal = JournalFake::new(Vec::new())
        .logging(&log)
        .follows(vec![line("c2", "s9", "another run's")])
        .then_reads(vec![
            line("c2", "s9", "another run's"),
            marker("c3", "s1", "inv"),
            line("c4", "s1", "a"),
            line("c5", "s9", "another run's, between"),
            line("c6", "s1", "b"),
        ]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(String::from_utf8(out).unwrap(), "a\nb\n");
    assert_eq!(
        journal.read_after_calls(),
        [(stdout_query("x.service"), "c1".to_string())],
        "with no marker found, the unit's lines after the Starting entry"
    );
    assert_eq!(
        log.lock().unwrap().iter().map(String::as_str).collect::<Vec<_>>(),
        [
            format!("follow {}", stdout_query("x.service").join(" ")).as_str(),
            "sync",
            "read_after"
        ],
        "the follower, then the sync, then the read"
    );
}

/// No marker after the terminal entry, the sync and a final read: the run's lines cannot be told from
/// another's, so none is shown, and the note says what to do about the limit that dropped it.
#[skuld::test]
fn no_marker_after_the_end_is_a_note_and_nothing_else() {
    let journal = JournalFake::new(Vec::new())
        .follows(vec![
            line("c2", "s9", "a line of another run"),
            marker("c3", "s8", "an-earlier-invocation"),
        ])
        .then_reads(vec![line("c4", "s9", "and another")]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("a missing marker is a note, not a failure");
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("note: "), "{out}");
    assert!(out.ends_with('\n') && out.lines().count() == 1, "{out:?}");
    for named in [
        "rate limit",
        "RateLimitBurst=0",
        "[Journal]",
        "journald.conf.d",
        "systemd-journald",
    ] {
        assert!(out.contains(named), "the note names `{named}`: {out}");
    }
    assert!(!out.contains("another"), "no line of another stream is shown: {out}");
    assert_eq!(
        journal.syncs(),
        1,
        "the sync came first, so the marker was not merely late"
    );
    assert_eq!(journal.read_after_calls().len(), 1);
}

/// The wait found no exit: the run's own error says what is wrong, and no note about a missing marker
/// is written beside it.
#[skuld::test]
fn no_marker_is_no_note_when_the_waits_exit_is_not_recorded() {
    let journal = JournalFake::new(Vec::new()).follows(vec![line("c2", "s9", "a line of another run")]);

    let (result, out) = shown_after_an_end(&journal, false);

    result.expect("the stream");
    assert_eq!(out, b"");
    assert_eq!(journal.syncs(), 1, "what was there is still read");
}

/// A marker the journal stores without its stream cannot say which lines are the run's.
#[skuld::test]
fn a_marker_with_no_stream_is_an_incomplete_answer() {
    let bare = Entry::of(&[
        ("MESSAGE", "goetia-run inv"),
        ("__CURSOR", "c2"),
        ("_TRANSPORT", "stdout"),
    ]);
    let journal = JournalFake::new(Vec::new()).follows(vec![bare]);

    let (result, out) = shown_after_the_end(&journal);

    let e = result.expect_err("no stream id");
    assert!(matches!(e, Error::IncompleteAnswer { .. }), "{e:?}");
    assert!(out.is_empty());
}

#[skuld::test]
fn the_rest_is_read_after_sync_and_written_once() {
    let log = Log::default();
    let journal = JournalFake::new(Vec::new())
        .logging(&log)
        .follows(vec![marker("c2", "s1", "inv"), line("c3", "s1", "live")])
        .then_reads(vec![line("c4", "s1", "late one"), line("c5", "s1", "late two")]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    assert_eq!(String::from_utf8(out).unwrap(), "live\nlate one\nlate two\n");
    assert_eq!(
        log.lock().unwrap().iter().map(String::as_str).collect::<Vec<_>>(),
        [
            format!("follow {}", stdout_query("x.service").join(" ")).as_str(),
            "sync",
            "read_after"
        ],
        "the follower, then the sync, then the read"
    );
    assert_eq!(journal.stops(), 1, "the follower was stopped when the run ended");
}

/// A line is written as the bytes the journal holds, a control character and a byte that is not
/// UTF-8 included, and an entry with no `MESSAGE` writes nothing.
#[skuld::test]
fn a_line_is_written_as_the_bytes_the_journal_holds() {
    let bytes = Entry::parse(br#"{"MESSAGE":[27,91,49,109,255],"__CURSOR":"c3","_STREAM_ID":"s1"}"#).expect("an entry");
    let none = Entry::of(&[("__CURSOR", "c4"), ("_STREAM_ID", "s1")]);
    let long = Entry::parse(
        format!(
            r#"{{"MESSAGE":"{}","__CURSOR":"c5","_STREAM_ID":"s1"}}"#,
            "x".repeat(4088)
        )
        .as_bytes(),
    )
    .expect("an entry");
    let journal = JournalFake::new(Vec::new()).follows(vec![marker("c2", "s1", "inv"), bytes, none, long]);

    let (result, out) = shown_after_the_end(&journal);

    result.expect("the stream");
    let mut expected = vec![27, 91, 49, 109, 255, b'\n'];
    expected.extend(std::iter::repeat_n(b'x', 4088));
    expected.push(b'\n');
    assert_eq!(out, expected);
}

#[skuld::test]
fn a_follower_that_cannot_start_is_the_streams_error() {
    let (result, out) = shown_after_the_end(&JournalFake::unreadable());

    let e = result.expect_err("no journal");
    assert!(e.report().to_string().contains("no journal"), "{e}");
    assert!(out.is_empty());
}

#[skuld::test]
fn a_sync_or_a_read_that_fails_ends_the_stream_with_its_error_after_what_was_shown() {
    let journal = JournalFake::new(Vec::new())
        .sync_fails()
        .follows(vec![marker("c2", "s1", "inv"), line("c3", "s1", "live")]);

    let (result, out) = shown_after_the_end(&journal);

    let e = result.expect_err("the sync failed");
    assert!(e.report().to_string().contains("it failed"), "{e}");
    assert_eq!(String::from_utf8(out).unwrap(), "live\n");
}

/// The follower is stopped when `ended` closes, and what it had was shown before: the stream waits
/// on the run, not on the clock.
#[skuld::test]
fn the_follower_is_stopped_when_the_run_ends() {
    let journal = JournalFake::new(Vec::new())
        .follows(vec![
            marker("c2", "s1", "inv"),
            line("c3", "s1", "a"),
            line("c4", "s1", "b"),
        ])
        .then_reads(vec![line("c5", "s1", "c")]);
    let mut streaming = Streaming::start(OutputSource::Journal(output_over(Arc::new(journal.clone()))));

    streaming.shows("a\nb\n");
    assert_eq!(journal.stops(), 0, "the run has not ended");
    streaming.ends().expect("the stream");

    assert_eq!(journal.stops(), 1);
    assert_eq!(journal.syncs(), 1);
}

// The real journalctl =================================================================================================

/// A `journalctl` as a shell script: the follow writes the run's marker, one whole line and half of
/// another and then waits, `--sync` succeeds, and the read of the stream after `c3` answers the line
/// the follow did not finish. The script's pid is written to `pid`.
fn follower_script(pid: &Path) -> String {
    format!(
        r#"case " $* " in
  *" --sync "*) exit 0;;
  *" -f "*) echo $$ > '{}'
     printf '{{"MESSAGE":"goetia-run inv","_STREAM_ID":"s1","__CURSOR":"c2"}}\n'
     printf '{{"MESSAGE":"a","_STREAM_ID":"s1","__CURSOR":"c3"}}\n{{"MESSAGE":"b","_STR'
     exec sleep 100000;;
  *) printf '{{"MESSAGE":"b","_STREAM_ID":"s1","__CURSOR":"c4"}}\n';;
esac"#,
        pid.display()
    )
}

/// A killed follower's half a line is no entry: it is read again after the last whole one, once. The
/// `journalctl` is gone when the stream returns.
#[skuld::test]
fn a_partial_trailing_line_is_discarded_and_reread_after_the_cursor() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let pid = dir.path().join("pid");
    let journal = Journalctl::of(
        ["/bin/sh", "-c", &follower_script(&pid), "journalctl"]
            .map(String::from)
            .to_vec(),
    );
    let mut streaming = Streaming::start(OutputSource::Journal(output_over(Arc::new(journal))));

    streaming.shows(
        "a
",
    );
    let child = std::fs::read_to_string(&pid).expect("the follower's pid");
    assert!(
        Path::new(&format!("/proc/{}", child.trim())).exists(),
        "it is following"
    );
    // The run ends; the follower is stopped and its pipe reaches EOF.
    let (result, shown) = streaming.ends_with_output();

    result.expect("the stream");
    assert_eq!(
        shown,
        "a
b
"
    );
    assert!(
        !Path::new(&format!("/proc/{}", child.trim())).exists(),
        "the journalctl outlived the stream"
    );
}
