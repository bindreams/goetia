//! The real elevation check, one per platform.
//!
//! `main.rs` wires this into `dispatch`'s `is_elevated`; tests inject their
//! own closure, since a unit test cannot construct an elevated or unelevated
//! process.

/// Whether the current process has root/Administrator privileges.
#[cfg(unix)]
pub fn is_elevated() -> bool {
    // SAFETY: `geteuid` takes no arguments, reads no memory, and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub use crate::elevation::is_elevated;

#[cfg(not(any(unix, windows)))]
pub fn is_elevated() -> bool {
    false
}
