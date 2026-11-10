//! `--timeout <DURATION>` / `--no-timeout`: the one wait surface `start`,
//! `stop`, `restart` and `install --start` share.
//!
//! Flattened into all four so the help text, the default and the mutual
//! exclusion cannot drift.

use std::time::Duration;

use clap::Args as ClapArgs;

use crate::manager::Budget;

/// The flag pair every waiting verb flattens in.
///
/// [`Self::timeout`] is an `Option`, never a clap `default_value`: a defaulted
/// argument counts as present, which would make `--timeout` indistinguishable
/// from its absence, and [`dispatch`](super::dispatch)'s `install` refusal
/// needs that distinction (see [`Self::was_given`]). Clap therefore cannot
/// render `[default: 10s]`, so the help text states the default in words.
#[derive(ClapArgs, Debug, Default)]
pub struct WaitArgs {
    /// How long to wait for the manager to report the daemon reached the
    /// state asked for. Defaults to 10s. `--timeout 0` issues the request
    /// and returns without waiting, establishing nothing — except `stop` on
    /// macOS, which waits until launchd no longer has the job; and a
    /// `restart: always` `start` on macOS that finds the label held by a job
    /// that is not running boots it out and waits until launchd has removed
    /// it, which `--timeout` does not cut short (for a job whose plist
    /// disables `ExitTimeOut` that may not happen); and a `start` on macOS over
    /// a job still being torn down, which waits for it whatever `--timeout` says,
    /// and spends the budget doing so. Write a compound
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
            // [`Budget::waits`] already folds a zero-length bound in with
            // `Immediate`; normalising here sends the backends one spelling of
            // "do not wait".
            Some(Duration::ZERO) => Budget::Immediate,
            Some(d) => Budget::Bounded(d),
        }
    }

    /// Whether either flag was actually given, which [`Self::budget`] cannot
    /// say: the default budget is what *no* flag means.
    pub fn was_given(&self) -> bool {
        self.no_timeout || self.timeout.is_some()
    }
}

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
        .map_err(|source| format!("`{raw}` is not a duration (e.g. `30s`, `2m30s`): {source}"))
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod wait_tests;
