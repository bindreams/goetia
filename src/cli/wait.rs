//! `--timeout <DURATION>` / `--no-timeout`: the one wait surface `start`,
//! `stop`, `restart` and `install --start` share.
//!
//! Flattened into all four so the help text, the default and the mutual
//! exclusion cannot drift.

use std::time::Duration;

use clap::Args as ClapArgs;

use crate::error::{Chain, Result};
use crate::manager::Budget;
use crate::spec::{StopAction, StopSignal};

/// The flag pair every waiting verb flattens in.
///
/// [`Self::timeout`] is an `Option`, never a clap `default_value`: a defaulted
/// argument counts as present, which would make `--timeout` indistinguishable
/// from its absence, and [`dispatch`](super::dispatch)'s `install` refusal
/// needs that distinction (see [`Self::was_given`]). Clap therefore cannot
/// render `[default: 10s]`, so the help text states the default in words, and `stop` and
/// `restart` take theirs from the daemon ([`Self::stop_budget`]).
#[derive(ClapArgs, Debug, Default)]
pub struct WaitArgs {
    /// How long to wait for the manager to report the daemon reached the
    /// state asked for. Defaults to 10s; for `stop` and `restart`, the daemon's own stop timeout
    /// plus 10s (see `stop-action` in the README). `--timeout 0` issues the request
    /// and returns without waiting, establishing nothing. On macOS, a stop and
    /// some starts and installs also wait for launchd to remove a job; see the
    /// README. Write a compound duration unspaced (`2m30s`), or quote it as
    /// one argument: every verb taking this flag also takes a variadic id
    /// list, so `--timeout 2m 30s` passes `2m` here and `30s` as a daemon id.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    pub timeout: Option<Duration>,
    /// Wait indefinitely instead of giving up at a bound.
    #[arg(long, conflicts_with = "timeout")]
    pub no_timeout: bool,
}

impl WaitArgs {
    /// The [`Budget`] these flags name.
    pub fn budget(&self) -> Budget {
        if self.no_timeout {
            return Budget::Unbounded;
        }
        match self.timeout {
            None => Budget::DEFAULT,
            // [`Budget::waits`] already folds a zero-length bound in with
            // `Immediate`; normalising here sends the backends one spelling of
            // "do not wait".
            Some(Duration::ZERO) => Budget::Immediate,
            Some(d) => Budget::Bounded(d),
        }
    }

    /// [`Self::budget`] for a verb that stops a daemon (`stop`, `restart`), whose default wait is the
    /// daemon's own: its stop timeout plus the teardown margin, unbounded for no timeout, and what it
    /// always was ([`Budget::DEFAULT`]) for `kill`, which sends nothing to answer. `stop_action` reads it,
    /// and is not asked when a flag was given. A daemon whose stop action cannot be read (not installed,
    /// not goetia's, a definition that will not decode) has none to wait for, and the previous default
    /// stands: the verb meets the same gate itself and reports it, as it always did, and the order in
    /// which it does is unchanged.
    pub fn stop_budget(&self, stop_action: impl FnOnce() -> Result<StopAction>) -> Budget {
        if self.was_given() {
            return self.budget();
        }
        stop_action().map_or(Budget::DEFAULT, |stop_action| default_stop_budget(&stop_action))
    }

    /// Whether either flag was actually given, which [`Self::budget`] cannot
    /// say: the default budget is what *no* flag means.
    pub fn was_given(&self) -> bool {
        self.no_timeout || self.timeout.is_some()
    }
}

/// The default wait of a verb that stops a daemon with `stop_action`: see [`WaitArgs::stop_budget`].
fn default_stop_budget(stop_action: &StopAction) -> Budget {
    if stop_action.send_signal == StopSignal::Kill {
        return Budget::DEFAULT;
    }
    match stop_action.longest_stop() {
        Some(longest) => Budget::Bounded(longest),
        None => Budget::Unbounded,
    }
}

/// Why a wait flag is refused for a task's `start`: the request is all it does.
pub(crate) const NOT_FOR_A_TASK: &str = "`--timeout` and `--no-timeout` do not apply to a task: `start` returns as soon as the manager has accepted the run";

/// The word a verb reports after succeeding under `budget`: `confirmed` when
/// the budget waited, `requested` when it did not: "started" after
/// `--timeout 0` would claim a confirmation the flag declined to obtain.
pub(crate) fn reported(budget: Budget, confirmed: &'static str, requested: &'static str) -> &'static str {
    if budget.waits() { confirmed } else { requested }
}

/// `--timeout`'s value parser: [`humantime::parse_duration`], the same
/// grammar `restart-delay` already accepts in the manifest, so goetia has one
/// duration syntax rather than two.
///
/// A bare `0` is `humantime`'s own special case; any other bare number is
/// rejected for naming no unit.
pub fn parse_timeout(raw: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(raw)
        .map_err(|source| format!("`{raw}` is not a duration (e.g. `30s`, `2m30s`): {}", Chain(&source)))
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod wait_tests;
