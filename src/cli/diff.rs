//! `goetia daemon diff [-f FILE]`
//!
//! Read-only: never checks elevation. Always needs a manager, `-f` or not.
//!
//! Renders [`ServiceManager::preview_install`] — the same
//! [`crate::decide::decide`] call `install` uses — rather than reimplementing
//! that policy against `list()`'s output: `list()` excludes foreign services
//! and never carries the raw on-disk artifact text, so it could not tell
//! "absent" from "occupied by a stranger's service" or "up to date" from
//! "hand-edited outside Goetia".
//!
//! Values are escaped for reading (see [`super::show`], guarantee 4).

use std::fmt::Write as _;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::PathBuf;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::refusal::{Lines, Mood, NotRefused};
use super::support::{load_and_warn, print_error, print_line};
use crate::error::Result;
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to goetia.yaml, or a directory containing it.
    #[arg(short = 'f', long = "file", default_value = ".")]
    pub file: PathBuf,
}

pub fn run(
    args: &Args,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let spec = match load_and_warn(&args.file, err) {
        Ok(spec) => spec,
        Err(e) => {
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            print_error(err, None, "", &e);
            return ExitCode::Error;
        }
    };

    let outcome = match mgr.preview_install(&spec) {
        Ok(outcome) => outcome,
        Err(e) => {
            print_error(err, Some(spec.common().id.as_str()), "", &e);
            return ExitCode::of(&e);
        }
    };
    let lines = Lines {
        id: &spec.common().id,
        mood: Mood::Would,
    };
    let settled = match lines.refusal(&outcome, out, err) {
        ControlFlow::Break(code) => return code,
        ControlFlow::Continue(settled) => settled,
    };
    match settled {
        NotRefused::Create => {
            let _ = writeln!(out, "{}: not installed (would be created)", spec.common().id);
        }
        NotRefused::UpToDate => {
            let _ = writeln!(out, "{}: up to date", spec.common().id);
            return ExitCode::Success;
        }
        NotRefused::Update { spec_diff } => {
            let _ = writeln!(out, "{}:", spec.common().id);
            let _ = write!(out, "{spec_diff}");
        }
        NotRefused::Stale { from_version } => {
            print_line(out, None, Some(spec.common().id.as_str()), |sink| {
                write!(sink, "would be regenerated (built by goetia {from_version})")
            });
        }
    }
    ExitCode::Drift
}
