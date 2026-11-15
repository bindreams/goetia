//! What the kernel says of real child processes: nothing here needs root, launchd or a file outside the
//! process's own temporary directory.

use super::*;
use crate::backend::process_watch::process_watch_tests::Held;

/// A live child started at a `mach_absolute_time` between the reads either side of its spawn.
#[skuld::test]
fn a_live_process_started_between_two_clock_reads() {
    let before = now();
    let child = Held::new(0);
    let after = now();

    let started = started(child.pid()).expect("read").expect("a live process");

    assert!(
        before <= started && started <= after,
        "{before} <= {started} <= {after}"
    );
}

#[skuld::test]
fn a_live_process_reads_running_and_a_stopped_one_reads_stopped() {
    let running = Held::new(0);
    let stopped = Held::new(0).stopped();

    assert_eq!(self::stopped(running.pid()).expect("read"), Some(false));
    assert_eq!(self::stopped(stopped.pid()).expect("read"), Some(true));
}

/// A process that has exited and is not reaped is no process to `stopped`, and asking its path is no
/// error: `run` reads the path before the state.
#[skuld::test]
fn an_exited_unreaped_process_is_not_stopped_or_running_and_its_path_is_no_error() {
    let mut child = Held::new(0);
    let pid = child.pid();
    child.exit_unreaped();

    assert_eq!(stopped(pid).expect("read"), None);
    path_of(pid).expect("the path of a zombie is no error");
}

/// A pid that has been reaped is no process, for any read: the answer is not an error.
#[skuld::test]
fn a_reaped_pid_reads_as_none() {
    let mut child = Held::new(0);
    let pid = child.pid();
    child.finish();

    assert_eq!(started(pid).expect("read"), None);
    assert_eq!(stopped(pid).expect("read"), None);
    assert!(path_of(pid).expect("path").is_none());
}

#[skuld::test]
fn the_path_is_the_programs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let named = Held::at_xpcproxy(dir.path(), "read _");
    let plain = Held::new(0);

    let named = path_of(named.pid()).expect("path").expect("a live process");
    let plain = path_of(plain.pid()).expect("path").expect("a live process");

    assert_eq!(named.file_name().and_then(|name| name.to_str()), Some("xpcproxy"));
    // `/bin/sh` is a stub that execs the selected shell: what the process is now is that shell's.
    assert_ne!(plain.file_name().and_then(|name| name.to_str()), Some("xpcproxy"));
}
