//! What a `run` shows of the run it follows: the output stream, written once for every backend.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::backend::{Marker, OutputSource};
use crate::error::{Error, Result};
use crate::spec::Id;

/// How long the tail of a log file waits for the run's end before it reads what the file gained. It
/// costs latency only: each expiry is a re-check of the file, never a conclusion about the run, and
/// the file is read to its end once the run is seen to have ended.
pub(crate) const TAIL_INTERVAL: Duration = Duration::from_millis(100);

/// Copies the output of a run to `out` until it has ended, which is when `ended` is closed (nothing
/// is ever sent on it). Returns only after everything the run wrote before it ended has been
/// written.
///
/// A failure to read the output or to write it ends the stream and is returned; the caller still
/// has the run to wait for.
pub(crate) fn stream(source: OutputSource, id: &Id, out: &mut dyn Write, ended: &Receiver<()>) -> Result<()> {
    match source {
        OutputSource::LogFile { path, offset, marker } => tail(path, offset, marker, out, ended),
        #[cfg(target_os = "linux")]
        OutputSource::Journal(journal) => journal.stream(out, ended),
        OutputSource::None => {
            note(
                out,
                &format!("note: `{id}` has no `logs` file, so its output is not shown; waiting for the run to end\n"),
            )?;
            // Closed, never sent on: this ends when the run does.
            while ended.recv().is_ok() {}
            Ok(())
        }
    }
}

// Log file ============================================================================================================

/// Shows `path` from `offset` as it grows. The run's end is looked for first and the file read after
/// it, so a line written before the end is shown whatever `TAIL_INTERVAL` is.
///
/// With a `marker`, only the run's own lines are shown ([`Marked`]), and a run whose marker the file
/// never held gets a note once it has ended and the file is read to its end.
fn tail(path: PathBuf, offset: u64, marker: Option<Marker>, out: &mut dyn Write, ended: &Receiver<()>) -> Result<()> {
    let mut position = offset;
    let mut marked = marker.map(Marked::new);
    loop {
        let finished = match ended.recv_timeout(TAIL_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => false,
            // Nothing is sent: a closed channel is the run's end.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        };
        position = read_new(&path, position, marked.as_mut(), out)?;
        if finished {
            if let Some(marked) = marked {
                marked.end(&path, out)?;
            }
            return Ok(());
        }
    }
}

/// What `path` holds from `position` to its end, written to `out` (all of it, or the run's lines when
/// `marked`); the position after it. A file that is not there is no output yet. A file shorter than
/// `position` was truncated: it is noted, and shown from its start.
fn read_new(path: &Path, mut position: u64, mut marked: Option<&mut Marked>, out: &mut dyn Write) -> Result<u64> {
    let read_failed = |source: io::Error| Error::Os {
        what: "read the output of the run".to_string(),
        path: Some(path.to_path_buf()),
        source: source.into(),
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(position),
        Err(e) => return Err(read_failed(e)),
    };
    if file.metadata().map_err(read_failed)?.len() < position {
        note(
            out,
            &format!("note: {} was truncated; showing it from its start\n", path.display()),
        )?;
        position = 0;
        if let Some(marked) = marked.as_deref_mut() {
            marked.truncated();
        }
    }
    file.seek(SeekFrom::Start(position)).map_err(read_failed)?;

    let mut buffer = [0u8; 8192];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_failed(e)),
        };
        match marked.as_deref_mut() {
            Some(marked) => {
                let mut lines = Vec::new();
                marked.feed(&buffer[..read], &mut lines);
                if !lines.is_empty() {
                    show(out, &lines)?;
                }
            }
            None => show(out, &buffer[..read])?,
        }
        position += read as u64;
    }
    Ok(position)
}

/// Which of the lines of a file that several runs append to are one run's: those after its marker
/// line and before the next marker line of any run. A line is judged whole, so one written in pieces
/// waits for its end.
///
/// A line the program itself prints that starts as a marker does (`<prefix> `) ends the run's output
/// there: that is the program's own action, and is not guarded.
struct Marked {
    marker: Marker,
    phase: Phase,
    /// The start of a line not yet whole.
    pending: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The run's marker line has not been read.
    Seeking,
    /// Between the run's marker and the next run's.
    Ours,
    /// The next run's marker was read: nothing more is the run's.
    Over,
}

impl Marked {
    fn new(marker: Marker) -> Marked {
        Marked {
            marker,
            phase: Phase::Seeking,
            pending: Vec::new(),
        }
    }

    /// Takes `bytes` of the file in order; adds the run's lines among them to `shown`.
    fn feed(&mut self, bytes: &[u8], shown: &mut Vec<u8>) {
        if self.phase == Phase::Over {
            return;
        }
        self.pending.extend_from_slice(bytes);
        while self.phase != Phase::Over {
            let Some(end) = self.pending.iter().position(|&b| b == b'\n') else {
                return;
            };
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.line(&line[..end], &line, shown);
        }
        self.pending.clear();
    }

    /// One line: `text` without its newline, `whole` with it.
    fn line(&mut self, text: &[u8], whole: &[u8], shown: &mut Vec<u8>) {
        match self.phase {
            Phase::Seeking if text == self.marker.line.as_bytes() => self.phase = Phase::Ours,
            Phase::Seeking | Phase::Over => {}
            Phase::Ours if self.is_a_marker(text) => self.phase = Phase::Over,
            Phase::Ours => shown.extend_from_slice(whole),
        }
    }

    fn is_a_marker(&self, text: &[u8]) -> bool {
        text.strip_prefix(self.marker.prefix.as_bytes())
            .is_some_and(|rest| rest.first() == Some(&b' '))
    }

    /// The file was emptied: what was read of it is gone, and the state stays.
    fn truncated(&mut self) {
        self.pending.clear();
    }

    /// The run has ended and the file is read to its end: the last line, if it has no newline, is the
    /// run's own; a run whose marker was never read gets a note.
    fn end(mut self, path: &Path, out: &mut dyn Write) -> Result<()> {
        let rest = std::mem::take(&mut self.pending);
        let mut shown = Vec::new();
        if !rest.is_empty() {
            self.line(&rest, &rest, &mut shown);
        }
        if !shown.is_empty() {
            show(out, &shown)?;
        }
        if self.phase == Phase::Seeking {
            note(
                out,
                &format!(
                    "note: the marker line of this run is not in {}, so its output is not shown: the file was \
                     emptied or replaced after the run began\n",
                    path.display()
                ),
            )?;
        }
        Ok(())
    }
}

// Writing =============================================================================================================

fn note(out: &mut dyn Write, line: &str) -> Result<()> {
    show(out, line.as_bytes())
}

/// `bytes` to `out`, flushed: the output is read as it is written.
pub(crate) fn show(out: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    out.write_all(bytes)
        .and_then(|()| out.flush())
        .map_err(|source| Error::os("write the output of the run", source))
}

#[cfg(test)]
#[path = "output_tests.rs"]
pub(crate) mod output_tests;
