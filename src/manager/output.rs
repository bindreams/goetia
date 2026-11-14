//! What a `run` shows of the run it follows: the output stream, written once for every backend.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::backend::OutputSource;
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
        OutputSource::LogFile { path, offset } => tail(path, offset, out, ended),
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
fn tail(path: PathBuf, offset: u64, out: &mut dyn Write, ended: &Receiver<()>) -> Result<()> {
    let mut position = offset;
    loop {
        let finished = match ended.recv_timeout(TAIL_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => false,
            // Nothing is sent: a closed channel is the run's end.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        };
        position = read_new(&path, position, out)?;
        if finished {
            return Ok(());
        }
    }
}

/// What `path` holds from `position` to its end, written to `out`; the position after it. A file
/// that is not there is no output yet. A file shorter than `position` was truncated: it is noted, and
/// shown from its start.
fn read_new(path: &Path, mut position: u64, out: &mut dyn Write) -> Result<u64> {
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
        show(out, &buffer[..read])?;
        position += read as u64;
    }
    Ok(position)
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
