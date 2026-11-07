//! The process list against a process the test starts itself: a copy of `ping.exe` under a shim
//! file name, which runs until it is killed.

use std::path::{Path, PathBuf};

use cosca::Stdio;

use super::super::sweep::normalise;
use super::*;
use crate::backend::scm::kill::system::{has_exited, open_process_with, wait_exit};

const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A running copy of `ping.exe` named like a shim, in a temp dir that outlives it.
struct Running {
    // Declared first, so dropped first: the child is killed before its file is removed.
    child: cosca::Child,
    exe: PathBuf,
    _dir: tempfile::TempDir,
}

fn run_a_copy() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(format!("goetia-shim-{HEX}.exe"));
    let system = std::env::var_os("SystemRoot").expect("SystemRoot");
    std::fs::copy(Path::new(&system).join(r"System32\ping.exe"), &exe).unwrap();
    let mut command = cosca::run([exe.as_os_str(), "-t".as_ref(), "127.0.0.1".as_ref()]);
    command.stdin(Stdio::null()).unwrap();
    command.stdout(Stdio::null()).unwrap();
    command.stderr(Stdio::null()).unwrap();
    let child = command.spawn().expect("start the copy");
    Running { child, exe, _dir: dir }
}

#[skuld::test]
fn running_images_lists_a_running_copy() {
    let running = run_a_copy();

    let images = running_images().expect("list the running images");

    assert!(
        images.contains(&normalise(&running.exe)),
        "{} is not among {} images",
        running.exe.display(),
        images.len()
    );
}

/// A process that has exited, though a handle to it is still held, references nothing.
#[skuld::test]
fn a_process_that_has_exited_is_not_an_image() {
    let running = run_a_copy();
    let held = open_process_with(running.child.id().pid(), ACCESS)
        .expect("open the copy")
        .expect("the copy is running");
    running.child.kill().unwrap();
    wait_exit(&held, || {}).expect("wait for the copy to exit");
    assert!(has_exited(&held).unwrap());

    let images = running_images().expect("list the running images");

    assert!(!images.contains(&normalise(&running.exe)));
}
