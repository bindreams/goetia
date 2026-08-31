use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use crate::blob::{self, Blob};
use crate::spec::{DaemonSpec, Id, Kind, Restart, User};

// Fixtures ============================================================================================================

// `DaemonSpec` is built literally here, never via `resolve()`: a resolved
// path renders with `\` on Windows and `/` elsewhere, which would make
// text-level assertions platform-dependent. Every path below is a single
// component for the same reason (see `diff_tests.rs`).
fn spec_fixture() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "FRP tunnel client".to_owned(),
        command: vec!["frpc".to_owned(), "-c".to_owned(), "frpc.toml".to_owned()],
        cwd: Some(PathBuf::from("app")),
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: Some(PathBuf::from("frpc.log")),
        kind: Kind::Simple,
    }
}

fn blob_for(spec: &DaemonSpec, version: &str) -> Blob {
    Blob {
        schema: blob::SCHEMA,
        version: version.to_owned(),
        spec: spec.clone(),
    }
}

const RUNNING_VERSION: &str = "1.2.3";

// decide ==============================================================================================================

#[skuld::test]
fn absent_creates() {
    let spec = spec_fixture();

    let outcome = decide(&Ownership::Absent, None, "GENERATED", &spec, RUNNING_VERSION, false);

    assert_eq!(outcome, Outcome::Create);
}

#[skuld::test]
fn identical_is_up_to_date() {
    let spec = spec_fixture();
    let blob = blob_for(&spec, RUNNING_VERSION);
    let artifact = "GENERATED ARTIFACT TEXT";

    let outcome = decide(
        &Ownership::Ours(blob),
        Some(artifact),
        artifact,
        &spec,
        RUNNING_VERSION,
        false,
    );

    assert_eq!(outcome, Outcome::UpToDate);
}

#[skuld::test]
fn spec_change_updates() {
    let embedded = spec_fixture();
    let mut new_spec = spec_fixture();
    new_spec.restart = Restart::Always;
    let blob = blob_for(&embedded, RUNNING_VERSION);

    let outcome = decide(
        &Ownership::Ours(blob),
        Some("OLD ARTIFACT TEXT"),
        "NEW ARTIFACT TEXT",
        &new_spec,
        RUNNING_VERSION,
        false,
    );

    match outcome {
        Outcome::Update { spec_diff } => {
            assert!(
                spec_diff.contains("restart"),
                "update diff should name the changed field: {spec_diff}"
            );
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

#[skuld::test]
fn hand_edit_conflicts() {
    let spec = spec_fixture();
    let blob = blob_for(&spec, RUNNING_VERSION);
    // A hand-added directive the spec cannot express: the embedded spec is
    // unchanged, so this can only be a hand-edit, not a spec change.
    let on_disk = "GENERATED\nMemoryMax=8G\n";
    let desired = "GENERATED\n";

    let outcome = decide(
        &Ownership::Ours(blob),
        Some(on_disk),
        desired,
        &spec,
        RUNNING_VERSION,
        false,
    );

    match outcome {
        Outcome::Conflict { artifact_diff } => {
            assert!(
                artifact_diff.contains("MemoryMax=8G"),
                "conflict diff should show the hand-edit: {artifact_diff}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[skuld::test]
fn hand_edit_with_force_updates() {
    let spec = spec_fixture();
    let blob = blob_for(&spec, RUNNING_VERSION);
    let on_disk = "GENERATED\nMemoryMax=8G\n";
    let desired = "GENERATED\n";

    let outcome = decide(
        &Ownership::Ours(blob),
        Some(on_disk),
        desired,
        &spec,
        RUNNING_VERSION,
        true,
    );

    assert!(
        matches!(outcome, Outcome::Update { .. }),
        "force should overwrite a hand-edit rather than refuse: {outcome:?}"
    );
}

#[skuld::test]
fn foreign_refuses_even_with_force() {
    let spec = spec_fixture();

    for force in [false, true] {
        let outcome = decide(
            &Ownership::Foreign,
            Some("some pre-existing, unrelated service"),
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            force,
        );

        match outcome {
            Outcome::RefuseForeign { recovery } => {
                assert!(!recovery.is_empty(), "recovery must be non-empty (force={force})");
            }
            other => panic!("expected RefuseForeign regardless of force={force}, got {other:?}"),
        }
    }
}

#[skuld::test]
fn unreadable_refuses_with_recovery() {
    let spec = spec_fixture();

    for force in [false, true] {
        let found = Ownership::OursUnreadable {
            reason: "schema 2 is not understood by this build".to_owned(),
        };

        let outcome = decide(
            &found,
            Some("some artifact"),
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            force,
        );

        match outcome {
            Outcome::RefuseUnreadable { reason, recovery } => {
                assert_eq!(reason, "schema 2 is not understood by this build");
                assert!(!recovery.is_empty(), "recovery must be non-empty (force={force})");
            }
            other => panic!("expected RefuseUnreadable regardless of force={force}, got {other:?}"),
        }
    }
}

#[skuld::test]
fn older_version_is_stale_not_conflict() {
    let spec = spec_fixture();
    // The spec is unchanged: on its own, that would route into the
    // hand-edit/conflict check. The version mismatch must be checked
    // first and win outright.
    let blob = blob_for(&spec, "0.1.0");
    let on_disk = "ARTIFACT WRITTEN BY 0.1.0";
    let desired = "ARTIFACT AS 1.2.3 WOULD WRITE IT";

    let outcome = decide(
        &Ownership::Ours(blob),
        Some(on_disk),
        desired,
        &spec,
        RUNNING_VERSION,
        false,
    );

    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: "0.1.0".to_owned()
        }
    );
}

#[skuld::test]
fn every_refusal_names_a_recovery_command() {
    let spec = spec_fixture();
    let refusals = [
        decide(
            &Ownership::Foreign,
            Some("x"),
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            false,
        ),
        decide(
            &Ownership::OursUnreadable {
                reason: "corrupt".to_owned(),
            },
            Some("x"),
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            false,
        ),
    ];

    for outcome in refusals {
        match outcome {
            Outcome::RefuseForeign { recovery } | Outcome::RefuseUnreadable { recovery, .. } => {
                assert!(!recovery.is_empty(), "every refusal must name a recovery command");
            }
            other => panic!("expected a refusing variant, got {other:?}"),
        }
    }
}
