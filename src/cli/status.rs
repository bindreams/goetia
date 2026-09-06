//! `goetia daemon status [ID...]`
//!
//! Read-only: never checks elevation. With no ids, every entry renders
//! through [`format_status_line`] — the exact per-id line, `pid` included —
//! so `status` has one human shape regardless of how it was invoked.

use std::io::Write;

use clap::Args as ClapArgs;

use super::report::{self, DaemonReport, Report};
use super::support::{parse_id, partition_installed, print_unreadable_warnings};
use crate::error::Result;
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
) -> i32 {
    let mgr = match get_manager() {
        Ok(mgr) => mgr,
        Err(e) => {
            let report = report::unavailable(&e);
            if json {
                report::write(&report, out);
            } else {
                let _ = writeln!(err, "error: {e}");
            }
            return report::exit_code(&report);
        }
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
    };
    for id_str in &args.ids {
        match parse_id(id_str) {
            Err(e) => report.errors.push(report::invalid_id(id_str, &e)),
            Ok(id) => match mgr.status(&id) {
                Ok(status) => report.daemons.push(report::daemon(id_str, &status)),
                Err(e) => report.errors.push(report::status_error(id_str, &e)),
            },
        }
    }

    if json {
        report::write(&report, out);
    } else {
        print_text(&report, out, err);
    }

    report::exit_code(&report)
}

fn status_all(mgr: &dyn ServiceManager, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let index = mgr.list().map(partition_installed);
    let report = match &index {
        Ok(index) => report::from_index(index),
        Err(e) => report::unavailable(e),
    };

    if json {
        report::write(&report, out);
    } else {
        match &index {
            Ok(index) => {
                // An unreadable entry is a `warning:` here rather than an
                // `error:` line, as it has been since `list` and `status`
                // first shared `partition_installed`.
                print_unreadable_warnings(&index.unreadable, err);
                print_daemons(&report, out);
            }
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
            }
        }
    }

    report::exit_code(&report)
}

fn print_text(report: &Report, out: &mut dyn Write, err: &mut dyn Write) {
    print_daemons(report, out);
    for error in &report.errors {
        match &error.id {
            Some(id) => {
                let _ = writeln!(err, "error: {id}: {}", error.message);
            }
            None => {
                let _ = writeln!(err, "error: {}", error.message);
            }
        }
    }
}

fn print_daemons(report: &Report, out: &mut dyn Write) {
    for daemon in &report.daemons {
        let _ = writeln!(out, "{}", format_status_line(daemon));
    }
}

/// The one human-readable shape `status` renders in — see the module doc
/// comment. Formats a [`DaemonReport`] so the two invocation forms cannot
/// drift: both build one before rendering anything.
fn format_status_line(daemon: &DaemonReport) -> String {
    format!(
        "{}: {} (enabled={}, pid={})",
        daemon.id,
        daemon.state,
        daemon.enabled,
        daemon.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string()),
    )
}
