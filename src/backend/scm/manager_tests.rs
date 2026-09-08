//! Pure unit tests: no real SCM/registry access, so these run unelevated
//! alongside every other `#[skuld::test]` in the library. Elevated behavior
//! (everything that actually talks to SCM) is covered by
//! `tests/scm_integration.rs` instead.

use std::path::PathBuf;

use super::*;

fn dummy_reg(account: Option<&str>) -> ScmRegistration {
    ScmRegistration {
        name: "goetia-test".to_string(),
        display_name: "Goetia Test".to_string(),
        executable: PathBuf::from(r"C:\bin\test.exe"),
        arguments: vec![],
        account: account.map(str::to_string),
        failure_actions: None,
        parameters: BTreeMap::new(),
    }
}

#[skuld::test]
fn map_state_maps_running_and_stopped_directly() {
    assert_eq!(map_state(WinState::Running), State::Running);
    assert_eq!(map_state(WinState::Stopped), State::Stopped);
}

#[skuld::test]
fn map_state_maps_pending_and_paused_to_unknown() {
    for s in [
        WinState::StartPending,
        WinState::StopPending,
        WinState::ContinuePending,
        WinState::PausePending,
        WinState::Paused,
    ] {
        assert_eq!(map_state(s), State::Unknown, "{s:?} should map to Unknown");
    }
}

#[skuld::test]
fn create_maps_no_account_to_null_for_local_system() {
    // `CreateServiceW(NULL)` means LocalSystem -- `windows-service` passes
    // `account_name: None` through as null, which is exactly what's wanted
    // here. `current_start_type: None` signals "create".
    let info = service_info(&dummy_reg(None), None, None);
    assert_eq!(info.account_name, None);
    assert_eq!(info.start_type, ServiceStartType::OnDemand);
}

#[skuld::test]
fn update_spells_out_local_system_explicitly() {
    // `ChangeServiceConfigW(NULL)` means "leave the account unchanged", not
    // LocalSystem -- see `service_info`'s doc comment. A desired
    // `LocalSystem` on the update path must therefore be the literal string.
    let info = service_info(&dummy_reg(None), None, Some(ServiceStartType::OnDemand));
    assert_eq!(info.account_name.as_deref(), Some(std::ffi::OsStr::new("LocalSystem")));
}

#[skuld::test]
fn a_real_account_is_passed_through_on_both_paths() {
    for current_start_type in [None, Some(ServiceStartType::OnDemand)] {
        let info = service_info(&dummy_reg(Some(r".\svc-account")), None, current_start_type);
        assert_eq!(
            info.account_name.as_deref(),
            Some(std::ffi::OsStr::new(r".\svc-account")),
            "current_start_type={current_start_type:?}"
        );
    }
}

#[skuld::test]
fn password_is_only_attached_when_given() {
    let with_password = service_info(&dummy_reg(Some("svc")), Some("hunter2".to_string()), None);
    assert_eq!(
        with_password.account_password.as_deref(),
        Some(std::ffi::OsStr::new("hunter2"))
    );

    let without_password = service_info(&dummy_reg(Some("svc")), None, None);
    assert_eq!(without_password.account_password, None);
}

#[skuld::test]
fn create_always_uses_demand_start() {
    let info = service_info(&dummy_reg(None), None, None);
    assert_eq!(info.start_type, ServiceStartType::OnDemand);
}

#[skuld::test]
fn update_preserves_whatever_start_type_is_already_live() {
    // A routine spec-driven update must not silently undo a prior `enable`
    // (SERVICE_AUTO_START) or `disable` (SERVICE_DEMAND_START) -- `windows-service`'s
    // `change_config` always sends a real `dwStartType`, never
    // `SERVICE_NO_CHANGE`, so `service_info` must restate whatever
    // `current_start_type` already says.
    for start_type in [ServiceStartType::OnDemand, ServiceStartType::AutoStart] {
        let info = service_info(&dummy_reg(None), None, Some(start_type));
        assert_eq!(info.start_type, start_type);
    }
}

#[skuld::test]
fn is_not_found_matches_only_error_service_does_not_exist() {
    let not_found = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1060));
    assert!(is_not_found(&not_found));

    let access_denied = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(5));
    assert!(!is_not_found(&access_denied));
}

// discover ============================================================================================================

/// `discover` reads the drift text for every ownership, but [`decide::decide`] consults it only for
/// `Ours`. So when the marker's demonstrable *absence* has already settled the answer, a failure to
/// read that text must not overwrite the refusal with a claim of ownership goetia has just
/// disproved — which is what `to_error`'s `Error::Other` does, via
/// `cli::report::status_error`'s catch-all `Kind::Unreadable` ("goetia owns this and cannot report
/// on it").
#[skuld::test]
fn a_failed_drift_read_over_a_foreign_service_stays_foreign() {
    let id = Id::try_from("x").expect("valid id");
    let failure = || {
        to_error(
            "query configuration for `x`",
            windows_service::Error::Winapi(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)),
        )
    };

    let over_foreign = drift_read_failure(&Ownership::Foreign, &id, failure());
    let Error::Foreign { id: reported, recovery } = &over_foreign else {
        panic!(
            "a missing marker proves the id is not goetia's, and a failed read does not un-prove it: {over_foreign:?}"
        );
    };
    assert_eq!(reported, "x");
    assert_eq!(
        recovery,
        &decide::foreign_recovery("x"),
        "the identical remedy `Outcome::RefuseForeign` would have carried"
    );

    // Ownership *is* established here — the marker is present, merely undecodable — so
    // `Kind::Unreadable` is a true statement about the id and the original error stands.
    let over_ours = drift_read_failure(
        &Ownership::OursUnreadable {
            reason: "a newer schema".to_string(),
        },
        &id,
        failure(),
    );
    assert!(matches!(over_ours, Error::Other(_)), "{over_ours:?}");
}

// undetermined ========================================================================================================

/// The substance `manager::fake`'s own `Error::Undetermined` carries, asserted of this backend's
/// constructor — as `the_launchd_undetermined_recovery_names_both_causes_and_not_uninstall` asserts
/// it of launchd's. The three describe one condition and must agree about it without sharing a
/// function. Split across the cause rather than crammed into one sentence — elevation is advice only
/// a permission boundary earns, and offering it for a failing disk sends the reader somewhere
/// useless — so "both causes" means the pair covers both, one each. Never `uninstall`, in any of
/// them: that is `Outcome::RefuseUnreadable`'s remedy, and it certifies the ownership this read
/// never established.
#[skuld::test]
fn the_scm_undetermined_recovery_names_both_causes_and_not_uninstall() {
    // 1117 is ERROR_IO_DEVICE: a read that did not complete for a reason no amount of privilege
    // fixes, so the class must not be keyed on the denial even though the advice is.
    for (raw, wants_elevation) in [(ERROR_ACCESS_DENIED as i32, true), (1117, false)] {
        let e = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(raw));
        let Error::Undetermined { id, reason, recovery } = service_undetermined("x", "open service `x`", &e) else {
            panic!("{raw}: every non-`is_not_found` failure is undetermined, not just a denial");
        };

        assert_eq!(id, "x");
        assert!(reason.contains("open service `x`"), "{raw}: {reason}");
        assert!(recovery.contains("re-run"), "{raw}: {recovery}");
        assert_eq!(
            recovery.contains("re-run as Administrator"),
            wants_elevation,
            "{raw}: {recovery}"
        );
        assert!(
            !recovery.contains("uninstall"),
            "{raw}: uninstall certifies ownership this read never established: {recovery}"
        );
    }
}

#[skuld::test]
fn is_access_denied_matches_only_error_access_denied() {
    let denied = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
    assert!(is_access_denied(&denied));

    let not_found = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(1060));
    assert!(!is_access_denied(&not_found));

    assert!(!is_access_denied(&windows_service::Error::LaunchArgumentsNotSupported));
}

// list's aggregate ====================================================================================================

/// `list` collects one [`Unreadable`] per service it could not read; only the name matters to a
/// caller about to assert on the count, so the cause and the remedy are the denial case's stock
/// pair — what `registry_undetermined` builds for an `ERROR_ACCESS_DENIED`.
fn names(items: &[&str]) -> Vec<Unreadable> {
    items.iter().map(|name| denied(name)).collect()
}

/// The [`Unreadable`] a denied `Parameters` read produces, built through the same [`undetermined`]
/// the production path uses so the remedy under test is the one `status` would render, not a copy.
fn denied(name: &str) -> Unreadable {
    unreadable_parts(
        name.to_string(),
        undetermined(
            name,
            format!("registry open HKLM\\...\\{name}\\Parameters: denied"),
            true,
        ),
    )
}

/// The count is data, not a diagnostic: it reaches the caller as an entry, whose null name is what
/// obliges every consumer to stop concluding absence (see `Installed::Undetermined`).
#[skuld::test]
fn an_aggregate_undetermined_entry_has_a_null_name_and_names_its_count() {
    match unreadable_aggregate(names(&["alpha", "bravo", "charlie"])) {
        Some(Installed::Undetermined { name, reason }) => {
            assert_eq!(
                name, None,
                "one entry stands for 3 ids, so there is no single id to name"
            );
            assert_eq!(reason, unreadable_notice(3), "the entry carries the notice verbatim");
        }
        other => panic!("services goetia could not classify establish neither presence nor absence: {other:?}"),
    }
}

/// The invariant `Installed::Undetermined` states and the aggregate used to break: *an entry for
/// exactly one known id always names it*. The name is in hand at the call site, and a null one is
/// not a stricter report but a wrong one — it withholds every negative answer about every other id
/// on the host, when the only thing goetia failed to read was this service.
#[skuld::test]
fn an_aggregate_standing_for_one_service_names_it() {
    let one = denied("MsSecFlt");
    match unreadable_aggregate(vec![denied("MsSecFlt")]) {
        Some(Installed::Undetermined { name, reason }) => {
            assert_eq!(name.as_deref(), Some("MsSecFlt"));
            assert_eq!(
                reason,
                named_unreadable_notice(&one.detail, one.recovery.as_deref()),
                "the entry carries the notice verbatim"
            );
        }
        other => panic!("one unreadable service is still undetermined, named: {other:?}"),
    }
}

/// The remedy on a named entry is the one the *errno* earned, not the one the common case usually
/// wants. `undetermined` picks between them for `status` — "elevation is advice only a permission
/// boundary earns" is its own doc comment — and a named `list` entry stands for exactly the same
/// single failed read, so it has the same errno in hand and no reason to answer differently. A
/// corrupt hive on an already-elevated host used to be told to re-run elevated by `list` while
/// `status <same-id>` told it to fix the disk: two answers on one condition.
#[skuld::test]
fn a_named_entry_carries_the_remedy_the_errno_earned_not_elevation() {
    let hive = unreadable_parts(
        "MsSecFlt".to_string(),
        undetermined(
            "MsSecFlt",
            r"registry open HKLM\...\MsSecFlt\Parameters: the hive is corrupt".to_string(),
            false,
        ),
    );
    let Some(Installed::Undetermined { reason, .. }) = unreadable_aggregate(vec![hive]) else {
        panic!("one unreadable service is undetermined, named");
    };
    assert!(
        reason.contains("resolve that failure and re-run"),
        "a failure that was not a denial gets the remedy `undetermined` chose for it: {reason}"
    );
    assert!(
        !reason.contains("elevated") && !reason.contains("Administrator"),
        "elevation is advice only a permission boundary earns, and this read hit a corrupt hive: {reason}"
    );
}

/// The same claim stated as the identity it rests on: whatever `undetermined` put in `recovery` —
/// which is what `Error::Undetermined`'s `Display` renders for `status` — is what the named `list`
/// entry carries. Pinning containment rather than the wording keeps the two verbs tied together
/// when either sentence is later reworded.
#[skuld::test]
fn a_named_entry_offers_the_same_remedy_status_renders() {
    for access_denied in [true, false] {
        let e = undetermined(
            "MsSecFlt",
            r"registry open HKLM\...\Parameters: failed".to_string(),
            access_denied,
        );
        let Error::Undetermined { recovery, .. } = &e else {
            panic!("`undetermined` builds that variant and no other");
        };
        let recovery = recovery.clone();
        let Some(Installed::Undetermined { reason, .. }) =
            unreadable_aggregate(vec![unreadable_parts("MsSecFlt".to_string(), e)])
        else {
            panic!("one unreadable service is undetermined, named");
        };
        assert!(
            reason.contains(&recovery),
            "`list` and `status` answer one condition once (access_denied={access_denied}): {reason}"
        );
    }
}

/// A failure that arrived without a conditioned remedy gets none invented for it. `read_parameters`
/// builds every failure it has through `registry_undetermined`, so this arm is unreachable there;
/// what it pins is that the fallback stays honest rather than defaulting to the elevation advice
/// that used to be unconditional.
#[skuld::test]
fn a_cause_that_carries_no_remedy_is_given_none() {
    let parts = unreadable_parts("MsSecFlt".to_string(), Error::Other("the hive is corrupt".to_string()));
    assert_eq!(parts.recovery, None, "no remedy was established, so none is offered");

    let Some(Installed::Undetermined { reason, .. }) = unreadable_aggregate(vec![parts]) else {
        panic!("one unreadable service is undetermined, named");
    };
    assert!(
        reason.contains("the hive is corrupt"),
        "the cause still travels: {reason}"
    );
    assert!(
        !reason.contains("elevated") && !reason.contains("Administrator"),
        "a remedy this never established must not be invented: {reason}"
    );
}

/// The other half of naming it: the *text* has to be about the service the entry names. The
/// aggregate wording says a daemon may be missing from the list, which is exactly what a named
/// entry has ruled out — nothing is missing, it is right there — and `cli::support` prints this
/// after the name, so the two would contradict each other in one line.
#[skuld::test]
fn a_named_entry_does_not_carry_the_aggregate_claim() {
    let Some(Installed::Undetermined { reason, .. }) = unreadable_aggregate(vec![denied("MsSecFlt")]) else {
        panic!("one unreadable service is undetermined");
    };
    // `Err(_)` used to throw the cause away here and hand the entry a static sentence — the one
    // thing `service_detail`'s own doc comment calls useless for diagnosing a real Win32 failure.
    assert!(
        reason.contains(r"registry open HKLM\...\MsSecFlt\Parameters: denied"),
        "the read that failed is the diagnosable part, and one entry has room for it: {reason}"
    );
    assert!(
        !reason.contains("may be missing"),
        "the entry names the one service it stands for, so nothing is missing from the list: {reason}"
    );
    assert!(
        !reason.contains('\n') && !reason.contains("1 service"),
        "a named entry states its own case rather than counting: {reason}"
    );
    // What it does still have to say: ownership is unknown, not refused, and how to clear it — the
    // denied case's remedy here, since that is the failure `denied` builds.
    assert!(reason.contains("unknown"), "{reason}");
    assert!(reason.contains("re-run as Administrator"), "{reason}");
}

/// Both arms of the unwrapping that keeps the cause: the `Undetermined` `read_parameters` actually
/// builds is split into the cause and the remedy it chose, without the variant's own `id` prose —
/// which `cli::support` would render a second time after the name it already prints — and anything
/// else still says what it was rather than nothing.
#[skuld::test]
fn a_failed_parameters_read_keeps_its_rendered_cause() {
    let carried = unreadable_parts(
        "MsSecFlt".to_string(),
        undetermined(
            "MsSecFlt",
            r"registry open HKLM\SYSTEM\CurrentControlSet\Services\MsSecFlt\Parameters: denied".to_string(),
            true,
        ),
    );
    assert_eq!(carried.name, "MsSecFlt");
    assert_eq!(
        carried.detail,
        r"registry open HKLM\SYSTEM\CurrentControlSet\Services\MsSecFlt\Parameters: denied"
    );
    assert!(
        carried.recovery.is_some_and(|r| r.contains("re-run as Administrator")),
        "the remedy the errno earned travels with the cause"
    );

    let other = unreadable_parts("MsSecFlt".to_string(), Error::Other("the hive is corrupt".to_string()));
    assert!(other.detail.contains("the hive is corrupt"));
}

/// The other half, and the one an over-eager fix would break: a host every one of whose services
/// goetia read leaves nothing undetermined, so `list` must add no entry and `daemon list` must not
/// exit `4` forever.
#[skuld::test]
fn no_aggregate_entry_is_emitted_when_nothing_was_unreadable() {
    assert!(unreadable_aggregate(Vec::new()).is_none());
}

#[skuld::test]
fn unreadable_notice_is_one_line_and_says_what_it_stands_for() {
    // The wording carries the honest part of this diagnostic — that ownership
    // is *unknown*, not that the services are foreign — so pin it rather than
    // let a later reword quietly turn it into a reassuring lie.
    let denied = denied("MsSecFlt");
    let one = named_unreadable_notice(&denied.detail, denied.recovery.as_deref());
    assert!(
        !one.contains("could not be inspected"),
        "a named entry is about its own service, not about a count of them: {one}"
    );

    let many = unreadable_notice(3);
    assert!(many.contains("3 services "), "plural noun: {many}");
    assert!(many.contains("unknown for them"), "plural pronoun: {many}");
    assert!(
        many.contains("may be missing"),
        "an entry that names nobody cannot say the list is complete: {many}"
    );

    // Each offers a remedy, but not the same one, and the difference is the point: the count has no
    // errno to condition on and hedges, while the named entry has exactly one and uses it.
    assert!(
        many.contains("re-running elevated"),
        "a count can still offer the usual remedy: {many}"
    );
    assert!(
        one.contains("re-run as Administrator"),
        "one denied read earns the remedy `undetermined` chose for it: {one}"
    );

    for text in [&one, &many] {
        assert!(!text.contains('\n'), "must stay one line: {text}");
        // `list` counts every non-`NotFound` failure, so naming denial as *the* cause would assert
        // something this never established — see `unreadable_notice`'s own doc comment.
        assert!(
            !text.contains("access denied"),
            "the count covers failures that were not denials: {text}"
        );
    }
}

/// The wiring that turns "the enumeration stopped early" into the entry a caller sees. Distinct
/// from `unreadable_aggregate`'s entry above — that one stands for services this pass *reached* and
/// could not read, this one for services it never reached — and both have to appear, since only
/// reporting them separately keeps each count honest.
///
/// Handed a scan rather than reading one: the `Services` key opens and enumerates on every host
/// that boots (`registry_tests.rs::the_services_key_enumerates_and_finishes`), so `list` itself
/// cannot be made to produce this without breaking the machine under the test.
#[skuld::test]
fn a_scan_that_stopped_early_becomes_an_unnamed_entry_in_the_listing() {
    let scan = registry::ServiceScan {
        names: Vec::new(),
        incomplete: Some("registry enumerate HKLM\\...: injected".to_string()),
    };

    let listed = classify_scan(scan).expect("a pass that stopped early reports, it does not fail");

    match listed.as_slice() {
        [Installed::Undetermined { name: None, reason }] => {
            assert!(reason.contains("injected"), "the detail must survive: {reason}");
            assert!(
                reason.contains(registry::SERVICES_KEY),
                "the entry must name what was being scanned: {reason}"
            );
        }
        other => panic!("what a pass never reached is one unnamed entry, not {other:?}"),
    }
}

/// The other half: a pass that finished over an empty host adds nothing at all, so an ordinary
/// `daemon list` is an empty document at exit `0` rather than a permanent `4`.
#[skuld::test]
fn a_scan_that_finished_over_nothing_adds_no_entry() {
    let scan = registry::ServiceScan {
        names: Vec::new(),
        incomplete: None,
    };

    assert!(classify_scan(scan).expect("list").is_empty());
}
