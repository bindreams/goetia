//! What a `run` shows of the run it follows: the output stream, written once for every backend.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// `exit_recorded` is stored by the wait before it closes `ended`: whether it found how the run
/// ended. A run whose marker is missing shows a note about it only where it did, since a wait that
/// failed is reported as that error.
///
/// A failure to read the output or to write it ends the stream and is returned; the caller still
/// has the run to wait for.
pub(crate) fn stream(
    source: OutputSource,
    id: &Id,
    out: &mut dyn Write,
    ended: &Receiver<()>,
    exit_recorded: &AtomicBool,
) -> Result<()> {
    match source {
        OutputSource::LogFile { path, offset, marker } => tail(path, offset, marker, out, ended, exit_recorded),
        #[cfg(target_os = "linux")]
        OutputSource::Journal(journal) => journal.stream(out, ended, exit_recorded),
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
/// With a `marker`, only the run's own lines are shown ([`Marked`]). A run whose marker the file
/// never held gets a note once it has ended and the file is read to its end, unless `exit_recorded`
/// says the wait found no exit: that failure is the run's error to report.
fn tail(
    path: PathBuf,
    offset: u64,
    marker: Option<Marker>,
    out: &mut dyn Write,
    ended: &Receiver<()>,
    exit_recorded: &AtomicBool,
) -> Result<()> {
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
                // Read after the channel closed, which the wait does after it stores whether it found
                // the exit.
                marked.finish(&path, exit_recorded.load(Ordering::SeqCst), out)?;
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
/// line and before the next marker line of any run.
///
/// The run's own part is shown as it arrives, partial lines included (a prompt, a `\r` progress
/// line). Only what could still become a marker waits: a line's first characters while they are a
/// prefix of `<prefix> `, and a trailing part that could begin a marker glued to the end of a line.
/// Before the run's marker, only whole lines are judged.
///
/// A run whose last line has no newline leaves the next run's marker at the end of that line:
/// `partial<prefix> <id>`, with `id_len` hex digits. It is split off ([`Marked::split`]), and the line
/// that was left is the earlier run's last.
///
/// A line the program itself prints that starts or ends as a marker does ends the run's output there:
/// that is the program's own action, and is not guarded.
struct Marked {
    marker: Marker,
    phase: Phase,
    /// What was read of the current line and is not shown or judged yet.
    pending: Vec<u8>,
    /// Whether nothing of the current line has been shown, so `pending` begins it.
    line_start: bool,
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
            line_start: true,
        }
    }

    /// Takes `bytes` of the file in order; adds the run's output among them to `shown`.
    fn feed(&mut self, bytes: &[u8], shown: &mut Vec<u8>) {
        self.pending.extend_from_slice(bytes);
        loop {
            if self.phase == Phase::Over {
                self.pending.clear();
                return;
            }
            let Some(end) = self.pending.iter().position(|&b| b == b'\n') else {
                if self.phase == Phase::Ours {
                    self.release(shown);
                }
                return;
            };
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.line(&line[..end], true, shown);
            self.line_start = true;
        }
    }

    /// The run's own line (or what is left of one), `text` without its newline; `terminated` is whether
    /// it had one.
    fn line(&mut self, text: &[u8], terminated: bool, shown: &mut Vec<u8>) {
        let line_start = self.line_start;
        let (before, marker) = self.split(text, line_start);
        match self.phase {
            Phase::Seeking if marker == Some(self.marker.line.as_bytes()) => self.phase = Phase::Ours,
            Phase::Seeking | Phase::Over => {}
            Phase::Ours => match marker {
                Some(_) => {
                    // What is before it is the run's last line, which no newline ended; a part of the
                    // line that was shown already is ended too.
                    shown.extend_from_slice(before);
                    if !before.is_empty() || !line_start {
                        shown.push(b'\n');
                    }
                    self.phase = Phase::Over;
                }
                None => {
                    shown.extend_from_slice(text);
                    if terminated {
                        shown.push(b'\n');
                    }
                }
            },
        }
    }

    /// `text` as what is before a marker, and the marker, when it holds one: at the start of a line,
    /// a line that starts with `<prefix> ` (whatever follows); at the end of a line, the prefix, a
    /// space and `id_len` lowercase hex digits (what is before that is the run before's last line).
    fn split<'a>(&self, text: &'a [u8], line_start: bool) -> (&'a [u8], Option<&'a [u8]>) {
        let start = self.start_of_marker();
        if line_start && text.starts_with(&start) {
            return (&[], Some(text));
        }
        let Some(from) = text.len().checked_sub(start.len() + self.marker.id_len) else {
            return (text, None);
        };
        let (before, tail) = text.split_at(from);
        if tail.starts_with(&start) && tail[start.len()..].iter().all(is_hex) {
            (before, Some(tail))
        } else {
            (text, None)
        }
    }

    /// What every marker line starts with: the prefix and a space.
    fn start_of_marker(&self) -> Vec<u8> {
        let mut start = self.marker.prefix.as_bytes().to_vec();
        start.push(b' ');
        start
    }

    /// The run's part has no newline yet in `pending`: shows what cannot still become a marker.
    fn release(&mut self, shown: &mut Vec<u8>) {
        let start = self.start_of_marker();
        if self.line_start {
            if self.pending.starts_with(&start) {
                // A marker line begun: it is a marker before its newline comes.
                self.phase = Phase::Over;
                self.pending.clear();
                return;
            }
            if start.starts_with(&self.pending) {
                return;
            }
        }
        let pending = std::mem::take(&mut self.pending);
        if let (before, Some(_)) = self.split(&pending, false) {
            shown.extend_from_slice(before);
            if !before.is_empty() || !self.line_start {
                shown.push(b'\n');
            }
            self.phase = Phase::Over;
            return;
        }
        let keep = pending.len() - self.held(&pending, &start);
        shown.extend_from_slice(&pending[..keep]);
        self.line_start = self.line_start && keep == 0;
        self.pending = pending[keep..].to_vec();
    }

    /// How much of the end of `pending` could be the start of a marker that is not all there yet.
    fn held(&self, pending: &[u8], start: &[u8]) -> usize {
        let longest = start.len() + self.marker.id_len;
        let from = pending.len().saturating_sub(longest - 1);
        (from..pending.len())
            .map(|i| &pending[i..])
            .find(|tail| {
                if tail.len() <= start.len() {
                    start.starts_with(tail)
                } else {
                    tail.starts_with(start) && tail[start.len()..].iter().all(is_hex)
                }
            })
            .map_or(0, <[u8]>::len)
    }

    /// The file was emptied: what was read of it is gone, and the state stays.
    fn truncated(&mut self) {
        self.pending.clear();
        self.line_start = true;
    }

    /// The last line, if it has no newline, is judged and shown.
    fn flush(&mut self, out: &mut dyn Write) -> Result<()> {
        let rest = std::mem::take(&mut self.pending);
        let mut shown = Vec::new();
        if !rest.is_empty() {
            self.line(&rest, false, &mut shown);
        }
        if shown.is_empty() {
            return Ok(());
        }
        show(out, &shown)
    }

    /// The run has ended and the file is read to its end.
    ///
    /// A run whose marker was not found after the offset may have begun before the offset was
    /// measured: a timer started it between the state read and the request, and the request joined it.
    /// Its marker is then earlier in the file, so the file is read again from its start for it. The
    /// line is exact: it holds the run's own `INVOCATION_ID`, which no other run writes, so the first
    /// match is the only one. A run still without one gets a note, unless `exit_recorded` is false.
    fn finish(mut self, path: &Path, exit_recorded: bool, out: &mut dyn Write) -> Result<()> {
        self.flush(out)?;
        let marked = if self.phase == Phase::Seeking {
            let mut again = Marked::new(self.marker);
            read_new(path, 0, Some(&mut again), out)?;
            again.flush(out)?;
            again
        } else {
            self
        };
        if marked.phase == Phase::Seeking && exit_recorded {
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

fn is_hex(digit: &u8) -> bool {
    matches!(digit, b'0'..=b'9' | b'a'..=b'f')
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
