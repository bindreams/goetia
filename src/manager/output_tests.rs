use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
        let thread = std::thread::spawn(move || stream(source, &id(), &mut Shown(tx), &ended));
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
    let result = stream(source, &id(), &mut out, &ended);
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

// None ================================================================================================================

#[skuld::test]
fn no_output_source_writes_the_note_and_returns_on_end() {
    let mut streaming = Streaming::start(OutputSource::None);

    streaming.shows("note: `nightly` has no `logs` file, so its output is not shown; waiting for the run to end\n");

    streaming.ends().unwrap();
}
