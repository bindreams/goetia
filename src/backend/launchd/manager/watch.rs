//! The process watch of [`process_watch`], joined to launchd's interrupt window: a [`Deferred`] is the
//! window a watch here can be woken by.

use std::os::fd::RawFd;

use super::interrupts::{Deferred, Interrupts};
use crate::backend::process_watch::{self, Window};

pub(super) type Watch = process_watch::Watch<Deferred>;
pub(super) type Registered = process_watch::Registered<Deferred>;
pub(super) type Added = process_watch::Added<Deferred>;
pub(super) type Woke = process_watch::Woke<Interrupts>;

impl Window for Deferred {
    type Interrupts = Interrupts;

    fn join(&self) -> Deferred {
        Deferred::join(self)
    }

    fn wake(&self) -> RawFd {
        Deferred::wake(self)
    }

    fn recorded(&self) -> Option<Interrupts> {
        Deferred::recorded(self)
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod watch_tests;
