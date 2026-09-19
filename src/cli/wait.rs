//! `--timeout <DURATION>` / `--no-timeout`: the one wait surface `start`,
//! `stop`, `restart` and `install --start` share.
//!
//! Flattened into all four rather than spelled out per verb, so the help
//! text, the default and the mutual exclusion cannot drift: four copies of
//! one flag pair are four chances to publish a different default.

use std::time::Duration;

use clap::Args as ClapArgs;

use crate::manager::Budget;

/// The flag pair every waiting verb flattens in.
///
/// [`Self::timeout`] is an `Option`, never a clap `default_value`: a
/// defaulted argument is "present" as far as clap is concerned, which would
/// make `--timeout` indistinguishable from its absence — and
/// [`dispatch`](super::dispatch)'s `install` refusal needs exactly that
/// distinction (see [`Self::was_given`]). The cost is that clap cannot
/// render `[default: 10s]` for it, so the help text states the default in
/// words.
#[derive(ClapArgs, Debug, Default)]
pub struct WaitArgs {
    /// How long to wait for the manager to report the daemon reached the
    /// state asked for. Defaults to 10s. `--timeout 0` issues the request
    /// and returns without waiting, establishing nothing — except `stop` on
    /// macOS, which runs `launchctl bootout` to completion because launchd
    /// has no request-only stop. Write a compound
    /// duration unspaced (`2m30s`), or quote it as one argument: every verb
    /// taking this flag also takes a variadic id list, so `--timeout 2m 30s`
    /// passes `2m` here and `30s` as a daemon id.
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
            // Not a fourth state: [`Budget::waits`] already folds a
            // zero-length bound in with `Immediate`, so normalising it here
            // means one spelling of "do not wait" reaches the backends.
            Some(Duration::ZERO) => Budget::Immediate,
            Some(d) => Budget::Bounded(d),
        }
    }

    /// Whether either flag was actually given — which is a different
    /// question from what [`Self::budget`] returns, since the default budget
    /// is what *no* flag means. `install`'s refusal reads this and nothing
    /// else.
    pub fn was_given(&self) -> bool {
        self.no_timeout || self.timeout.is_some()
    }
}

/// The word a verb reports after succeeding under `budget`: `confirmed` when
/// the budget waited, `requested` when it did not. Printing "started" after
/// `--timeout 0` would claim exactly the confirmation the flag declined to
/// obtain.
pub(crate) fn reported(budget: Budget, confirmed: &'static str, requested: &'static str) -> &'static str {
    if budget.waits() { confirmed } else { requested }
}

/// `--timeout`'s value parser: [`humantime::parse_duration`], the same
/// grammar `restart-delay` already accepts in the manifest, so goetia has one
/// duration syntax rather than two.
///
/// A bare `0` is `humantime`'s own special case and needs no carve-out here.
/// `1` is rejected for naming no unit, which is the right answer to an
/// ambiguous number.
pub fn parse_timeout(raw: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(raw)
        .map_err(|source| format!("`{raw}` is not a duration (e.g. `30s`, `2m30s`): {source}"))
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod wait_tests;
