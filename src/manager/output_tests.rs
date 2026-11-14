use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crossbeam_channel::{Receiver, Sender};

use super::*;
use crate::error::Error;
use crate::spec::Id;

// Fixtures ============================================================================================================

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

/// What a run's end looks like to the stream: the channel `flow::run` closes when the wait finishes.
fn run_end() -> (Sender<()>, Receiver<()>) {
    crossbeam_channel::bounded(0)
}

/// An `out` that hands each write to the test, so a test waits for what the stream has shown rather
/// than for time to pass.
struct Shown(Sender<Vec<u8>>);

impl Write for Shown {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(buf.to_vec()).expect("the test is still listening");
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A stream running on its own thread, as `flow::run`'s caller runs it, over a log.
pub(crate) struct Streaming {
    end: Sender<()>,
    shown: Receiver<Vec<u8>>,
    thread: std::thread::JoinHandle<Result<()>>,
    seen: String,
}

impl Streaming {
    pub(crate) fn start(source: OutputSource) -> Self {
        let (end, ended) = run_end();
        let (tx, shown) = crossbeam_channel::unbounded();
        let thread = std::thread::spawn(move || stream(source, &id(), &mut Shown(tx), &ended, &AtomicBool::new(true)));
        Self {
            end,
            shown,
            thread,
            seen: String::new(),
        }
    }

    /// Blocks until the stream has shown everything of `expected` and no more.
    pub(crate) fn shows(&mut self, expected: &str) {
        while self.seen.len() < expected.len() {
            let chunk = self.shown.recv().expect("the stream ended before showing it all");
            self.seen.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert_eq!(self.seen, expected);
    }

    /// The run ends; the stream's result.
    pub(crate) fn ends(self) -> Result<()> {
        self.ends_with_output().0
    }

    /// The run ends; the stream's result, and everything it has shown in all.
    pub(crate) fn ends_with_output(mut self) -> (Result<()>, String) {
        drop(self.end);
        let result = self.thread.join().expect("the stream does not panic");
        // The stream has returned: everything it showed is in the channel.
        for chunk in self.shown.try_iter() {
            self.seen.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        (result, self.seen)
    }
}

fn log_in(dir: &Path) -> PathBuf {
    dir.join("job.log")
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// Everything `stream` writes for `source` when the run has already ended.
fn streamed_after_the_end(source: OutputSource) -> (Result<()>, String) {
    let (end, ended) = run_end();
    drop(end);
    let mut out = Vec::new();
    let result = stream(source, &id(), &mut out, &ended, &AtomicBool::new(true));
    (result, String::from_utf8(out).unwrap())
}

/// [`streamed_after_the_end`] for a run whose wait did not find its exit.
fn streamed_after_an_unrecorded_end(source: OutputSource) -> (Result<()>, String) {
    let (end, ended) = run_end();
    drop(end);
    let mut out = Vec::new();
    let result = stream(source, &id(), &mut out, &ended, &AtomicBool::new(false));
    (result, String::from_utf8(out).unwrap())
}

// LogFile =============================================================================================================

#[skuld::test]
fn streams_from_the_offset_and_not_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "an earlier run\n");
    let offset = std::fs::metadata(&path).unwrap().len();
    append(&path, "this run\n");

    let (result, out) = streamed_after_the_end(OutputSource::LogFile {
        path,
        offset,
        marker: None,
    });

    result.unwrap();
    assert_eq!(out, "this run\n");
}

#[skuld::test]
fn shows_lines_written_while_the_run_lasts() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "");
    let mut streaming = Streaming::start(OutputSource::LogFile {
        path: path.clone(),
        offset: 0,
        marker: None,
    });

    append(&path, "one\n");
    streaming.shows("one\n");
    append(&path, "two\n");
    streaming.shows("one\ntwo\n");

    streaming.ends().unwrap();
}

/// The end is seen first and the file read after it, so output written before the end is shown
/// however long the interval is: the end here is already there when the stream starts.
#[skuld::test]
fn reads_to_eof_after_the_end_is_seen() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    let lines: String = (0..2000).map(|n| format!("line {n}\n")).collect();
    append(&path, &lines);

    let (result, out) = streamed_after_the_end(OutputSource::LogFile {
        path,
        offset: 0,
        marker: None,
    });

    result.unwrap();
    assert_eq!(out, lines);
}

#[skuld::test]
fn a_file_created_by_the_run_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    let mut streaming = Streaming::start(OutputSource::LogFile {
        path: path.clone(),
        offset: 0,
        marker: None,
    });

    append(&path, "late\n");
    streaming.shows("late\n");

    streaming.ends().unwrap();
}

#[skuld::test]
fn a_file_the_run_never_created_is_no_output() {
    let dir = tempfile::tempdir().unwrap();

    let (result, out) = streamed_after_the_end(OutputSource::LogFile {
        path: log_in(dir.path()),
        offset: 0,
        marker: None,
    });

    result.unwrap();
    assert_eq!(out, "");
}

#[skuld::test]
fn a_truncated_file_is_noted_and_read_from_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "aaaa\nbbbb\n");
    let mut streaming = Streaming::start(OutputSource::LogFile {
        path: path.clone(),
        offset: 0,
        marker: None,
    });
    streaming.shows("aaaa\nbbbb\n");

    std::fs::write(&path, "c\n").unwrap();

    let note = format!("note: {} was truncated; showing it from its start\n", path.display());
    streaming.shows(&format!("aaaa\nbbbb\n{note}c\n"));
    streaming.ends().unwrap();
}

#[skuld::test]
fn an_unreadable_file_ends_the_stream_with_its_error() {
    let dir = tempfile::tempdir().unwrap();

    // A directory cannot be read as a file: the open or the first read fails, whichever the host does.
    let (result, out) = streamed_after_the_end(OutputSource::LogFile {
        path: dir.path().to_path_buf(),
        offset: 0,
        marker: None,
    });

    let e = result.expect_err("a directory is not a log");
    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    assert!(e.to_string().contains(&dir.path().display().to_string()), "{e}");
    assert_eq!(out, "");
}

#[skuld::test]
fn a_failed_write_ends_the_stream_with_its_error() {
    struct Refuses;
    impl Write for Refuses {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("sink refused"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "x\n");
    let (end, ended) = run_end();
    drop(end);

    let e = stream(
        OutputSource::LogFile {
            path,
            offset: 0,
            marker: None,
        },
        &id(),
        &mut Refuses,
        &ended,
        &AtomicBool::new(true),
    )
    .expect_err("the sink refuses");

    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    assert!(e.report().to_string().contains("sink refused"), "{}", e.report());
}

// A log file with markers =============================================================================================
//
// A backend whose runs each write a marker line first (the systemd task wrapper) gives the marker, so
// that runs sharing one file are told apart: our output is the lines between our marker and the next
// run's, and no marker is ever shown.

/// The marker of the run `inv`, as a systemd task writes it.
fn marker(inv: &str) -> Marker {
    Marker {
        prefix: "goetia-run",
        line: format!("goetia-run {inv}"),
        id_len: 32,
    }
}

fn marked(path: PathBuf, offset: u64, inv: &str) -> OutputSource {
    OutputSource::LogFile {
        path,
        offset,
        marker: Some(marker(inv)),
    }
}

/// The note of a run whose marker is not in its file.
fn no_marker_note(path: &Path) -> String {
    format!(
        "note: the marker line of this run is not in {}, so its output is not shown: the file was \
         emptied or replaced after the run began\n",
        path.display()
    )
}

/// Runs that follow one another in one file: ours shows its own lines whatever else the file holds,
/// before our marker or after the next one.
#[skuld::test]
fn a_run_shows_only_its_own_lines_in_a_file_that_holds_other_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "an earlier run\n");
    let offset = std::fs::metadata(&path).unwrap().len();
    append(
        &path,
        "goetia-run before\nbefore 1\ngoetia-run ours\n1\n2\ngoetia-run next\nnext 1\ngoetia-run after\nafter 1\n",
    );

    let (result, out) = streamed_after_the_end(marked(path, offset, "ours"));

    result.unwrap();
    assert_eq!(out, "1\n2\n");
}

/// A run that printed nothing has its marker and no lines, which is no missing marker.
#[skuld::test]
fn a_run_that_printed_nothing_is_no_output_and_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "goetia-run ours\ngoetia-run next\nnext 1\n");

    let (result, out) = streamed_after_the_end(marked(path, 0, "ours"));

    result.unwrap();
    assert_eq!(out, "");
}

/// While the run lasts, lines appear as they are written, a line is held until it is whole, and
/// lines of a run that started after ours are never shown.
#[skuld::test]
fn marked_lines_are_shown_as_they_are_written_and_stop_at_the_next_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "");
    let mut streaming = Streaming::start(marked(path.clone(), 0, "ours"));

    append(&path, "goetia-r");
    append(&path, "un ours\none");
    append(&path, "\ntwo\n");
    streaming.shows("one\ntwo\n");
    append(&path, "goetia-run next\n");
    append(&path, "three\n");
    let (result, out) = streaming.ends_with_output();

    result.unwrap();
    assert_eq!(out, "one\ntwo\n");
}

/// A program that prints a line starting with `goetia-run ` ends its own output there. That is the
/// program's own action, as printing the marker text of another run would be; it is not guarded.
#[skuld::test]
fn a_line_the_program_prints_that_looks_like_a_marker_ends_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "goetia-run ours\n1\ngoetia-run fake\n2\n");

    let (result, out) = streamed_after_the_end(marked(path, 0, "ours"));

    result.unwrap();
    assert_eq!(out, "1\n");
}

/// The last line of a run may have no newline: it is shown once the run has ended.
#[skuld::test]
fn an_unterminated_last_line_is_shown_when_the_run_has_ended() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "goetia-run ours\n1\npartial");

    let (result, out) = streamed_after_the_end(marked(path, 0, "ours"));

    result.unwrap();
    assert_eq!(out, "1\npartial");
}

/// No marker of ours after the end and a read to EOF: a note, and the other runs' lines stay unshown.
#[skuld::test]
fn no_marker_is_a_note_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "goetia-run another\nits line\nplain line\n");

    let (result, out) = streamed_after_the_end(marked(path.clone(), 0, "ours"));

    result.unwrap();
    assert_eq!(out, no_marker_note(&path));
}

#[skuld::test]
fn a_file_the_run_never_created_has_no_marker_either() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());

    let (result, out) = streamed_after_the_end(marked(path.clone(), 0, "ours"));

    result.unwrap();
    assert_eq!(out, no_marker_note(&path));
}

/// A file emptied while our run shows keeps the state it was in: lines after the truncation are
/// still ours.
#[skuld::test]
fn a_truncated_marked_file_goes_on_with_the_state_it_was_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "goetia-run ours\naaaa\nbbbb\n");
    let mut streaming = Streaming::start(marked(path.clone(), 0, "ours"));
    streaming.shows("aaaa\nbbbb\n");

    std::fs::write(&path, "c\n").unwrap();

    let note = format!("note: {} was truncated; showing it from its start\n", path.display());
    streaming.shows(&format!("aaaa\nbbbb\n{note}c\n"));
    streaming.ends().unwrap();
}

// Glued markers -------------------------------------------------------------------------------------------------------
//
// A run whose last line has no newline leaves the next run's marker on that line: `partialgoetia-run <id>`.
// A marker that ends a line as `goetia-run ` and 32 lowercase hex digits (an `INVOCATION_ID`) is split off.

const RUN_A: &str = "0123456789abcdef0123456789abcdef";
const RUN_B: &str = "fedcba9876543210fedcba9876543210";

/// Our run's last line has no newline, and the next run's marker follows it on the same line: our line
/// is shown, ended with a newline, and the next run's lines are not.
#[skuld::test]
fn our_unterminated_last_line_is_shown_when_the_next_marker_is_glued_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, &format!("goetia-run {RUN_A}\n1\nlastgoetia-run {RUN_B}\nb1\nb2"));

    let (result, out) = streamed_after_the_end(marked(path, 0, RUN_A));

    result.unwrap();
    assert_eq!(out, "1\nlast\n");
}

/// The run before ours left no newline: our marker is on its last line, and our output starts after it.
#[skuld::test]
fn our_marker_glued_to_the_previous_runs_last_line_starts_our_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(
        &path,
        &format!("goetia-run {RUN_B}\nb1\nb lastgoetia-run {RUN_A}\n1\n2\n"),
    );

    let (result, out) = streamed_after_the_end(marked(path, 0, RUN_A));

    result.unwrap();
    assert_eq!(out, "1\n2\n");
}

/// While the run lasts the unterminated line is shown as it arrives, and the next marker, glued to
/// it, ends it.
#[skuld::test]
fn a_glued_marker_ends_the_output_while_the_run_lasts() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "");
    let mut streaming = Streaming::start(marked(path.clone(), 0, RUN_A));

    append(&path, &format!("goetia-run {RUN_A}\n1\nlast"));
    streaming.shows("1\nlast");
    append(&path, &format!("goetia-run {RUN_B}\nb1\n"));
    streaming.shows("1\nlast\n");
    let (result, out) = streaming.ends_with_output();

    result.unwrap();
    assert_eq!(out, "1\nlast\n");
}

/// The glued marker is the file's last bytes, with no newline of its own yet.
#[skuld::test]
fn a_glued_marker_at_the_end_of_the_file_ends_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, &format!("goetia-run {RUN_A}\n1\nlastgoetia-run {RUN_B}"));

    let (result, out) = streamed_after_the_end(marked(path, 0, RUN_A));

    result.unwrap();
    assert_eq!(out, "1\nlast\n");
}

/// Only a marker of the form an `INVOCATION_ID` makes is split off a line: the same words with fewer
/// digits, or with capitals, are the program's output. A program that prints a line that does end so
/// has done it itself, which is not guarded.
#[skuld::test]
fn text_that_only_resembles_a_glued_marker_is_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    let short = &RUN_B[1..];
    let upper = RUN_B.to_uppercase();
    append(
        &path,
        &format!("goetia-run {RUN_A}\nsee goetia-run {short}\nsee goetia-run {upper}\nsee goetia-run\n"),
    );

    let (result, out) = streamed_after_the_end(marked(path, 0, RUN_A));

    result.unwrap();
    assert_eq!(
        out,
        format!("see goetia-run {short}\nsee goetia-run {upper}\nsee goetia-run\n")
    );
}

// Runs that began before the offset -----------------------------------------------------------------------------------

/// A timer started the run between the state read and the request, so the request joined it: its
/// marker (and its first lines) are before the offset, which was measured before the request. Our
/// marker is found by a read from the file's start, and our output is shown from it.
#[skuld::test]
fn a_run_joined_in_flight_shows_from_its_marker_before_the_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(
        &path,
        &format!("an earlier run\ngoetia-run {RUN_B}\nb1\ngoetia-run {RUN_A}\n1\n"),
    );
    let offset = std::fs::metadata(&path).unwrap().len();
    append(&path, &format!("2\ngoetia-run {RUN_B}\nb2\n"));

    let (result, out) = streamed_after_the_end(marked(path, offset, RUN_A));

    result.unwrap();
    assert_eq!(out, "1\n2\n");
}

/// A joined run that is found from the start is exact, and a file that does not hold our marker even
/// there is still the note.
#[skuld::test]
fn a_marker_in_neither_part_of_the_file_is_the_note() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, &format!("goetia-run {RUN_B}\nb1\n"));
    let offset = std::fs::metadata(&path).unwrap().len();
    append(&path, "b2\n");

    let (result, out) = streamed_after_the_end(marked(path.clone(), offset, RUN_A));

    result.unwrap();
    assert_eq!(out, no_marker_note(&path));
}

/// The wait found no exit, so the stream shows no note about a marker beside that failure: the run's
/// error says what is wrong.
#[skuld::test]
fn no_marker_is_no_note_when_the_waits_exit_is_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, &format!("goetia-run {RUN_B}\nb1\n"));

    let (result, out) = streamed_after_an_unrecorded_end(marked(path, 0, RUN_A));

    result.unwrap();
    assert_eq!(out, "");
}

// Partial lines -------------------------------------------------------------------------------------------------------

/// A prompt or a `\r` progress line has no newline: it is shown as it arrives, and the line is ended
/// when the next marker follows it.
#[skuld::test]
fn a_partial_line_is_shown_as_it_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "");
    let mut streaming = Streaming::start(marked(path.clone(), 0, RUN_A));

    append(&path, &format!("goetia-run {RUN_A}\n"));
    append(&path, "Password: ");
    streaming.shows("Password: ");
    append(&path, "10%\r20%\r");
    streaming.shows("Password: 10%\r20%\r");
    append(&path, &format!("goetia-run {RUN_B}\nb1\n"));
    streaming.shows("Password: 10%\r20%\r\n");
    let (result, out) = streaming.ends_with_output();

    result.unwrap();
    assert_eq!(out, "Password: 10%\r20%\r\n");
}

/// Only what could still be the start of a marker waits: a line's first characters while they are a
/// prefix of `goetia-run `, and a trailing part that is a prefix of a marker glued to the line.
#[skuld::test]
fn only_what_could_become_a_marker_waits() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, "");
    let mut streaming = Streaming::start(marked(path.clone(), 0, RUN_A));

    append(&path, &format!("goetia-run {RUN_A}\n"));
    append(&path, "goe");
    append(&path, "tia is a word\n");
    streaming.shows("goetia is a word\n");
    append(&path, "abc goetia-run 0123");
    streaming.shows("goetia is a word\nabc ");
    append(&path, "456789abcdef0123456789abcde\nnot a marker\n");
    // The glued text was not a marker (31 digits, then a newline), so it is shown with the line.
    streaming.shows("goetia is a word\nabc goetia-run 0123456789abcdef0123456789abcde\nnot a marker\n");

    streaming.ends().unwrap();
}

/// A line that begins as a marker is a marker before its newline arrives: nothing of it is shown.
#[skuld::test]
fn a_marker_line_begun_but_not_ended_ends_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = log_in(dir.path());
    append(&path, &format!("goetia-run {RUN_A}\n1\ngoetia-run fe"));

    let (result, out) = streamed_after_the_end(marked(path, 0, RUN_A));

    result.unwrap();
    assert_eq!(out, "1\n");
}

// None ================================================================================================================

#[skuld::test]
fn no_output_source_writes_the_note_and_returns_on_end() {
    let mut streaming = Streaming::start(OutputSource::None);

    streaming.shows("note: `nightly` has no `logs` file, so its output is not shown; waiting for the run to end\n");

    streaming.ends().unwrap();
}
