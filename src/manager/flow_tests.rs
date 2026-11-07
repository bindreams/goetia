use super::*;
use crate::decide::{self, ExternalConfig, Outcome};
use crate::error::Error;
use crate::manager::fake::fake_tests::mk;
use crate::manager::fake::{Fake, generate};
use crate::manager::{Budget, Installed, ServiceManager, Step};
use crate::spec::{DaemonSpec, Id, User};

// Fixtures ============================================================================================================

fn mk_as(id: &str, user: &str) -> DaemonSpec {
    DaemonSpec {
        user: User::Name(user.to_string()),
        ..mk(id)
    }
}

/// What `Fake::render` fails with for a seeded unresolvable account.
fn unresolvable(user: &str) -> String {
    format!("account lookup failed: account `{user}` cannot be resolved (seeded)")
}

// install =============================================================================================================

#[skuld::test]
fn install_retries_a_raced_write_until_it_lands() {
    let fake = Fake::new();
    fake.seed_write_races("frpc", 2);

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    assert_eq!(outcome, Outcome::Create);
    assert_eq!(fake.writes(), 3, "two raced writes, then the one that landed");
    fake.status(&Id::try_from("frpc").unwrap())
        .expect("the artifact must be present");
    assert_eq!(
        fake.prepared(),
        [(vec![Step::Install], Budget::Unbounded)],
        "one reservation is made for every retry"
    );
}

#[skuld::test]
fn install_refuses_an_unresolvable_account_with_uninstall_advice() {
    let fake = Fake::new();
    fake.install(&mk_as("frpc", "gone"), false).unwrap();
    fake.seed_unresolvable_account("gone");

    let outcome = fake.install(&mk_as("frpc", "other"), false).unwrap();

    assert_eq!(
        outcome,
        Outcome::RefuseUnusable {
            reason: Reason::said(format!("cannot regenerate the embedded spec: {}", unresolvable("gone"))),
            recovery: Some(decide::unusable_recovery("frpc")),
        }
    );
}

#[skuld::test]
fn install_refuses_an_undecodable_blob_with_uninstall_advice() {
    let fake = Fake::new();
    fake.seed_unusable("frpc");

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    match outcome {
        Outcome::RefuseUnusable { recovery, .. } => {
            assert_eq!(recovery, Some(decide::unusable_recovery("frpc")));
        }
        other => panic!("expected RefuseUnusable, got {other:?}"),
    }
}

#[skuld::test]
fn install_refuses_a_failed_comparison_with_its_error_alone() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_installed_fails("frpc", || {
        Error::os(
            "read back the unit",
            std::io::Error::other("the unit could not be read back"),
        )
    });

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    match outcome {
        Outcome::RefuseUnusable { reason, recovery } => {
            let reason = reason.to_string();
            assert!(reason.contains("the unit could not be read back"), "{reason}");
            assert_eq!(recovery, None);
        }
        other => panic!("expected RefuseUnusable, got {other:?}"),
    }
}

/// The write must notice the id changed since it was read. A flow that retried the write with the
/// stale read would overwrite what appeared.
#[skuld::test]
fn a_write_that_loses_to_a_foreign_entry_refuses_and_overwrites_nothing() {
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", "some pre-existing, unrelated service\n");

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    assert!(matches!(outcome, Outcome::RefuseForeign { .. }), "{outcome:?}");
    assert_eq!(fake.writes(), 1, "the raced write is not retried over a foreign entry");
    let after = fake.preview_install(&mk("frpc")).unwrap();
    assert!(matches!(after, Outcome::RefuseForeign { .. }), "{after:?}");
}

/// A manager that matches ids case-insensitively answers a request for `FRPC` with `frpc`'s
/// service. The snapshot is named `FRPC`; its blob says `frpc`. No flag lets a write replace it.
#[skuld::test]
fn install_and_preview_refuse_a_service_the_manager_finds_for_another_daemon() {
    let fake = Fake::new();
    fake.seed_installed_as("FRPC", &mk("frpc"));
    let requested = mk("FRPC");
    let expected = Outcome::RefuseOtherDaemon {
        installed: Id::try_from("frpc").unwrap(),
    };

    for force in [false, true] {
        assert_eq!(fake.install(&requested, force).unwrap(), expected, "force={force}");
    }
    assert_eq!(fake.preview_install(&requested).unwrap(), expected);
    assert_eq!(fake.writes(), 0, "the installed service must not be written over");
}

/// What the interloper wrote is read afresh and decided on, whichever of `decide`'s rows it hits.
#[skuld::test]
fn a_write_that_loses_to_an_ours_entry_follows_that_entry() {
    let theirs = mk("frpc");
    let mine = mk_as("frpc", "svc");
    let hand_edited = format!("{}# hand-added directive\n", generate(&theirs));

    // Same spec: nothing is left to write.
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", generate(&theirs));
    assert_eq!(fake.install(&theirs, false).unwrap(), Outcome::UpToDate);
    assert_eq!(fake.writes(), 1);

    // A clean entry of another spec: updated, and the update lands.
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", generate(&theirs));
    let outcome = fake.install(&mine, false).unwrap();
    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(fake.writes(), 2, "the raced write, then the one over the new state");
    assert_eq!(fake.preview_install(&mine).unwrap(), Outcome::UpToDate);

    // A hand-edited entry: refused as a conflict, and the edit survives.
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", hand_edited);
    let outcome = fake.install(&mine, false).unwrap();
    assert!(matches!(outcome, Outcome::Conflict { .. }), "{outcome:?}");
    assert_eq!(fake.writes(), 1);
    let after = fake.preview_install(&mine).unwrap();
    assert!(matches!(after, Outcome::Conflict { .. }), "{after:?}");
}

/// The recovery for configuration a write will not remove reaches the user from `install` and
/// `preview_install`; only `--force` over what a write does remove goes through.
#[skuld::test]
fn external_configuration_is_carried_into_the_outcome() {
    let recovery = "remove /etc/drop-in by hand".to_string();

    let hand_edited = Fake::new();
    hand_edited.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");
    hand_edited.seed_external(
        "frpc",
        ExternalConfig::Unclearable {
            recovery: recovery.clone(),
        },
    );
    for outcome in [
        hand_edited.install(&mk_as("frpc", "svc"), false).unwrap(),
        hand_edited.preview_install(&mk_as("frpc", "svc")).unwrap(),
    ] {
        match outcome {
            Outcome::Conflict {
                unclearable_recovery, ..
            } => assert_eq!(unclearable_recovery, Some(recovery.clone())),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // A stale entry that carries external configuration is a conflict unless forced.
    let stale = Fake::new();
    stale.seed_stale(&mk("frpc"), "0.0.1");
    stale.seed_external("frpc", ExternalConfig::Clearable);
    for outcome in [
        stale.install(&mk("frpc"), false).unwrap(),
        stale.preview_install(&mk("frpc")).unwrap(),
    ] {
        match outcome {
            Outcome::Conflict {
                unclearable_recovery, ..
            } => assert_eq!(unclearable_recovery, None),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
    let forced = stale.install(&mk("frpc"), true).unwrap();
    assert!(matches!(forced, Outcome::Stale { .. }), "{forced:?}");
}

// regenerate ==========================================================================================================

/// A service whose host the provider now names elsewhere is the spec's own, unchanged: judged by
/// the installed host, it is an `Update` to the new one, never a hand-edit's `Conflict`.
#[skuld::test]
fn moved_shim_reads_as_update() {
    let fake = Fake::new();
    fake.seed_host("old");
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_host("new");

    let previewed = fake.preview_install(&mk("frpc")).unwrap();
    let installed = fake.install(&mk("frpc"), false).unwrap();

    assert!(matches!(previewed, Outcome::Update { .. }), "{previewed:?}");
    assert!(matches!(installed, Outcome::Update { .. }), "{installed:?}");
}

/// `status` and `list` only check that the stored spec still regenerates, which needs no provider.
#[skuld::test]
fn status_and_list_use_regenerate() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_no_provider();

    fake.status(&Id::try_from("frpc").unwrap())
        .expect("status regenerates, which needs no provider");
    let listed = fake.list().unwrap();

    assert!(
        matches!(listed.as_slice(), [Installed::Ours { spec, .. }] if spec.id.as_str() == "frpc"),
        "{listed:?}"
    );
}

/// A name that is no id has no snapshot, so it is reported by that name alone, whatever its spec
/// would do when regenerated.
#[skuld::test]
fn misnamed_entry_is_reported_without_regenerating() {
    let fake = Fake::new();
    let name = "not a valid id";
    fake.seed_foreign(name, generate(&mk_as("frpc", "gone")));
    fake.seed_unresolvable_account("gone");
    let invalid = Id::try_from(name).unwrap_err();

    let listed = fake.list().unwrap();

    let [entry @ Installed::OursUnusable { name: got, .. }] = listed.as_slice() else {
        panic!("one unusable entry expected, got {listed:?}");
    };
    assert_eq!(got, name);
    assert_eq!(
        entry.unusable_reason(),
        Some(format!("marked ours, but its name is not a valid daemon id: {invalid}"))
    );
}

// status and list =====================================================================================================

#[skuld::test]
fn status_reports_an_unresolvable_account_as_an_error() {
    let fake = Fake::new();
    fake.install(&mk_as("frpc", "gone"), false).unwrap();
    fake.seed_unresolvable_account("gone");

    let err = fake.status(&Id::try_from("frpc").unwrap()).unwrap_err();

    match &err {
        Error::Step { .. } => assert_eq!(
            err.report().to_string(),
            format!("`frpc`'s embedded spec cannot be regenerated: {}", unresolvable("gone"))
        ),
        other => panic!("expected Error::Step, got {other:?}"),
    }
}

fn installed_host_unreadable() -> Error {
    Error::InstalledUnreadable {
        id: "frpc".to_string(),
        source: Box::new(Error::os(
            "query configuration for `frpc`",
            std::io::Error::other("access denied"),
        )),
    }
}

/// A failed read of what is installed is not a spec that cannot be regenerated.
#[skuld::test]
fn status_names_a_failed_read_of_the_installed_host_as_that() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_installed_fails("frpc", installed_host_unreadable);

    let err = fake.status(&Id::try_from("frpc").unwrap()).unwrap_err();

    assert_eq!(
        err.report().to_string(),
        "read `frpc`'s installed host: failed to query configuration for `frpc`: access denied"
    );
    assert!(matches!(err, Error::InstalledUnreadable { .. }), "{err:?}");
}

#[skuld::test]
fn list_names_a_failed_read_of_the_installed_host_as_that() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_installed_fails("frpc", installed_host_unreadable);

    let listed = fake.list().unwrap();

    let [entry @ Installed::OursUnusable { name, .. }] = listed.as_slice() else {
        panic!("one unusable entry expected, got {listed:?}");
    };
    assert_eq!(name, "frpc");
    assert_eq!(
        entry.unusable_reason().as_deref(),
        Some("read `frpc`'s installed host: failed to query configuration for `frpc`: access denied")
    );
}

/// The read finding the service gone is absence, however it is labelled.
#[skuld::test]
fn list_omits_a_service_whose_installed_host_read_found_it_gone() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_installed_fails("frpc", || Error::InstalledUnreadable {
        id: "frpc".to_string(),
        source: Box::new(Error::NotInstalled { id: "frpc".to_string() }),
    });

    assert!(fake.list().unwrap().is_empty());
}

#[skuld::test]
fn list_reports_an_unresolvable_account_as_ours_unusable() {
    let fake = Fake::new();
    fake.install(&mk_as("frpc", "gone"), false).unwrap();
    fake.seed_unresolvable_account("gone");

    let listed = fake.list().unwrap();

    let [entry @ Installed::OursUnusable { name, reason }] = listed.as_slice() else {
        panic!("one unusable entry expected, got {listed:?}");
    };
    assert_eq!(name, "frpc");
    assert!(
        matches!(reason.cause(), Some(Error::AccountLookup { .. })),
        "{reason:?}"
    );
    assert_eq!(
        entry.unusable_reason().as_deref(),
        Some(
            format!(
                "marked ours, but its embedded spec cannot be regenerated: {}",
                unresolvable("gone")
            )
            .as_str()
        )
    );
}

#[skuld::test]
fn list_reports_a_failed_live_status_as_ours_unusable() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.install(&mk("other"), false).unwrap();
    fake.seed_live_status_fails("frpc", || {
        Error::os("query the live state", std::io::Error::other("state unavailable"))
    });

    let listed = fake.list().unwrap();

    assert_eq!(
        listed.len(),
        2,
        "one entry's failure must not take down the others: {listed:?}"
    );
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::OursUnusable { name, reason }
                if name == "frpc" && matches!(reason.cause(), Some(Error::Os { .. })))
                && e.unusable_reason().as_deref()
                    == Some(
                        "marked ours, but its live state could not be queried: failed to query the live state: state \
                     unavailable"
                    )),
        "{listed:?}"
    );
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.id.as_str() == "other")),
        "{listed:?}"
    );
    match fake.status(&Id::try_from("frpc").unwrap()) {
        Err(e @ Error::Os { .. }) => assert_eq!(
            e.report().to_string(),
            "failed to query the live state: state unavailable"
        ),
        other => panic!("status of the same id must return the error, got {other:?}"),
    }
}

#[skuld::test]
fn list_reports_a_name_that_is_not_an_id_without_querying_it() {
    let fake = Fake::new();
    let name = "not a valid id";
    fake.seed_foreign(name, generate(&mk("frpc")));
    fake.install(&mk("other"), false).unwrap();
    let invalid = Id::try_from(name).unwrap_err();

    let listed = fake.list().unwrap();

    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::OursUnusable { name: got, reason }
                if got == name && matches!(reason.cause(), Some(Error::Invalid { .. })))
                && e.unusable_reason()
                    == Some(format!("marked ours, but its name is not a valid daemon id: {invalid}"))),
        "{listed:?}"
    );
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.id.as_str() == "other")),
        "{listed:?}"
    );
}

/// The reasons a marked entry is unusable come in a fixed order: its blob first, its name last.
#[skuld::test]
fn list_reports_an_undecodable_blob_before_a_name_that_is_not_an_id() {
    let fake = Fake::new();
    let name = "not a valid id";
    fake.seed_unusable(name);

    let listed = fake.list().unwrap();

    match listed.as_slice() {
        [entry @ Installed::OursUnusable { name: got, .. }] => {
            assert_eq!(got, name);
            let reason = entry.unusable_reason().expect("an unusable entry has a reason");
            assert!(
                reason.starts_with("blob: "),
                "the blob's reason, not the name's: {reason}"
            );
        }
        other => panic!("one unusable entry expected, got {other:?}"),
    }
}

#[skuld::test]
fn list_omits_an_entry_that_vanished_before_its_live_query() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.install(&mk("other"), false).unwrap();
    fake.seed_live_status_gone("frpc");

    let listed = fake.list().unwrap();

    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        matches!(&listed[0], Installed::Ours { spec, .. } if spec.id.as_str() == "other"),
        "{listed:?}"
    );
}

#[skuld::test]
fn list_aborts_on_no_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_no_manager();

    let result = fake.list();

    assert!(matches!(result, Err(Error::NoManager { .. })), "{result:?}");
}

#[skuld::test]
fn status_and_gate_carry_a_backend_foreign_recovery() {
    let fake = Fake::new();
    fake.seed_residual_artifact("leftover");
    fake.seed_residual_recovery("advised", "remove the leftover by hand");

    for (id, recovery) in [
        ("leftover", decide::foreign_recovery("leftover")),
        ("advised", "remove the leftover by hand".to_string()),
    ] {
        let id = Id::try_from(id).unwrap();
        for (what, result) in [
            ("status", fake.status(&id).map(drop)),
            ("owned", owned(&fake, &id).map(drop)),
        ] {
            match result {
                Err(Error::Foreign { recovery: got, .. }) => assert_eq!(got, recovery, "{what} of {id}"),
                other => panic!("{what} of {id} must be Foreign, got {other:?}"),
            }
        }
    }
}

// owned ===============================================================================================================

#[skuld::test]
fn owned_maps_absent_foreign_and_marked() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact\n");
    fake.install(&mk("ours"), false).unwrap();
    fake.seed_unusable("corrupt");
    let id = |s: &str| Id::try_from(s).unwrap();

    assert!(matches!(owned(&fake, &id("nobody")), Err(Error::NotInstalled { .. })));
    assert!(matches!(owned(&fake, &id("stranger")), Err(Error::Foreign { .. })));
    assert!(owned(&fake, &id("ours")).is_ok());
    assert!(
        owned(&fake, &id("corrupt")).is_ok(),
        "the marker alone is proof of ownership"
    );
}

/// A verb takes the snapshot the gate yields, so the blanket impl alone gates every verb: on an id
/// that is not goetia's each says why, and the backend is never asked to act. The one thing it is
/// asked is the cleanup `uninstall` of an absent id offers it, which acts on nothing of the id's.
#[skuld::test]
fn every_verb_on_an_id_that_is_not_ours_reports_why_and_performs_nothing() {
    let fake = Fake::new();
    fake.seed_native_restart();
    fake.seed_foreign("stranger", "not a goetia artifact\n");
    let absent = Id::try_from("nobody").unwrap();
    let foreign = Id::try_from("stranger").unwrap();
    let budget = crate::manager::Budget::DEFAULT;
    let mgr: &dyn ServiceManager = &fake;
    type Verb<'a> = (&'a str, &'a dyn Fn(&Id) -> Result<()>);
    let verbs: [Verb; 9] = [
        ("uninstall", &|id| mgr.uninstall(id)),
        ("enable", &|id| mgr.enable(id)),
        ("disable", &|id| mgr.disable(id)),
        ("start", &|id| mgr.start(id, budget)),
        ("stop", &|id| mgr.stop(id, budget)),
        ("kill", &|id| mgr.kill(id)),
        ("request_restart", &|id| {
            mgr.request_restart(id).expect("this fake restarts natively")
        }),
        ("request_start_after_stop", &|id| mgr.request_start_after_stop(id)),
        ("status", &|id| mgr.status(id).map(drop)),
    ];

    for (verb, run) in verbs {
        let result = run(&absent);
        assert!(
            matches!(&result, Err(Error::NotInstalled { id }) if id == "nobody"),
            "{verb} of an absent id: {result:?}"
        );
        let result = run(&foreign);
        assert!(
            matches!(&result, Err(Error::Foreign { id, .. }) if id == "stranger"),
            "{verb} of a foreign id: {result:?}"
        );
    }

    assert_eq!(
        fake.calls(),
        [("uninstall_absent", "nobody".to_string())],
        "no verb reached the backend but the absent id's cleanup, once"
    );
    assert_eq!(fake.prepared(), [], "no verb reserved anything before the gate passed");
    assert_eq!(fake.writes(), 0);
    let after = fake.preview_install(&mk("stranger")).unwrap();
    assert!(matches!(after, Outcome::RefuseForeign { .. }), "{after:?}");
}

// scan ================================================================================================================

type Names = std::vec::IntoIter<std::io::Result<String>>;

fn named(items: Vec<std::io::Result<String>>) -> std::io::Result<Names> {
    Ok(items.into_iter())
}

fn op_and_cause(op: &'static str, e: std::io::Error) -> Error {
    Error::os(op, e)
}

/// Collecting into a `Result` instead would discard every name enumerated before a later one
/// faulted, which reports each of them absent.
#[skuld::test]
fn scan_keeps_entries_before_a_mid_pass_fault() {
    let items = vec![
        Ok("named-before-the-fault".to_string()),
        Err(std::io::Error::other("injected mid-scan failure")),
        Ok("never-reached".to_string()),
    ];
    let mut kept = Vec::new();

    let reported = scan(
        "the place",
        named(items),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert_eq!(kept, ["named-before-the-fault"], "the pass stops at the fault");
    match reported {
        Some(Installed::Undetermined { name: None, reason }) => {
            let reason = reason.to_string();
            assert!(reason.contains("the place"), "{reason}");
            assert!(
                reason.contains("failed to enumerate: injected mid-scan failure"),
                "{reason}"
            );
        }
        other => panic!("a pass that stopped early must say what it did not reach, got {other:?}"),
    }
}

/// Absence is established: an empty scan with nothing outstanding, not an unfinished one.
#[skuld::test]
fn scan_treats_a_missing_location_as_empty() {
    let mut kept = Vec::new();

    let reported = scan::<Names, String>(
        "the place",
        Err(std::io::ErrorKind::NotFound.into()),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert!(kept.is_empty(), "{kept:?}");
    assert!(reported.is_none(), "{reported:?}");
}

/// The outcome that must never be confused with the one above: a location that is there and will
/// not open leaves the pass unfinished, so no id may be concluded absent.
#[skuld::test]
fn scan_reports_a_location_that_will_not_open() {
    let mut kept = Vec::new();

    let reported = scan::<Names, String>(
        "the place",
        Err(std::io::ErrorKind::PermissionDenied.into()),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert!(kept.is_empty(), "{kept:?}");
    match reported {
        Some(Installed::Undetermined { name: None, reason }) => {
            let reason = reason.to_string();
            assert!(reason.contains("the place"), "{reason}");
            assert!(reason.contains("failed to open:"), "{reason}");
        }
        other => panic!("a location that exists and will not open establishes no absence, got {other:?}"),
    }
}

#[skuld::test]
fn scan_that_finishes_reports_nothing() {
    let mut kept = Vec::new();

    let reported = scan(
        "the place",
        named(vec![Ok("a".to_string()), Ok("b".to_string())]),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert_eq!(kept, ["a", "b"]);
    assert!(reported.is_none(), "{reported:?}");
}

/// The text of a directory pass that stopped, which every directory-scanning backend hands
/// `scan` as its `detail`. Injected here because no real `readdir` fails on request.
#[cfg(unix)]
#[skuld::test]
fn a_directory_pass_that_stopped_is_worded_by_where_it_stopped() {
    let e = std::io::Error::other("injected mid-scan failure");

    let again = || std::io::Error::other("injected mid-scan failure");

    assert_eq!(
        directory_detail("enumerate", e).report().to_string(),
        "failed to read a directory entry: injected mid-scan failure"
    );
    assert_eq!(
        directory_detail("open", again()).report().to_string(),
        "failed to read directory: injected mid-scan failure"
    );
}

// single verbs reserve ================================================================================================

/// The verbs that draw on a reservation, each run on `mgr` as the blanket impl runs it outside a
/// sequence, with the step it reserves for and the budget it reserves under.
#[allow(clippy::type_complexity)]
fn single_verbs<'a>(
    mgr: &'a dyn ServiceManager,
    id: &'a Id,
    spec: &'a DaemonSpec,
) -> Vec<(&'static str, Step, Budget, Box<dyn Fn() -> Result<()> + 'a>)> {
    let budget = Budget::DEFAULT;
    vec![
        (
            "install",
            Step::Install,
            Budget::Unbounded,
            Box::new(|| mgr.install(spec, true).map(drop)),
        ),
        ("start", Step::Start, budget, Box::new(move || mgr.start(id, budget))),
        ("stop", Step::Stop, budget, Box::new(move || mgr.stop(id, budget))),
        ("kill", Step::Kill, Budget::Unbounded, Box::new(|| mgr.kill(id))),
        (
            "uninstall",
            Step::Uninstall,
            Budget::Unbounded,
            Box::new(|| mgr.uninstall(id)),
        ),
        (
            "request_start_after_stop",
            Step::RequestStartAfterStop,
            Budget::Immediate,
            Box::new(|| mgr.request_start_after_stop(id)),
        ),
    ]
}

/// Each verb run outside a sequence reserves for its own step, under the budget it runs with. `kill`
/// and `uninstall` count what `stop` does on a real backend, so only the step recorded tells them
/// apart.
#[skuld::test]
fn a_single_verb_reserves_for_its_own_step() {
    let spec = mk("frpc");
    let id = spec.id.clone();
    let fake = Fake::new();
    let mgr: &dyn ServiceManager = &fake;
    for (verb, step, budget, run) in single_verbs(mgr, &id, &spec) {
        fake.install(&spec, true).unwrap();
        let before = fake.prepared().len();

        // Whether the verb then succeeds is not the question: its reservation comes first.
        drop(run());

        assert_eq!(fake.prepared()[before..], [(vec![step], budget)], "{verb}");
    }
}

/// A single verb whose reservation cannot be made sends nothing and fails as `Error::Reservation`
/// carrying its id, whichever verb it is.
#[skuld::test]
fn a_single_verb_that_cannot_reserve_sends_nothing_and_names_its_id() {
    let spec = mk("frpc");
    let id = spec.id.clone();
    let fake = Fake::new();
    fake.install(&spec, false).unwrap();
    let (calls, writes) = (fake.calls(), fake.writes());
    let mgr: &dyn ServiceManager = &fake;

    for (verb, step, _, run) in single_verbs(mgr, &id, &spec) {
        fake.seed_prepare_fails(step);

        let result = run();

        assert!(
            matches!(&result, Err(Error::Reservation { id: Some(named), .. }) if named == "frpc"),
            "{verb}: {result:?}"
        );
        assert_eq!(fake.calls(), calls, "{verb} reached the backend");
        assert_eq!(fake.writes(), writes, "{verb} wrote");
        assert!(fake.status(&id).is_ok(), "{verb} removed the service");
    }
}

// sequence ============================================================================================================

/// A prepared step is used once, and only a prepared step: a verb that was not prepared for cannot
/// have had anything reserved, so it is a miscount, loud in every debug build.
#[cfg(debug_assertions)]
#[skuld::test]
#[should_panic(
    expected = "[Start] was not prepared for, or already ran: it would draw on a reservation not counted for it"
)]
fn a_sequence_refuses_a_verb_it_was_not_prepared_for() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    let id = Id::try_from("frpc").unwrap();
    let mut sequence = fake.prepare(&[Step::Stop], Budget::DEFAULT).unwrap();

    drop(sequence.start(&id, Budget::DEFAULT));
}

#[cfg(debug_assertions)]
#[skuld::test]
#[should_panic(
    expected = "[Stop] was not prepared for, or already ran: it would draw on a reservation not counted for it"
)]
fn a_sequence_refuses_a_step_run_twice() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    let id = Id::try_from("frpc").unwrap();
    let mut sequence = fake.prepare(&[Step::Stop], Budget::DEFAULT).unwrap();

    sequence.stop(&id, Budget::DEFAULT).unwrap();
    drop(sequence.stop(&id, Budget::DEFAULT));
}

/// `restart --timeout 0` prepares `Start` and runs `request_start_after_stop`: a request for a start
/// spends the step a start does.
#[skuld::test]
fn a_prepared_start_covers_a_start_requested_after_a_stop() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    let id = Id::try_from("frpc").unwrap();
    let mut sequence = fake.prepare(&[Step::Stop, Step::Start], Budget::Immediate).unwrap();

    sequence.stop(&id, Budget::Immediate).unwrap();
    sequence.request_start_after_stop(&id).unwrap();
}

/// A request for a start spends the step made for exactly that before it falls back to `Start`,
/// so a sequence prepared for both can still start afterwards.
#[cfg(debug_assertions)]
#[skuld::test]
fn a_requested_start_spends_its_own_step_before_start() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    let id = Id::try_from("frpc").unwrap();
    let mut sequence = fake
        .prepare(&[Step::RequestStartAfterStop, Step::Start], Budget::Immediate)
        .unwrap();

    sequence.request_start_after_stop(&id).unwrap();

    sequence.start(&id, Budget::Immediate).unwrap();
}

/// The cover goes one way: a step prepared for the request alone does not count a start.
#[cfg(debug_assertions)]
#[skuld::test]
#[should_panic(
    expected = "[Start] was not prepared for, or already ran: it would draw on a reservation not counted for it"
)]
fn a_step_prepared_for_a_requested_start_does_not_cover_a_start() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    let id = Id::try_from("frpc").unwrap();
    let mut sequence = fake.prepare(&[Step::RequestStartAfterStop], Budget::Immediate).unwrap();

    drop(sequence.start(&id, Budget::DEFAULT));
}

/// A verb the ownership gate refuses draws nothing, so it spends nothing: the sequence still holds
/// every step, the refused one included, and runs it once the gate lets it through.
#[cfg(debug_assertions)]
#[skuld::test]
fn a_verb_the_gate_refuses_spends_no_step() {
    let fake = Fake::new();
    let id = Id::try_from("frpc").unwrap();
    let mut sequence = fake.prepare(&[Step::Stop, Step::Start], Budget::DEFAULT).unwrap();

    let refused = sequence.stop(&id, Budget::DEFAULT);

    assert!(matches!(refused, Err(Error::NotInstalled { .. })), "{refused:?}");
    fake.install(&mk("frpc"), false).unwrap();
    sequence.stop(&id, Budget::DEFAULT).unwrap();
    sequence.start(&id, Budget::DEFAULT).unwrap();
}
