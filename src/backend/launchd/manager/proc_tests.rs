//! What the kernel says of real child processes: nothing here needs root, launchd or a file outside the
//! process's own temporary directory.

use super::super::watch::watch_tests::Held;
use super::*;

/// A live child is not stopped, and its start time is a `mach_absolute_time` between the reads either side
/// of its spawn.
#[skuld::test]
fn a_live_process_reads_running_and_started_between_two_clock_reads() {
    let before = now();
    let child = Held::new(0);
    let after = now();

    let process = read(child.pid()).expect("read").expect("a live process");

    assert!(!process.stopped);
    assert!(
        before <= process.started && process.started <= after,
        "{before} <= {} <= {after}",
        process.started
    );
}

#[skuld::test]
fn a_stopped_process_reads_stopped() {
    let child = Held::new(0).stopped();

    let process = read(child.pid()).expect("read").expect("a live process");

    assert!(process.stopped);
}

/// A process that has exited and is not reaped is no process to `read`, and asking its path is no error:
/// `run` reads the path first.
#[skuld::test]
fn an_exited_unreaped_process_reads_as_none_and_its_path_is_no_error() {
    let mut child = Held::new(0);
    let pid = child.pid();
    child.exit_unreaped();

    assert!(read(pid).expect("read").is_none());
    path_of(pid).expect("the path of a zombie is no error");
}

/// A pid that has been reaped is no process, for either read: the answer is not an error.
#[skuld::test]
fn a_reaped_pid_reads_as_none() {
    let mut child = Held::new(0);
    let pid = child.pid();
    child.finish();

    assert!(read(pid).expect("read").is_none());
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
    assert_eq!(plain.file_name().and_then(|name| name.to_str()), Some("sh"));
}
