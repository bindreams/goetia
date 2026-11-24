//! The pipe a window's handler writes to and a wait reads.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::fcntl::{FcntlArg, OFlag, fcntl};

/// A pipe with both ends non-blocking and close-on-exec. The ends are private to this module: neither can
/// be moved out and closed on its own, so they close together, when this drops, in field order (read, then
/// write, then the test's mark).
pub(super) struct Wake {
    read: OwnedFd,
    write: OwnedFd,
    /// Drops after both ends are closed, and says so to the probe: for the test of the close's order. The
    /// order is the fields' declaration order.
    #[cfg(test)]
    closed: super::test_hook::CloseMark,
}

impl Wake {
    pub(super) fn new() -> io::Result<Wake> {
        #[cfg(test)]
        super::test_hook::step()?;
        // Apple has no `pipe2`, so `std` sets close-on-exec on each end after the pipe is made, not with
        // it. A program started in between would inherit the ends, but nothing in goetia starts one while
        // a window opens: `run` starts its child from its own thread, in sequence after the window.
        let (read, write) = io::pipe()?;
        let (read, write): (OwnedFd, OwnedFd) = (read.into(), write.into());
        for end in [&read, &write] {
            let flags = OFlag::from_bits_truncate(fcntl(end, FcntlArg::F_GETFL).map_err(io::Error::from)?);
            fcntl(end, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
        }
        Ok(Wake {
            read,
            write,
            #[cfg(test)]
            closed: Default::default(),
        })
    }

    /// The read end; valid while `self` lives.
    pub(super) fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// The write end; valid while `self` lives.
    pub(super) fn write_fd(&self) -> RawFd {
        self.write.as_raw_fd()
    }

    /// Records how this pipe's close went.
    #[cfg(test)]
    pub(super) fn probe(&self) -> &std::sync::Arc<super::test_hook::Probe> {
        &self.closed.probe
    }
}
