//! `busctl`, for the one answer `systemctl show` cannot give: when a timer elapses next, in
//! microseconds.
//!
//! `systemctl show` prints `NextElapseUSecRealtime` as a date with whole seconds, and its
//! `--timestamp=` does not cover a timer's properties (and needs a newer systemd than the floor).
//! The bus gives the number itself: `LoadUnit` answers the unit's object path, and the timer's
//! property is read from it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::bounded::{Finished, Reservation};
use crate::backend::client::{Client, ClientError, Real, Wait};
use crate::error::{Error, Result};

use super::systemctl::{Host, environment, refuse_unreachable};

const SYSTEMD1: &str = "org.freedesktop.systemd1";

/// What the manager answers for a timer that has no next elapse: `USEC_INFINITY`, which a timer that
/// was never armed reads as (measured on systemd 255). `0` is the other reading of "none", and means
/// the same.
const NEVER: u64 = u64::MAX;

/// `busctl`, reached through a [`Client`], on the system `host` reads: the same door and the same
/// environment as [`Systemctl`](super::systemctl::Systemctl).
pub(super) struct Busctl {
    client: Box<dyn Client>,
    host: fn() -> Host,
}

impl Busctl {
    pub(super) fn new(client: Box<dyn Client>, host: fn() -> Host) -> Busctl {
        Busctl { client, host }
    }

    /// The real `busctl`, on this system.
    pub(super) fn native() -> Busctl {
        Busctl::new(Box::new(Real::new(vec!["busctl".to_string()], environment)), Host::this)
    }

    /// When `timer_unit` elapses next on the realtime clock, as the manager has it now: `None` for a
    /// timer that is not armed, which the manager answers `0` or [`NEVER`].
    pub(super) fn next_elapse(&self, timer_unit: &str) -> Result<Option<SystemTime>> {
        let path = self.unit_path(timer_unit)?;
        let property = self.run(&[
            "--system",
            "get-property",
            SYSTEMD1,
            &path,
            "org.freedesktop.systemd1.Timer",
            "NextElapseUSecRealtime",
        ])?;
        let micros =
            parse_micros(&property).ok_or_else(|| answer_lacks("get-property NextElapseUSecRealtime", "a time"))?;
        Ok((micros != 0 && micros != NEVER).then(|| UNIX_EPOCH + Duration::from_micros(micros)))
    }

    /// The object path the manager keeps `unit` at, which it loads for the answer.
    fn unit_path(&self, unit: &str) -> Result<String> {
        let reply = self.run(&[
            "--system",
            "call",
            SYSTEMD1,
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "LoadUnit",
            "s",
            unit,
        ])?;
        parse_path(&reply).ok_or_else(|| answer_lacks("call LoadUnit", "an object path"))
    }

    /// `busctl <args>`, to completion: its stdout when it exited `0`.
    fn run(&self, args: &[&str]) -> Result<String> {
        refuse_unreachable(self.host)?;
        let finished = self
            .client
            .run(args, Wait::ToCompletion, &mut Reservation::default())
            .map_err(|error| unran(args, error))?;
        match finished {
            Finished::Exited { status, capture } if status.success() => {
                Ok(String::from_utf8_lossy(&capture.stdout).into_owned())
            }
            Finished::Exited { status, capture } => Err(Error::command(
                format!("busctl {}", args.join(" ")),
                status,
                &capture.stderr,
            )),
            Finished::Expired | Finished::Left(_) => unreachable!("a run to completion does not expire"),
        }
    }
}

/// A `busctl` that did not run to its end. It changes nothing, so every way is a plain failure.
fn unran(args: &[&str], error: ClientError) -> Error {
    let source = match error {
        ClientError::NotRun(source) | ClientError::MayHaveRun(source) => source,
        ClientError::Lost { error, .. } => error,
    };
    Error::spawn(format!("busctl {}", args.join(" ")), source)
}

/// An answer that holds nothing of what `command` asked for, as [`Error::IncompleteAnswer`].
fn answer_lacks(command: &str, what: &str) -> Error {
    Error::IncompleteAnswer {
        command: format!("busctl {command}"),
        missing: vec![what.to_string()],
    }
}

/// The path in `o "<path>"`, `busctl`'s print of an object path.
fn parse_path(reply: &str) -> Option<String> {
    let quoted = reply.trim().strip_prefix("o ")?;
    let path = quoted.strip_prefix('"')?.strip_suffix('"')?;
    (path.starts_with('/') && !path.contains('"')).then(|| path.to_string())
}

/// The number in `t <number>`, `busctl`'s print of an unsigned 64-bit value.
fn parse_micros(reply: &str) -> Option<u64> {
    reply.trim().strip_prefix("t ")?.parse().ok()
}

#[cfg(test)]
#[path = "busctl_tests.rs"]
mod busctl_tests;
