//! What the task suites of the two Unix backends share: a task that is in flight until the test
//! releases it ([`Held`]), the tasks' fixtures, and reading a run's output and a task's status back.

use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use goetia::manager::{Exit, ServiceManager, Status, TaskStatus};
use goetia::spec::{DaemonSpec, Id, TaskSpec};

use super::wait;

pub fn id_of(id: &str) -> Id {
    Id::try_from(id).unwrap_or_else(|e| panic!("`{id}` is not an id: {e}"))
}

pub fn sh(script: &str) -> Vec<String> {
    ["/bin/sh", "-c", script].map(String::from).to_vec()
}

/// `spec` with its output going to `logs`.
pub fn with_logs(spec: DaemonSpec, logs: &Path) -> DaemonSpec {
    let DaemonSpec::Task(task) = spec else {
        panic!("`with_logs` is given a task");
    };
    let mut fields = task.into_fields();
    fields.common.logs = Some(logs.to_path_buf());
    TaskSpec::new(fields).expect("a task with logs").into()
}

pub fn mkfifo(path: &Path) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a path with no NUL");
    // SAFETY: `c_path` is a NUL-terminated pointer valid for the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

/// A task that is in flight until the test releases it. It numbers its run (1, 2, ...), prints `ready`,
/// announces itself and its number on a FIFO and waits for the test to open the other. The announcing
/// FIFO is opened read-write here, so the task's write never waits for the test to read it:
/// [`Held::wait_started`] reads it when it wants to know a run has begun, once per run. Released, the run
/// exits 0.
pub struct Held {
    started: BufReader<File>,
    hold: PathBuf,
    count: PathBuf,
    pub command: Vec<String>,
    _dir: tempfile::TempDir,
}

impl Held {
    pub fn new() -> Held {
        Self::after("")
    }

    /// A run that ignores SIGTERM: only SIGKILL ends it before it is released.
    pub fn ignoring_sigterm() -> Held {
        Self::after("trap '' TERM; ")
    }

    /// The task runs `prelude` before anything else.
    fn after(prelude: &str) -> Held {
        let dir = tempfile::tempdir().expect("tempdir");
        let (started, hold) = (dir.path().join("started"), dir.path().join("hold"));
        mkfifo(&started);
        mkfifo(&hold);
        let announce = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&started)
            .expect("open the announcing FIFO");
        // The count is replaced whole (a rename), so a read of it never meets a half-written one.
        let count = dir.path().join("count");
        let script = format!(
            "{prelude}n=$(cat \"{c}\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > \"{c}.new\"; \
             mv \"{c}.new\" \"{c}\"; echo ready; echo started $n > \"{}\"; read line < \"{}\"; exit 0",
            started.display(),
            hold.display(),
            c = count.display()
        );
        Held {
            started: BufReader::new(announce),
            hold,
            count,
            command: sh(&script),
            _dir: dir,
        }
    }

    /// Blocks until the next run has printed `ready`, and returns its number. A run that never starts
    /// hangs the test.
    pub fn wait_started(&mut self) -> u32 {
        let mut line = String::new();
        self.started.read_line(&mut line).expect("read the announcing FIFO");
        line.strip_prefix("started ")
            .and_then(|n| n.trim_end().parse().ok())
            .unwrap_or_else(|| panic!("the announcing FIFO held `{line}`"))
    }

    /// The number of the newest run that has numbered itself: 0 before any. A run that began before the
    /// call and has not numbered itself yet is not counted.
    pub fn begun(&self) -> u32 {
        match std::fs::read_to_string(&self.count) {
            Ok(text) => text.trim().parse().expect("the count is whole"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => panic!("read the run count: {e}"),
        }
    }

    /// Lets the run in flight end. Blocks until the run is waiting for it, so only call it while one is.
    pub fn release(&self) {
        drop(
            OpenOptions::new()
                .write(true)
                .open(&self.hold)
                .expect("open the hold FIFO"),
        );
    }
}

/// An `io::Write` that sends what it is given down a channel, for a `run` on a thread whose output the
/// test reads as it comes.
pub struct Lines(pub mpsc::Sender<Vec<u8>>);

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(std::io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn task_status(mgr: &dyn ServiceManager, id: &Id) -> TaskStatus {
    match mgr.status(id).expect("status") {
        Status::Task(status) => status,
        other => panic!("a task's status is a task's: {other:?}"),
    }
}

/// The first status of `id` that `ready` accepts, read again until then with no cap: the state `ready`
/// names ends the wait, and a state that never comes hangs the test (unless `ready` panics on it). A
/// read is several processes, so the turns are paced.
pub fn status_until(mgr: &dyn ServiceManager, id: &Id, mut ready: impl FnMut(&TaskStatus) -> bool) -> TaskStatus {
    let mut found = None;
    wait::until_paced(Duration::from_millis(100), || {
        let status = task_status(mgr, id);
        let done = ready(&status);
        found = Some(status);
        done
    });
    found.expect("a status was read")
}

/// The status of `id` once its newest run has a result.
pub fn status_with_result(mgr: &dyn ServiceManager, id: &Id) -> TaskStatus {
    status_until(mgr, id, |status| status.last_result.is_some())
}

/// What `run` shows, and how it ended.
pub fn run_task(mgr: &dyn ServiceManager, id: &Id) -> (Exit, Vec<u8>) {
    let mut out = Vec::new();
    let exit = mgr
        .run(id, &mut out)
        .unwrap_or_else(|e| panic!("run {id}: {}", e.report()));
    (exit, out)
}

/// A diff small enough to read: the first line where `got` and `want` differ.
pub fn assert_same_text(run: usize, got: &[u8], want: &[u8]) {
    if got == want {
        return;
    }
    let (got, want) = (String::from_utf8_lossy(got), String::from_utf8_lossy(want));
    let (mut got_lines, mut want_lines) = (got.lines(), want.lines());
    let mut line = 1;
    loop {
        match (got_lines.next(), want_lines.next()) {
            (a, b) if a == b => line += 1,
            (a, b) => panic!(
                "run {run}: the output differs at line {line}: got {a:?}, want {b:?} ({} lines against {})",
                got.lines().count(),
                want.lines().count()
            ),
        }
    }
}
