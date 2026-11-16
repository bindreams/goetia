//! The one place a listing becomes "running". Task Scheduler leaves an ended run counted in its
//! listing for a few milliseconds, so a listing is never proof that a run is alive: the process
//! is. The listing's pid is the process held by Task Scheduler's own handle while it lists it, so
//! asking the process about that pid is asking about the run.

use std::io;

use super::api::Instance;
use crate::backend::scm::kill::system::is_running;

/// What a probe says of the process of one listed instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Liveness {
    Alive,
    /// No such process, or its object is signalled: the run has ended.
    Gone,
}

/// The instances of `listed` whose process `alive` says is [`Liveness::Alive`], in the listing's
/// order: there can be more than one, for an instant, when a start is accepted just before the
/// first run's process exits.
///
/// A probe that fails is the error and no instance is judged alive by it. Every verb that asks
/// whether a task runs goes through this, so none reads the listing alone.
pub(super) fn live_instances(
    listed: Vec<Instance>,
    mut alive: impl FnMut(u32) -> io::Result<Liveness>,
) -> io::Result<Vec<Instance>> {
    let mut live = Vec::new();
    for instance in listed {
        if alive(instance.pid)? == Liveness::Alive {
            live.push(instance);
        }
    }
    Ok(live)
}

/// The probe of a real listing: the process behind `pid`, opened and asked whether it has exited.
pub(super) fn probe(pid: u32) -> io::Result<Liveness> {
    is_running(pid).map(|running| if running { Liveness::Alive } else { Liveness::Gone })
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod live_tests;
