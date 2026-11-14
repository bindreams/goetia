//! The real journal: `journalctl`, run once per query, with the environment every `systemctl` of
//! goetia's gets (no `SYSTEMD_*` of the caller's).

use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Cursor, Entries, Entry, Follower, Journal, Match, Stop};
use crate::backend::bounded::{self, Finished, Reservation};
use crate::backend::client::{Client as _, ClientError, Real, Wait};
use crate::backend::systemd::manager::systemctl::environment;
use crate::error::{Error, Result};

/// What every query passes: all of every field (`-a`), as JSON, with no pager, from the system
/// journal only (`--system`: see [`Journal`]).
const COMMON: [&str; 5] = ["-a", "-o", "json", "--no-pager", "--system"];

pub(in crate::backend::systemd::manager) struct Journalctl {
    program: Vec<String>,
    /// Runs `journalctl --sync` to completion.
    client: Real,
}

impl Journalctl {
    pub(in crate::backend::systemd::manager) fn native() -> Journalctl {
        Journalctl::of(vec!["journalctl".to_string()])
    }

    /// The journal as `program` answers it, with the arguments of a query after `program`'s own.
    pub(in crate::backend::systemd::manager) fn of(program: Vec<String>) -> Journalctl {
        debug_assert!(!program.is_empty(), "a program to run");
        let client = Real::new(program.clone(), environment);
        Journalctl { program, client }
    }

    /// `journalctl` with `arguments`, its output piped and read by the caller and its diagnostics
    /// written to `stderr`. `follows`: the query does not end on its own.
    fn query(&self, arguments: Vec<String>, follows: bool, stderr: File) -> Result<Lines> {
        let argv: Vec<&str> = self.program.iter().chain(&arguments).map(String::as_str).collect();
        let command = argv.join(" ");
        let spawn_failed = |e: cosca::error::Error| Error::spawn(command.clone(), e);
        let diagnostics = stderr
            .try_clone()
            .map_err(|e| Error::os("make a file for `journalctl`'s diagnostics", e))?;
        let mut cmd = bounded::command(argv[0], &argv[1..]).map_err(spawn_failed)?;
        environment(&mut cmd);
        cmd.stdout(cosca::Stdio::pipe_out()).map_err(spawn_failed)?;
        cmd.stderr(cosca::Stdio::from_file(diagnostics)).map_err(spawn_failed)?;
        let mut child = cmd.spawn().map_err(spawn_failed)?;
        let Some(stdout) = child.stdout() else {
            return Err(Error::spawn(
                command,
                std::io::Error::other("the child has no stdout pipe"),
            ));
        };
        Ok(Lines {
            lines: BufReader::new(stdout),
            child: Arc::new(child),
            stderr,
            command,
            follows,
            stopped: Arc::default(),
            ended: false,
        })
    }
}

/// A temp file for a `journalctl`'s diagnostics, made by the caller.
fn diagnostics() -> Result<File> {
    tempfile::tempfile().map_err(|e| Error::os("make a file for `journalctl`'s diagnostics", e))
}

/// The file a followed query's diagnostics go to, drawn from `draws`.
fn drawn(draws: &mut Reservation) -> Result<File> {
    draws
        .file()
        .map_err(|e| Error::os("make a file for `journalctl`'s diagnostics", e))
}

/// `COMMON`, then `options`, then `matches`.
fn arguments(options: &[String], matches: &[Match]) -> Vec<String> {
    COMMON
        .iter()
        .map(|argument| argument.to_string())
        .chain(options.iter().cloned())
        .chain(matches.iter().map(Match::argument))
        .collect()
}

fn options(of: &[&str]) -> Vec<String> {
    of.iter().map(|option| option.to_string()).collect()
}

impl Journal for Journalctl {
    fn newest_first(&self, matches: &[Match]) -> Result<Box<dyn Entries>> {
        // The newest-first read is `status`'s, which draws on no reservation.
        Ok(Box::new(self.query(
            arguments(&options(&["-r"]), matches),
            false,
            diagnostics()?,
        )?))
    }

    fn follow(&self, matches: &[Match], after: Option<&Cursor>, draws: &mut Reservation) -> Result<Box<dyn Follower>> {
        let mut chosen = options(&["-f", "-n", "all"]);
        // Job numbers restart with each boot.
        if matches.iter().any(|m| m.is("JOB_ID")) {
            chosen.push("-b".to_string());
        }
        if let Some(cursor) = after {
            chosen.push(format!("--after-cursor={}", cursor.as_str()));
        }
        Ok(Box::new(self.query(
            arguments(&chosen, matches),
            true,
            drawn(draws)?,
        )?))
    }

    fn read_after(&self, matches: &[Match], cursor: &Cursor, draws: &mut Reservation) -> Result<Vec<Entry>> {
        let chosen = vec![format!("--after-cursor={}", cursor.as_str())];
        let mut lines = self.query(arguments(&chosen, matches), false, drawn(draws)?)?;
        let mut all = Vec::new();
        while let Some(entry) = lines.next()? {
            all.push(entry);
        }
        Ok(all)
    }

    fn sync(&self) -> Result<()> {
        let command = format!("{} --sync", self.program.join(" "));
        let finished = self
            .client
            .run(&["--sync"], Wait::ToCompletion, &mut Reservation::default())
            .map_err(|error| {
                let (ClientError::NotRun(source)
                | ClientError::MayHaveRun(source)
                | ClientError::Lost { error: source, .. }) = error;
                Error::spawn(command.clone(), source)
            })?;
        match finished {
            Finished::Exited { status, .. } if status.success() => Ok(()),
            Finished::Exited { status, capture } => Err(Error::command(command, status, &capture.stderr)),
            Finished::Expired | Finished::Left(_) => unreachable!("a run to completion does not expire"),
        }
    }
}

/// The lines `journalctl -o json` writes, one entry each.
struct Lines {
    lines: BufReader<cosca::PipeReader>,
    /// Dropped with this (and with every [`Stop`] made of it), which ends the `journalctl` if it
    /// still runs.
    child: Arc<cosca::Child>,
    /// What the child wrote on stderr, which is only read for the error of a failed exit.
    stderr: File,
    command: String,
    /// The query does not end on its own: its output ending is a failure unless it was stopped.
    follows: bool,
    /// Set before the child is killed by a [`Stop`], so its end is the end that was asked for.
    stopped: Arc<AtomicBool>,
    ended: bool,
}

impl Entries for Lines {
    fn next(&mut self) -> Result<Option<Entry>> {
        if self.ended {
            return Ok(None);
        }
        let mut line = Vec::new();
        self.lines
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::os("read from `journalctl`", e))?;
        // A line the child did not finish (it was killed mid-write) is no entry: the exit status
        // below says whether that was a failure.
        if line.ends_with(b"\n") {
            return Entry::parse(&line).map(Some);
        }
        self.ended = true;
        self.finish()
    }
}

impl Follower for Lines {
    fn stop(&self) -> Stop {
        let stopped = Arc::clone(&self.stopped);
        let child = Arc::clone(&self.child);
        Arc::new(move || {
            stopped.store(true, Ordering::SeqCst);
            // A child that cannot be signalled has exited: it is ours, held by its pidfd.
            _ = child.kill();
        })
    }
}

impl Lines {
    /// The output is at its end: it was stopped, or `journalctl` ended `Ok`, or failed with what it
    /// said.
    fn finish(&mut self) -> Result<Option<Entry>> {
        let status = self.child.wait().map_err(|e| Error::spawn(self.command.clone(), e))?;
        if self.stopped.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if status.success() {
            if self.follows {
                return Err(Error::os(
                    "follow the journal",
                    std::io::Error::other(format!("`{}` ended on its own", self.command)),
                ));
            }
            return Ok(None);
        }
        let mut said = Vec::new();
        // The child has exited, so what it wrote is whole.
        let read = self.stderr.rewind().and_then(|()| self.stderr.read_to_end(&mut said));
        if let Err(e) = read {
            return Err(Error::Both {
                first: Box::new(Error::command(self.command.clone(), status, b"")),
                then: Box::new(Error::os("read `journalctl`'s diagnostics", e)),
            });
        }
        Err(Error::command(self.command.clone(), status, &said))
    }
}
