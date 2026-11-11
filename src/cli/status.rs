//! `goetia daemon status [ID...]`
//!
//! Read-only: never checks elevation. Every entry renders through
//! `format_status_line`, so `status` has one human shape however invoked.

use std::io::Write;

use clap::Args as ClapArgs;

use super::ExitCode;
use super::report::{self, DaemonReport, Report};
use super::support::{parse_id, partition_installed, print_undetermined_warnings, print_unusable_warnings};
use crate::error::{Error, Result};
use crate::manager::ServiceManager;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Daemon ids to report on. With none, reports every installed daemon.
    pub ids: Vec<String>,
}

pub fn run(
    args: &Args,
    json: bool,
    get_manager: &dyn Fn() -> Result<Box<dyn ServiceManager>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => return unavailable(e, json, out, err),
    };

    if args.ids.is_empty() {
        return status_all(mgr.as_ref(), json, out, err);
    }

    // Ids are reported in argument order, a repeated argument yielding a
    // repeated entry. The whole `Report` is built before either renderer
    // runs, so the exit code cannot depend on the output format.
    let mut report = Report {
        daemons: Vec::new(),
        errors: Vec::new(),
        // The `list()` path's key: here an id `status(&id)` could not
        // classify is that id's own entry in `errors`.
        undetermined: Vec::new(),
    };
    for id_str in &args.ids {
        match parse_id(id_str) {
            Err(e) => report.errors.push(report::invalid_id(id_str, e)),
            Ok(id) => match mgr.status(&id) {
                Ok(status) => report.daemons.push(report::daemon(id_str, &status)),
                Err(e) => report.errors.push(report::status_error(id_str, e)),
            },
        }
    }

    if json {
        return report::emit(&report, out, err);
    }

    print_text(&report, out, err);
    report::exit_code(&report)
}

/// `report::unavailable` always maps to exit 1; taken from
/// `report::exit_code` here so json and text agree.
fn unavailable(e: Error, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let report = report::unavailable(e);
    if json {
        return report::emit(&report, out, err);
    }
    report::print_errors(&report, err);
    report::exit_code(&report)
}

fn status_all(mgr: &dyn ServiceManager, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let (index, report) = match mgr.list().map(partition_installed) {
        Ok(index) => {
            let report = report::from_index(&index);
            (Some(index), report)
        }
        Err(e) => (None, report::unavailable(e)),
    };

    if json {
        return report::emit(&report, out, err);
    }

    match &index {
        Some(index) => {
            // An unusable entry is a `warning:` here, not an `error:` line.
            print_unusable_warnings(&index.unusable, err);
            print_undetermined_warnings(&index.undetermined, err);
            print_daemons(&report, out);
        }
        None => report::print_errors(&report, err),
    }

    report::exit_code(&report)
}

fn print_text(report: &Report, out: &mut dyn Write, err: &mut dyn Write) {
    print_daemons(report, out);
    report::print_errors(report, err);
}

fn print_daemons(report: &Report, out: &mut dyn Write) {
    for daemon in &report.daemons {
        let _ = writeln!(out, "{}", format_status_line(daemon));
    }
}

/// The one human-readable shape `status` renders in, shared by both
/// invocation forms.
fn format_status_line(daemon: &DaemonReport) -> String {
    format!(
        "{}: {} (enabled={}, pid={})",
        daemon.id,
        daemon.state,
        daemon.enabled,
        daemon.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string()),
    )
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod status_tests;
