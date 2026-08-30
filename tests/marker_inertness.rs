//! Empirical probe of the three metadata embed sites.
//!
//! Goetia embeds a base64 spec blob in the artifact it writes, at a site each
//! platform is supposed to ignore. Only systemd documents its site as inert
//! (`X-`-prefixed sections); launchd's tolerance of our plist and Windows'
//! preservation of unknown `Parameters` values rest on the absence of
//! documentation saying otherwise. This suite establishes the truth before
//! three backends are built on it.
//!
//! Nothing here consumes Goetia: the artifacts are hand-written and driven
//! through raw `systemctl` / `launchctl` / `sc.exe`, so the probe runs before
//! any backend exists.

#[path = "support/mod.rs"]
mod support;

#[cfg(target_os = "macos")]
#[path = "marker_inertness/launchd.rs"]
mod launchd;
#[cfg(windows)]
#[path = "marker_inertness/scm.rs"]
mod scm;
#[cfg(target_os = "linux")]
#[path = "marker_inertness/systemd.rs"]
mod systemd;

fn main() {
    // The probes need a real daemon to observe, and the only binary they can
    // be sure exists on the host is this one. Both sentinel modes must be
    // dispatched before the harness starts, because in those modes the process
    // was launched by launchd or the SCM, not by a test runner.
    if support::sentinel::run_if_requested() {
        return;
    }
    skuld::run_all();
}
