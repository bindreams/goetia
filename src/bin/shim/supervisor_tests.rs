use std::time::Duration;

use goetia::spec::Restart;

use super::*;

#[skuld::test]
fn never_does_not_respawn() {
    for outcome in [
        ChildOutcome::Exited(0),
        ChildOutcome::Exited(1),
        ChildOutcome::SpawnFailed,
    ] {
        let decision = decide_restart(Restart::Never, outcome, false, None);
        assert_eq!(decision, RestartDecision::Stop, "outcome={outcome:?}");
    }
}

#[skuld::test]
fn on_failure_respawns_only_on_nonzero_exit() {
    let clean = decide_restart(Restart::OnFailure, ChildOutcome::Exited(0), false, None);
    assert_eq!(clean, RestartDecision::Stop);

    let failed = decide_restart(Restart::OnFailure, ChildOutcome::Exited(1), false, None);
    assert_eq!(
        failed,
        RestartDecision::Respawn {
            delay: DEFAULT_RESTART_DELAY
        }
    );

    let spawn_failed = decide_restart(Restart::OnFailure, ChildOutcome::SpawnFailed, false, None);
    assert_eq!(
        spawn_failed,
        RestartDecision::Respawn {
            delay: DEFAULT_RESTART_DELAY
        }
    );
}

#[skuld::test]
fn always_respawns_on_clean_exit() {
    let decision = decide_restart(Restart::Always, ChildOutcome::Exited(0), false, None);
    assert_eq!(
        decision,
        RestartDecision::Respawn {
            delay: DEFAULT_RESTART_DELAY
        }
    );
}

#[skuld::test]
fn commanded_stop_suppresses_respawn() {
    // `restart: always` plus a nonzero exit is the combination that would
    // respawn under every other input in this table — `stopping: true`
    // must override it regardless.
    let decision = decide_restart(
        Restart::Always,
        ChildOutcome::Exited(1),
        true,
        Some(Duration::from_secs(5)),
    );
    assert_eq!(decision, RestartDecision::Stop);
}

#[skuld::test]
fn missing_restart_delay_uses_the_documented_default() {
    let decision = decide_restart(Restart::Always, ChildOutcome::Exited(0), false, None);
    assert_eq!(
        decision,
        RestartDecision::Respawn {
            delay: DEFAULT_RESTART_DELAY
        }
    );

    // A configured delay is honored rather than overridden.
    let configured = decide_restart(
        Restart::Always,
        ChildOutcome::Exited(0),
        false,
        Some(Duration::from_secs(7)),
    );
    assert_eq!(
        configured,
        RestartDecision::Respawn {
            delay: Duration::from_secs(7)
        }
    );
}
