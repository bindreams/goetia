//! `goetia-shim`: the Windows service host `type: simple` daemons run
//! under. SCM has no working-directory, environment, or stdout-capture
//! field of its own (`backend::scm::generate`'s mapping table calls these
//! cells "shim spawns child"/"shim sets it"/"shim writes"), so for
//! `type: simple` `ScmRegistration::executable` always names this binary
//! (`goetia-shim.exe <id>`, `backend::scm::generate::registration`'s
//! `Kind::Simple` arm) — never the daemon's own command. This binary reads
//! that daemon's real `DaemonSpec` back out of the same
//! `Services\<id>\Parameters` metadata blob the SCM backend wrote it into
//! (see `service::run`), and does everything a real service host needs:
//! spawn the daemon once, capture its output, report its exit as the service's
//! exit, and kill its whole process tree on `SERVICE_STOP`. It never restarts the
//! daemon: SCM's recovery actions do, each time as a new shim reading the
//! installed spec.
//!
//! **Process-tree management is `cosca`'s job, not hand-rolled here.**
//! `cosca::Command::contain()` is a Windows Job Object with
//! `KILL_ON_JOB_CLOSE`, and `cosca::Child::wait()`/`wait_tree()` are real,
//! event-driven kernel waits. See `service.rs`'s own module doc comment for
//! the one piece `cosca` does not supply: interrupting a blocking
//! `child.wait()` when SCM delivers `Stop`, which `stop_bus` provides.

// In test mode the bin's `main` becomes a skuld test runner; the regular
// entry point below is dead code in that build — mirrors `src/main.rs`.
#![cfg_attr(test, allow(dead_code))]

#[cfg(windows)]
mod logging;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod stop_bus;
#[cfg(all(test, windows))]
mod test_support;

#[cfg(not(test))]
fn main() {
    #[cfg(windows)]
    service::run();

    #[cfg(not(windows))]
    {
        eprintln!("goetia-shim is Windows-only");
        std::process::exit(1);
    }
}

#[cfg(test)]
fn main() {
    skuld::run_all();
}
