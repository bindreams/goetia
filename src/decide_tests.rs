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

/// Builds the `Ours` ownership a real backend would report: the decoded
/// blob, plus `regenerated` -- what that backend's own generator produces
/// from the *embedded* spec (`generate(blob.spec)`), computed by the
/// caller, never by `decide` itself.
fn ours(spec: &DaemonSpec, version: &str, regenerated: &str) -> Ownership {
    Ownership::Ours {
        blob: blob_for(spec, version),
        regenerated: regenerated.to_owned(),
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
    let artifact = "GENERATED ARTIFACT TEXT";
    let found = ours(&spec, RUNNING_VERSION, artifact);

    let outcome = decide(&found, Some(artifact), artifact, &spec, RUNNING_VERSION, false);

    assert_eq!(outcome, Outcome::UpToDate);
}

/// A clean spec change: the artifact on disk is exactly what the embedded
/// spec regenerates to (no hand-edit), but `new_spec` differs from the
/// embedded spec. Pinned apart from `spec_change_with_hand_edit_conflicts`
/// below, which looks identical except `on_disk` is *not*
/// `regenerated` -- that's the row `decide` could not previously reach.
#[skuld::test]
fn spec_change_updates() {
    let embedded = spec_fixture();
    let mut new_spec = spec_fixture();
    new_spec.restart = Restart::Always;
    let regenerated = "ARTIFACT FROM EMBEDDED SPEC";
    let found = ours(&embedded, RUNNING_VERSION, regenerated);

    let outcome = decide(
        &found,
        Some(regenerated),
        "ARTIFACT FROM NEW SPEC",
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
    let regenerated = "GENERATED\n";
    let found = ours(&spec, RUNNING_VERSION, regenerated);
    // A hand-added directive the spec cannot express: the embedded spec is
    // unchanged, so this can only be a hand-edit, not a spec change.
    let on_disk = "GENERATED\nMemoryMax=8G\n";
    let desired = "GENERATED\n";

    let outcome = decide(&found, Some(on_disk), desired, &spec, RUNNING_VERSION, false);

    match outcome {
        Outcome::Conflict { artifact_diff } => {
            // Direction matters, so assert it: the diff runs from what
            // goetia last wrote to what is on disk now, so the admin's
            // hand-edit reads as an ADDITION. Merely asserting the line
            // appears somewhere would pass in either direction and would
            // not pin the behaviour.
            assert!(
                artifact_diff.contains("+MemoryMax=8G"),
                "conflict diff should show the hand-edit as an addition: {artifact_diff}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[skuld::test]
fn hand_edit_with_force_updates() {
    let spec = spec_fixture();
    let regenerated = "GENERATED\n";
    let found = ours(&spec, RUNNING_VERSION, regenerated);
    let on_disk = "GENERATED\nMemoryMax=8G\n";
    let desired = "GENERATED\n";

    let outcome = decide(&found, Some(on_disk), desired, &spec, RUNNING_VERSION, true);

    assert!(
        matches!(outcome, Outcome::Update { .. }),
        "force should overwrite a hand-edit rather than refuse: {outcome:?}"
    );
}

/// The row `decide` previously could not reach: the spec changed *and* the
/// artifact was hand-edited on top of the old spec. `on_disk` matches
/// neither `desired` (generated from the new spec) nor `regenerated`
/// (generated from the embedded spec) -- it must be a conflict, not a
/// silent overwrite of the hand-edit riding along with the spec change.
#[skuld::test]
fn spec_change_with_hand_edit_conflicts() {
    let embedded = spec_fixture();
    let mut new_spec = spec_fixture();
    new_spec.restart = Restart::Always;
    let regenerated = "ARTIFACT FROM EMBEDDED SPEC\n";
    let found = ours(&embedded, RUNNING_VERSION, regenerated);
    // Hand-edited on top of the old (embedded-spec) artifact: differs from
    // both `regenerated` and `desired`.
    let on_disk = "ARTIFACT FROM EMBEDDED SPEC\nMemoryMax=8G\n";
    let desired = "ARTIFACT FROM NEW SPEC\n";

    let outcome = decide(&found, Some(on_disk), desired, &new_spec, RUNNING_VERSION, false);

    match outcome {
        Outcome::Conflict { artifact_diff } => {
            assert!(
                artifact_diff.contains("MemoryMax=8G"),
                "conflict diff should show the hand-edit: {artifact_diff}"
            );
        }
        other => panic!("expected Conflict even though the spec also changed, got {other:?}"),
    }
}

#[skuld::test]
fn spec_change_with_hand_edit_and_force_updates() {
    let embedded = spec_fixture();
    let mut new_spec = spec_fixture();
    new_spec.restart = Restart::Always;
    let regenerated = "ARTIFACT FROM EMBEDDED SPEC\n";
    let found = ours(&embedded, RUNNING_VERSION, regenerated);
    let on_disk = "ARTIFACT FROM EMBEDDED SPEC\nMemoryMax=8G\n";
    let desired = "ARTIFACT FROM NEW SPEC\n";

    let outcome = decide(&found, Some(on_disk), desired, &new_spec, RUNNING_VERSION, true);

    assert!(
        matches!(outcome, Outcome::Update { .. }),
        "force should overwrite even a hand-edit-plus-spec-change conflict: {outcome:?}"
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
    let on_disk = "ARTIFACT WRITTEN BY 0.1.0";
    let desired = "ARTIFACT AS 1.2.3 WOULD WRITE IT";
    let found = ours(&spec, "0.1.0", on_disk);

    let outcome = decide(&found, Some(on_disk), desired, &spec, RUNNING_VERSION, false);

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
