//! Running `systemctl` / `launchctl` / `sc.exe` and keeping the evidence.
//!
//! A probe that fails in CI on a platform nobody can reproduce locally is only
//! useful if the failure carries the command, its exit code, and both streams.

use std::fmt;
use std::process::Command;

use super::cleanup::retrying;

#[derive(Debug)]
pub struct Run {
    pub program: String,
    pub args: Vec<String>,
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    #[track_caller]
    pub fn expect_ok(self) -> Self {
        assert!(self.ok(), "{self}");
        self
    }
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}", self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        write!(
            f,
            "` exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

/// Runs `program`; a failure to spawn it is the error, for a caller that must not panic.
pub fn try_run(program: &str, args: &[&str]) -> Result<Run, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("spawn `{program}`: {e}"))?;
    Ok(Run {
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn run(program: &str, args: &[&str]) -> Run {
    try_run(program, args).unwrap_or_else(|e| panic!("{e}"))
}

/// Cleanup that never panics: runs every command of `argvs` in one [`retrying`] attempt, so a guard
/// spends at most one `CLEANUP_BOUND` however many it has. A command that exits with a code in `gone`
/// ("already gone": not loaded, does not exist) is done; any other failure, a spawn error included,
/// retries the whole attempt, so the commands must be safe to repeat. Returns every problem of the
/// last attempt, one per line, for the caller to log.
pub fn run_cleanup(program: &str, argvs: &[&[&str]], gone: &[i32]) -> Option<String> {
    run_cleanup_unless(program, argvs, |run| run.code.is_some_and(|code| gone.contains(&code)))
}

/// [`run_cleanup`] for a program whose "already gone" is not told by the exit code alone: `is_gone`
/// judges each failed [`Run`].
pub fn run_cleanup_unless(program: &str, argvs: &[&[&str]], is_gone: impl Fn(&Run) -> bool) -> Option<String> {
    retrying(|| {
        let problems: Vec<String> = argvs
            .iter()
            .filter_map(|args| match try_run(program, args) {
                Ok(run) if run.ok() || is_gone(&run) => None,
                Ok(run) => Some(run.to_string()),
                Err(e) => Some(e),
            })
            .collect();
        (!problems.is_empty()).then(|| problems.join("\n"))
    })
}
