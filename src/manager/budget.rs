//! `Budget`/`Deadline`: the wait vocabulary shared by every backend's
//! `start`/`stop` wait path. `Budget` is how long a caller is willing to
//! wait; `Deadline` is the absolute instant [`Budget::start`] derives from
//! it. Pure Rust, no platform code — see the crate-level design notes on
//! `daemon start --timeout`.

use std::time::{Duration, Instant};

// Budget ==============================================================================================================

/// How long a caller is willing to wait for a service manager to report a
/// service running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Do not wait at all.
    Immediate,
    /// Wait up to this long.
    Bounded(Duration),
    /// Wait indefinitely.
    Unbounded,
}

impl Budget {
    pub const DEFAULT: Budget = Budget::Bounded(Duration::from_secs(10));

    /// Whether this budget permits any waiting at all. `false` for
    /// `Immediate` and for `Bounded(Duration::ZERO)` alike — a zero-length
    /// bound is not a fourth state, it behaves exactly as `Immediate` does
    /// everywhere a caller branches on this. See the module's design notes
    /// on why folding it in here (rather than leaving it to every call
    /// site) is load-bearing.
    pub fn waits(self) -> bool {
        !matches!(self, Budget::Immediate | Budget::Bounded(Duration::ZERO))
    }

    /// Start the clock: the absolute [`Deadline`] this budget resolves to
    /// starting now.
    ///
    /// `Immediate` yields an already-expired deadline (`Some(Instant::now())`,
    /// never `now - something` — `Instant` subtraction can panic) so a
    /// caller that forgets to branch on [`Self::waits`] fails closed rather
    /// than waiting. `Bounded` uses `checked_add`: `Instant + Duration`
    /// panics on overflow, and a duration too large for the platform's
    /// clock to represent is indistinguishable from `Unbounded`, so that is
    /// what it becomes.
    pub fn start(self) -> Deadline {
        self.start_at(Instant::now())
    }

    /// [`Self::start`] as of `now`, so a test can hold the instant fixed.
    fn start_at(self, now: Instant) -> Deadline {
        match self {
            Budget::Immediate => Deadline(Some(now)),
            Budget::Bounded(d) => Deadline(now.checked_add(d)),
            Budget::Unbounded => Deadline(None),
        }
    }
}

/// The budget a *later* leg of a multi-step operation gets from a deadline
/// the earlier legs have been spending. `Deadline(None)` (unbounded) stays
/// `Unbounded`; a spent deadline becomes `Immediate`; otherwise
/// `Bounded(remaining)`. Normalises the zero case at the one place it is
/// manufactured, so it agrees with [`Budget::waits`] by construction rather
/// than by coincidence.
///
/// `Immediate` means "do not wait", which a bounded budget that ran out is
/// not: a caller that must tell the two apart compares against the budget it
/// started from, as `cli::restart`'s `leg` does.
pub fn budget_for(deadline: Deadline) -> Budget {
    budget_for_at(deadline, Instant::now())
}

/// [`budget_for`] as of `now`, so a test can hold the instant fixed.
fn budget_for_at(deadline: Deadline, now: Instant) -> Budget {
    match deadline.remaining_at(now) {
        None => Budget::Unbounded,
        Some(Duration::ZERO) => Budget::Immediate,
        Some(remaining) => Budget::Bounded(remaining),
    }
}

// timed_out ===========================================================================================================

/// The one [`crate::Error::WaitTimeout`] constructor, so all three backends
/// word an expiry identically rather than three times — the analogue of
/// `decide::foreign_recovery`, and for the same reason: one condition, one
/// remedy, no drift.
///
/// `awaited` is `"running"` or `"stopped"`, and nothing else.
///
/// Takes the `Budget` rather than a bare `Duration` because a backend holds
/// the budget, not the original duration, and because only a **non-zero**
/// `Bounded` budget can expire at all: `Immediate` and `Bounded(ZERO)` never
/// waited (see [`Budget::waits`]), and `Unbounded` never ends. Reaching here
/// with any of those is a backend reporting an expiry that provably cannot
/// have happened, so it is a `debug_assert!` rather than a silently plausible
/// message.
pub fn timed_out(id: &str, awaited: &'static str, budget: Budget) -> crate::Error {
    debug_assert!(
        matches!(budget, Budget::Bounded(d) if d > Duration::ZERO),
        "only a non-zero Bounded budget can expire, got {budget:?}"
    );
    debug_assert!(
        matches!(awaited, "running" | "stopped"),
        "awaited is `running` or `stopped`, got {awaited:?}"
    );
    crate::Error::WaitTimeout {
        id: id.to_string(),
        awaited,
        waited: match budget {
            Budget::Bounded(d) => d,
            Budget::Immediate | Budget::Unbounded => Duration::ZERO,
        },
        recovery: format!(
            "goetia stopped waiting. The request was not cancelled, so `{id}` may still reach \
             {awaited}; `goetia daemon status {id}` shows the manager's current view. Allow longer \
             with `--timeout <duration>`, or wait indefinitely with `--no-timeout`."
        ),
    }
}

// Deadline ============================================================================================================

/// An absolute instant a [`Budget`] resolves to via [`Budget::start`].
/// `None` means unbounded — never expires.
// `PartialEq` is exercised by `scm::wait_tests::a_pending_callback_rearms_under_the_same_deadline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    /// The absolute instant this deadline expires at; `None` = unbounded.
    /// `bounded::wait_bounded` hands it straight to
    /// `cosca::Child::wait_deadline`, which is deadline-native — so nothing
    /// on that path converts to a duration and back, and the unbounded case
    /// is a different call rather than an invented timeout value.
    pub fn at(&self) -> Option<Instant> {
        self.0
    }

    /// Time left until expiry, saturating at `Duration::ZERO` rather than
    /// going negative. `None` = unbounded, never expires.
    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    /// [`Self::remaining`] as of `now`, so a test can hold the instant fixed.
    fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.0.map(|deadline| deadline.saturating_duration_since(now))
    }

    pub fn expired(&self) -> bool {
        self.remaining() == Some(Duration::ZERO)
    }

    /// [`Self::remaining`] in whole milliseconds, rounded up, capped below
    /// `u32::MAX` (which means `INFINITE` to `SleepEx`). `None` = unbounded
    /// — pass `INFINITE` deliberately.
    pub fn remaining_millis_capped(&self) -> Option<u32> {
        let millis = millis_ceil(self.remaining()?);
        debug_assert!(millis < u32::MAX, "must never collide with SleepEx's INFINITE");
        debug_assert!(millis != 0 || self.expired(), "a Some(0) return must imply expired()");
        Some(millis)
    }
}

/// `d` in whole milliseconds, rounded **up**, saturating at `u32::MAX - 1`.
/// Private; the sibling test module tests it directly, because neither end
/// of its range can be reached deterministically through a live `Deadline`.
///
/// Divides first and adjusts after — the obvious `(d + 999_999ns) / 1ms`
/// panics on `Duration::MAX` (`checked_add` is `None` there). This form is
/// total over the whole `Duration` range and needs no saturating
/// construction at all.
fn millis_ceil(d: Duration) -> u32 {
    // `as_millis` truncates; add one back iff anything was truncated. Whole
    // seconds contribute exact milliseconds, so the remainder is entirely
    // in the sub-second part.
    let ceil = d.as_millis() + u128::from(!d.subsec_nanos().is_multiple_of(1_000_000));
    u32::try_from(ceil).unwrap_or(u32::MAX).min(u32::MAX - 1)
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod budget_tests;
