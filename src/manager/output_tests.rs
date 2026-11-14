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
struct Streaming {
    end: Sender<()>,
    shown: Receiver<Vec<u8>>,
    thread: std::thread::JoinHandle<Result<()>>,
    seen: String,
}

impl Streaming {
    fn start(source: OutputSource) -> Self {
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
    fn shows(&mut self, expected: &str) {
        while self.seen.len() < expected.len() {
            let chunk = self.shown.recv().expect("the stream ended before showing it all");
            self.seen.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert_eq!(self.seen, expected);
    }

    /// The run ends; the stream's result.
    fn ends(self) -> Result<()> {
        drop(self.end);
        self.thread.join().expect("the stream does not panic")
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

    let (result, out) = streamed_after_the_end(OutputSource::LogFile { path, offset });

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

    let (result, out) = streamed_after_the_end(OutputSource::LogFile { path, offset: 0 });

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

    let e =
        stream(OutputSource::LogFile { path, offset: 0 }, &id(), &mut Refuses, &ended).expect_err("the sink refuses");

    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    assert!(e.report().to_string().contains("sink refused"), "{}", e.report());
}

// None ================================================================================================================

#[skuld::test]
fn no_output_source_writes_the_note_and_returns_on_end() {
    let mut streaming = Streaming::start(OutputSource::None);

    streaming.shows("note: `nightly` has no `logs` file, so its output is not shown; waiting for the run to end\n");

    streaming.ends().unwrap();
}
