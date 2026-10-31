use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use crate::blob::{self, Blob};
use crate::spec::{DaemonSpec, Id, Kind, Restart, User};

// Fixtures ============================================================================================================

/// External configuration entirely within the backend's own writable root, which `--force` really
/// does resolve.
fn clearable() -> ExternalConfig {
    ExternalConfig::Clearable
}

/// External configuration with a part the backend's own write will not remove.
fn unclearable() -> ExternalConfig {
    ExternalConfig::Unclearable {
        recovery: "remove /usr/lib/systemd/system/frpc.service.d by hand".to_string(),
    }
}

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
/// blob, `regenerated` -- what that backend's own generator produces
/// from the *embedded* spec (`generate(blob.spec)`), computed by the
/// caller, never by `decide` itself -- and the artifact's `on_disk` text.
fn ours(spec: &DaemonSpec, version: &str, regenerated: &str, on_disk: &str) -> Ownership {
    Ownership::Ours {
        blob: blob_for(spec, version),
        regenerated: regenerated.to_owned(),
        on_disk: on_disk.to_owned(),
    }
}

const RUNNING_VERSION: &str = "1.2.3";

// decide ==============================================================================================================

#[skuld::test]
fn absent_creates() {
    let spec = spec_fixture();

    let outcome = decide(
        &Ownership::Absent,
        "GENERATED",
        &spec,
        RUNNING_VERSION,
        false,
        &ExternalConfig::Absent,
    );

    assert_eq!(outcome, Outcome::Create);
}

#[skuld::test]
fn identical_is_up_to_date() {
    let spec = spec_fixture();
    let artifact = "GENERATED ARTIFACT TEXT";
    let found = ours(&spec, RUNNING_VERSION, artifact, artifact);

    let outcome = decide(&found, artifact, &spec, RUNNING_VERSION, false, &ExternalConfig::Absent);

    assert_eq!(outcome, Outcome::UpToDate);
}

/// A clean spec change: the artifact on disk is exactly what the embedded
/// spec regenerates to (no hand-edit), but `new_spec` differs from the
/// embedded spec. Pinned apart from `spec_change_with_hand_edit_conflicts`
/// below, which looks identical except `on_disk` is *not*
/// `regenerated`.
#[skuld::test]
fn spec_change_updates() {
    let embedded = spec_fixture();
    let mut new_spec = spec_fixture();
    new_spec.restart = Restart::Always;
    let regenerated = "ARTIFACT FROM EMBEDDED SPEC";
    let found = ours(&embedded, RUNNING_VERSION, regenerated, regenerated);

    let outcome = decide(
        &found,
        "ARTIFACT FROM NEW SPEC",
        &new_spec,
        RUNNING_VERSION,
        false,
        &ExternalConfig::Absent,
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
    // A hand-added directive the spec cannot express: the embedded spec is
    // unchanged, so this can only be a hand-edit, not a spec change.
    let on_disk = "GENERATED\nMemoryMax=8G\n";
    let found = ours(&spec, RUNNING_VERSION, regenerated, on_disk);
    let desired = "GENERATED\n";

    let outcome = decide(&found, desired, &spec, RUNNING_VERSION, false, &ExternalConfig::Absent);

    match outcome {
        Outcome::Conflict { artifact_diff, .. } => {
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
    let on_disk = "GENERATED\nMemoryMax=8G\n";
    let found = ours(&spec, RUNNING_VERSION, regenerated, on_disk);
    let desired = "GENERATED\n";

    let outcome = decide(&found, desired, &spec, RUNNING_VERSION, true, &ExternalConfig::Absent);

    assert!(
        matches!(outcome, Outcome::Update { .. }),
        "force should overwrite a hand-edit rather than refuse: {outcome:?}"
    );
}

/// The spec changed *and* the
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
    // Hand-edited on top of the old (embedded-spec) artifact: differs from
    // both `regenerated` and `desired`.
    let on_disk = "ARTIFACT FROM EMBEDDED SPEC\nMemoryMax=8G\n";
    let found = ours(&embedded, RUNNING_VERSION, regenerated, on_disk);
    let desired = "ARTIFACT FROM NEW SPEC\n";

    let outcome = decide(
        &found,
        desired,
        &new_spec,
        RUNNING_VERSION,
        false,
        &ExternalConfig::Absent,
    );

    match outcome {
        Outcome::Conflict { artifact_diff, .. } => {
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
    let on_disk = "ARTIFACT FROM EMBEDDED SPEC\nMemoryMax=8G\n";
    let found = ours(&embedded, RUNNING_VERSION, regenerated, on_disk);
    let desired = "ARTIFACT FROM NEW SPEC\n";

    let outcome = decide(
        &found,
        desired,
        &new_spec,
        RUNNING_VERSION,
        true,
        &ExternalConfig::Absent,
    );

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
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            force,
            &ExternalConfig::Absent,
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
            recovery: Some(unreadable_recovery("frpc")),
        };

        let outcome = decide(
            &found,
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            force,
            &ExternalConfig::Absent,
        );

        match outcome {
            Outcome::RefuseUnreadable { reason, recovery } => {
                assert_eq!(reason, "schema 2 is not understood by this build");
                let recovery = recovery.expect("a decode failure carries a recovery");
                assert!(!recovery.is_empty(), "recovery must be non-empty (force={force})");
            }
            other => panic!("expected RefuseUnreadable regardless of force={force}, got {other:?}"),
        }
    }
}

/// `decide` hands the recovery on exactly as discovery gave it: a recovery stays, and no recovery
/// stays none, rather than `decide` inventing one for a refusal that has no action to name.
#[skuld::test]
fn unreadable_carries_its_recovery_through() {
    let spec = spec_fixture();

    for recovery in [Some("run the exact remedy".to_owned()), None] {
        let found = Ownership::OursUnreadable {
            reason: "a drop-in could not be read".to_owned(),
            recovery: recovery.clone(),
        };

        let outcome = decide(
            &found,
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            false,
            &ExternalConfig::Absent,
        );

        assert_eq!(
            outcome,
            Outcome::RefuseUnreadable {
                reason: "a drop-in could not be read".to_owned(),
                recovery,
            }
        );
    }
}

/// The text a decode failure carries names the exact `uninstall` that unblocks the id.
#[skuld::test]
fn unreadable_recovery_names_the_uninstall() {
    let recovery = unreadable_recovery("frpc");

    assert!(
        recovery.contains("goetia daemon uninstall frpc"),
        "the recovery must name the exact command: {recovery}"
    );
}

/// A service registered by the released 0.1.0 must read as stale under this build, whatever the
/// artifact looks like: the version string is the only thing that tells the two generators apart.
#[skuld::test]
fn a_registration_by_released_0_1_0_is_stale_under_this_build() {
    let spec = spec_fixture();
    let on_disk = "ARTIFACT WITHOUT RECOVERY ACTIONS";
    let found = ours(&spec, "0.1.0", "REGENERATED WITH THE OLD GENERATOR", on_disk);

    let outcome = decide(
        &found,
        "ARTIFACT WITH RECOVERY ACTIONS",
        &spec,
        crate::version(),
        false,
        &ExternalConfig::Absent,
    );

    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: "0.1.0".to_owned()
        }
    );
}

#[skuld::test]
fn older_version_is_stale_not_conflict() {
    let spec = spec_fixture();
    // The spec is unchanged: on its own, that would route into the
    // hand-edit/conflict check. The version mismatch must be checked
    // first and win outright.
    let on_disk = "ARTIFACT WRITTEN BY 0.1.0";
    let desired = "ARTIFACT AS 1.2.3 WOULD WRITE IT";
    let found = ours(&spec, "0.1.0", on_disk, on_disk);

    let outcome = decide(&found, desired, &spec, RUNNING_VERSION, false, &ExternalConfig::Absent);

    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: "0.1.0".to_owned()
        }
    );
}

/// A rendering that differs from the desired one only in its recovery actions, as a `type: simple`
/// service registered by 0.1.0 does, is a `Stale` refresh and never a `Conflict`, because the
/// version is compared before the content. Pins that ordering only; that re-installing such a
/// service really writes the actions is `reinstalling_an_earlier_version_simple_service_applies_the_recovery_actions`.
#[skuld::test]
fn a_version_difference_wins_over_a_difference_in_recovery_actions() {
    use crate::backend::Identity;
    use crate::backend::scm::generate::{ScmRegistration, registration};

    let spec = spec_fixture();
    let reg_now = registration(
        &spec,
        &Identity { user: "svc".to_owned() },
        &PathBuf::from("goetia-shim.exe"),
    )
    .unwrap();
    assert!(
        reg_now.failure_actions.is_some(),
        "the fixture must be a policy that now gets recovery actions"
    );
    let reg_0_1_0 = ScmRegistration {
        failure_actions: None,
        ..reg_now.clone()
    };
    let on_disk = reg_0_1_0.to_string();
    assert!(on_disk.contains("FailureActions: none"));
    let desired = reg_now.to_string();
    assert_ne!(on_disk, desired);
    let found = ours(&spec, "0.1.0", &on_disk, &on_disk);

    let outcome = decide(&found, &desired, &spec, RUNNING_VERSION, false, &ExternalConfig::Absent);

    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: "0.1.0".to_owned()
        }
    );
}

/// Both places `decide` returns `Conflict` -- a hand-edited artifact, and a stale one carrying
/// external configuration -- have to carry its recovery through. Losing it on either leaves the CLI
/// offering `--force` for a conflict `--force` rewrites the artifact for and leaves exactly as it
/// found it, so the operator forces, is told to force, and forces again.
#[skuld::test]
fn every_conflict_carries_the_external_configs_unclearable_recovery() {
    let spec = spec_fixture();
    let regenerated = "ARTIFACT WRITTEN BY GOETIA";
    let on_disk = "ARTIFACT WRITTEN BY GOETIA\n# --- drop-in: /usr/lib/... ---\n";
    let desired = "ARTIFACT AS 1.2.3 WOULD WRITE IT";

    for (case, found) in [
        ("hand-edited", ours(&spec, RUNNING_VERSION, regenerated, on_disk)),
        (
            "stale under external config",
            ours(&spec, "0.1.0", regenerated, on_disk),
        ),
    ] {
        for (flavour, external) in [("clearable", clearable()), ("unclearable", unclearable())] {
            let outcome = decide(&found, desired, &spec, RUNNING_VERSION, false, &external);
            let Outcome::Conflict {
                unclearable_recovery, ..
            } = outcome
            else {
                panic!("{case}/{flavour}: expected Conflict, got {outcome:?}");
            };
            let expected = match &external {
                ExternalConfig::Unclearable { recovery } => Some(recovery.clone()),
                ExternalConfig::Absent | ExternalConfig::Clearable => None,
            };
            assert_eq!(
                unclearable_recovery, expected,
                "{case}/{flavour}: the CLI decides whether to offer `--force` from this alone"
            );
        }
    }
}

/// A stale artifact that also carries external configuration (systemd's drop-in directory, or any
/// future backend's equivalent) must not be silently regenerated: the configuration is exactly as
/// much "not solely goetia's doing" as a hand-edited fragment is, and `decide`'s version check runs
/// before any text comparison, so without this it would be destroyed without ever being reported.
#[skuld::test]
fn stale_with_external_config_conflicts_not_stale() {
    let spec = spec_fixture();
    // `regenerated` and `on_disk` deliberately differ, mirroring how a real backend's `discover`
    // folds a marker for the external configuration onto `on_disk` without ever including it in
    // `regenerated`: an identical pair here would make the conflict diff trivially (and wrongly) empty.
    let regenerated = "ARTIFACT WRITTEN BY 0.1.0";
    let on_disk = "ARTIFACT WRITTEN BY 0.1.0\n# --- external config marker ---\n";
    let desired = "ARTIFACT AS 1.2.3 WOULD WRITE IT";
    let found = ours(&spec, "0.1.0", regenerated, on_disk);

    let outcome = decide(&found, desired, &spec, RUNNING_VERSION, false, &clearable());

    match outcome {
        Outcome::Conflict { artifact_diff, .. } => {
            assert!(!artifact_diff.is_empty(), "conflict must carry a non-empty diff");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

/// `force` still lets a stale-plus-external-config artifact regenerate, exactly as it does for a
/// plain hand-edit conflict: the external configuration only changes what happens when nothing
/// overrides the refusal.
#[skuld::test]
fn stale_with_external_config_and_force_is_stale() {
    let spec = spec_fixture();
    let regenerated = "ARTIFACT WRITTEN BY 0.1.0";
    let on_disk = "ARTIFACT WRITTEN BY 0.1.0\n# --- external config marker ---\n";
    let desired = "ARTIFACT AS 1.2.3 WOULD WRITE IT";
    let found = ours(&spec, "0.1.0", regenerated, on_disk);

    let outcome = decide(&found, desired, &spec, RUNNING_VERSION, true, &clearable());

    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: "0.1.0".to_owned()
        }
    );
}

/// Every refusal names a recovery, except an unreadable one whose discovery had none to name (a
/// drop-in that could not be read).
#[skuld::test]
fn every_refusal_names_a_recovery_command_except_a_recoveryless_unreadable() {
    let spec = spec_fixture();
    let refusals = [
        decide(
            &Ownership::Foreign,
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            false,
            &ExternalConfig::Absent,
        ),
        decide(
            &Ownership::OursUnreadable {
                reason: "corrupt".to_owned(),
                recovery: Some(unreadable_recovery("frpc")),
            },
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            false,
            &ExternalConfig::Absent,
        ),
        decide(
            &Ownership::OursUnreadable {
                reason: "a drop-in could not be read".to_owned(),
                recovery: None,
            },
            "GENERATED",
            &spec,
            RUNNING_VERSION,
            false,
            &ExternalConfig::Absent,
        ),
    ];

    for outcome in refusals {
        match outcome {
            Outcome::RefuseForeign { recovery }
            | Outcome::RefuseUnreadable {
                recovery: Some(recovery),
                ..
            } => {
                assert!(!recovery.is_empty(), "every refusal must name a recovery command");
            }
            Outcome::RefuseUnreadable { recovery: None, .. } => {}
            other => panic!("expected a refusing variant, got {other:?}"),
        }
    }
}
