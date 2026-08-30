//! Running `systemctl` / `launchctl` / `sc.exe` and keeping the evidence.
//!
//! A probe that fails in CI on a platform nobody can reproduce locally is only
//! useful if the failure carries the command, its exit code, and both streams.

use std::fmt;
use std::process::Command;

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

pub fn run(program: &str, args: &[&str]) -> Run {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn `{program}`: {e}"));
    Run {
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}
