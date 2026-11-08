//! Pure unit tests: no real SCM/registry access, so these run unelevated
//! alongside every other `#[skuld::test]` in the library. Elevated behavior
//! (everything that actually talks to SCM) is covered by
//! `tests/scm_integration.rs` instead.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::*;
use crate::backend::scm::parameters::Stray;
use crate::manager::ServiceManager;
use crate::shim::{CheckPermissions, FolderCheck, HostCommand, ShimProvider, SweepFailure};
use crate::spec::Restart;
use crate::spec::test_support::{plain_fields, program};
use crate::spec::{AccountId, DaemonFields, User};
use windows_sys::Win32::System::Registry::REG_DWORD;

fn dummy_reg(account: Option<&str>) -> ScmRegistration {
    ScmRegistration {
        name: "goetia-test".to_string(),
        display_name: "Goetia Test".to_string(),
        executable: PathBuf::from(r"C:\bin\test.exe"),
        arguments: vec![],
        account: account.map(str::to_string),
        failure_actions: None,
        environment: vec![],
        parameters: BTreeMap::new(),
        stray_parameters: Vec::new(),
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

// undetermined ========================================================================================================

/// The substance `manager::fake`'s own `Error::Undetermined` carries, asserted of this backend's
/// constructor — as `the_launchd_undetermined_recovery_names_both_causes_and_not_uninstall` asserts
/// it of launchd's. The three describe one condition and must agree about it without sharing a
/// function. Split across the cause rather than crammed into one sentence — elevation is advice only
/// a permission boundary earns, and offering it for a failing disk sends the reader somewhere
/// useless — so "both causes" means the pair covers both, one each. Never `uninstall`, in any of
/// them: that is `Outcome::RefuseUnusable`'s remedy, and it certifies the ownership this read
/// never established.
#[skuld::test]
fn the_scm_undetermined_recovery_names_both_causes_and_not_uninstall() {
    // 1117 is ERROR_IO_DEVICE: a read that did not complete for a reason no amount of privilege
    // fixes, so the class must not be keyed on the denial even though the advice is.
    for (raw, wants_elevation) in [(ERROR_ACCESS_DENIED as i32, true), (1117, false)] {
        let e = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(raw));
        let Error::Undetermined { id, reason, recovery } = service_undetermined("x", "open service `x`", e) else {
            panic!("{raw}: every non-`is_not_found` failure is undetermined, not just a denial");
        };

        assert_eq!(id, "x");
        let reason = reason.report().to_string();
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

/// What a failed open of `name`'s `Parameters` key was, for [`undetermined`] to wrap.
fn registry_failure(name: &str, cause: &str) -> Error {
    Error::os(
        format!("open HKLM\\...\\{name}\\Parameters"),
        std::io::Error::other(cause.to_string()),
    )
}

/// The [`Unreadable`] a denied `Parameters` read produces, built through the same [`undetermined`]
/// the production path uses so the remedy under test is the one `status` would render, not a copy.
fn denied(name: &str) -> Unreadable {
    unreadable_parts(
        name.to_string(),
        undetermined(name, registry_failure(name, "denied"), true),
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
            assert_eq!(
                reason.to_string(),
                unreadable_notice(3),
                "the entry carries the notice verbatim"
            );
        }
        other => panic!("services goetia could not classify establish neither presence nor absence: {other:?}"),
    }
}

/// The invariant `Installed::Undetermined` states: *an entry for exactly one known id always names
/// it*. The name is in hand at the call site, and a null one is
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
                named_unreadable_notice(one.detail, one.recovery.as_deref()),
                "the entry carries the notice verbatim"
            );
        }
        other => panic!("one unreadable service is still undetermined, named: {other:?}"),
    }
}

/// The remedy on a named entry is the one the *errno* earned, not the one the common case usually
/// wants. `undetermined` picks between them for `status` — "elevation is advice only a permission
/// boundary earns" is its own doc comment — and a named `list` entry stands for exactly the same
/// single failed read, so it has the same errno in hand and no reason to answer differently. On a
/// corrupt hive on an already-elevated host, `list` and `status <same-id>` both say to fix the disk,
/// not to re-run elevated.
#[skuld::test]
fn a_named_entry_carries_the_remedy_the_errno_earned_not_elevation() {
    let hive = unreadable_parts(
        "MsSecFlt".to_string(),
        undetermined("MsSecFlt", registry_failure("MsSecFlt", "the hive is corrupt"), false),
    );
    let Some(Installed::Undetermined { reason, .. }) = unreadable_aggregate(vec![hive]) else {
        panic!("one unreadable service is undetermined, named");
    };
    let reason = reason.to_string();
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
        let e = undetermined("MsSecFlt", registry_failure("MsSecFlt", "failed"), access_denied);
        let Error::Undetermined { recovery, .. } = &e else {
            panic!("`undetermined` builds that variant and no other");
        };
        let recovery = recovery.clone();
        let Some(Installed::Undetermined { reason, .. }) =
            unreadable_aggregate(vec![unreadable_parts("MsSecFlt".to_string(), e)])
        else {
            panic!("one unreadable service is undetermined, named");
        };
        let reason = reason.to_string();
        assert!(
            reason.contains(&recovery),
            "`list` and `status` answer one condition once (access_denied={access_denied}): {reason}"
        );
    }
}

/// A failure that arrived without a conditioned remedy gets none invented for it. `unreadable_parts`
/// never makes one (`read_parameters` builds every failure through `registry_undetermined`); what
/// this pins is that the aggregate stays honest rather than defaulting to the elevation advice.
#[skuld::test]
fn a_cause_that_carries_no_remedy_is_given_none() {
    let parts = Unreadable {
        name: "MsSecFlt".to_string(),
        detail: Error::os("read the hive", std::io::Error::other("the hive is corrupt")),
        recovery: None,
    };

    let Some(Installed::Undetermined { reason, .. }) = unreadable_aggregate(vec![parts]) else {
        panic!("one unreadable service is undetermined, named");
    };
    let reason = reason.to_string();
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
    let reason = reason.to_string();
    // The cause is kept: a static sentence says nothing of a real Win32 failure.
    assert!(
        reason.contains(r"failed to open HKLM\...\MsSecFlt\Parameters: denied"),
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

/// The `Undetermined` `read_parameters` actually builds is split into the cause and the remedy it
/// chose, without the variant's own `id` prose, which `cli::support` would render a second time
/// after the name it already prints.
#[skuld::test]
fn a_failed_parameters_read_keeps_its_rendered_cause() {
    let carried = unreadable_parts(
        "MsSecFlt".to_string(),
        undetermined(
            "MsSecFlt",
            Error::os(
                r"open HKLM\SYSTEM\CurrentControlSet\Services\MsSecFlt\Parameters",
                std::io::Error::other("denied"),
            ),
            true,
        ),
    );
    assert_eq!(carried.name, "MsSecFlt");
    assert_eq!(
        carried.detail.report().to_string(),
        r"failed to open HKLM\SYSTEM\CurrentControlSet\Services\MsSecFlt\Parameters: denied"
    );
    assert!(
        carried.recovery.is_some_and(|r| r.contains("re-run as Administrator")),
        "the remedy the errno earned travels with the cause"
    );
}

/// A failure the read was wrapped in on its way up is looked through: the remedy is the inner
/// `Undetermined`'s, and the wrapper stays around its read.
#[skuld::test]
fn a_wrapped_parameters_read_failure_keeps_the_inner_cause_and_remedy() {
    let wrapped = Error::Step {
        what: "read the parameters".to_string(),
        source: Box::new(undetermined(
            "MsSecFlt",
            Error::os("read the key", std::io::Error::other("unreadable")),
            true,
        )),
    };

    let parts = unreadable_parts("MsSecFlt".to_string(), wrapped);

    assert_eq!(
        parts.detail.report().to_string(),
        "read the parameters: failed to read the key: unreadable"
    );
    assert!(
        parts.recovery.is_some_and(|r| r.contains("re-run as Administrator")),
        "the remedy of the inner `Undetermined` travels"
    );
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
    let one = named_unreadable_notice(denied.detail, denied.recovery.as_deref()).to_string();
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

/// What the `Parameters` of an openable service say about ownership. The snapshot is the map as it
/// was read, so `installed` compares what `read` saw.
#[skuld::test]
fn read_marker_sorts_parameters_into_foreign_and_marked() {
    let spec = blob_of(Kind::Managed).spec;
    let ours = Parameters {
        fields: BTreeMap::from([
            ("Marker".to_string(), crate::blob::MARKER.to_string()),
            ("Schema".to_string(), crate::blob::SCHEMA.to_string()),
            ("Version".to_string(), crate::version().to_string()),
            ("Spec".to_string(), crate::blob::encode(&spec)),
        ]),
        strays: vec![Stray::Subkey { name: vec![0x41] }],
    };
    let torn = Parameters {
        fields: BTreeMap::from([("Marker".to_string(), crate::blob::MARKER.to_string())]),
        strays: Vec::new(),
    };

    let id = Id::try_from("frpc").unwrap();
    assert!(matches!(
        read_marker(Parameters::default(), None),
        ReadOutcome::Foreign { recovery: None }
    ));
    // A `Marker` that is no goetia string reads as a stray, not a field: no marker.
    let mut foreign = Parameters::default();
    foreign.add_value("Marker".encode_utf16().collect(), REG_DWORD, &[1, 0, 0, 0]);
    assert!(matches!(
        read_marker(foreign, None),
        ReadOutcome::Foreign { recovery: None }
    ));
    match read_marker(ours.clone(), None).named(&id, ScmSnapshot::new) {
        ReadOutcome::Marked(snapshot) => {
            assert_eq!(snapshot.id().as_str(), "frpc");
            assert_eq!(snapshot.blob().expect("decoded").spec, spec);
            assert_eq!(snapshot.parameters, ours);
        }
        _ => panic!("a complete marker is goetia's, decoded"),
    }
    match read_marker(torn.clone(), None).named(&id, ScmSnapshot::new) {
        ReadOutcome::Marked(snapshot) => {
            assert!(snapshot.blob().is_err());
            assert_eq!(snapshot.parameters, torn);
        }
        _ => panic!("the marker alone is proof of ownership, decodable or not"),
    }
}

// Install refuses `restart: always` ===================================================================================

fn daemon_spec(restart: crate::spec::Restart) -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        restart,
        ..plain_fields("frpc", vec![r"C:\bin\frpc.exe".to_string()])
    })
    .unwrap()
}

// start_kind ----------------------------------------------------------------------------------------------------------

fn blob_of(kind: Kind) -> Blob {
    Blob {
        schema: 1,
        version: "0.1.0".to_string(),
        spec: DaemonSpec::new(DaemonFields {
            kind,
            ..plain_fields("frpc", vec![program("frpc")])
        })
        .unwrap(),
    }
}

#[skuld::test]
fn start_reads_a_simple_service_by_the_shims_vocabulary_and_anything_else_strictly() {
    for kind in [Kind::Simple, Kind::Managed] {
        let (read, why) = start_kind(Ok(&blob_of(kind)));
        assert_eq!(read, kind);
        assert!(why.is_none(), "{why:?}");
    }
    let (kind, why) = start_kind(Err(&Arc::new(Error::blob("corrupt"))));
    assert_eq!(kind, Kind::Managed);
    assert!(matches!(why.as_deref(), Some(Error::Blob { message, .. }) if message == "corrupt"));
}

/// A start judged strictly because the blob could not be read says why, so the user is not told a
/// `type: simple` daemon failed to start in a way that only a managed service's start can.
#[skuld::test]
fn a_failed_start_names_the_blob_read_error_that_made_it_strict() {
    let id = Id::try_from("frpc").unwrap();
    let e = || std::io::Error::other("service stopped before reaching Running");

    let plain = failed_start(&id, e(), None).report().to_string();
    assert!(!plain.contains("blob"), "{plain}");

    let strict = failed_start(&id, e(), Some(Arc::new(Error::blob("invalid blob JSON: EOF"))))
        .report()
        .to_string();
    assert!(strict.contains("service stopped before reaching Running"), "{strict}");
    assert!(strict.contains("could not be read"), "{strict}");
    assert!(strict.contains("invalid blob JSON: EOF"), "{strict}");
}

/// A blob that says `restart: always` decodes, and a `type: simple` one starts by the shim's vocabulary like
/// any other.
#[skuld::test]
fn a_blob_that_says_always_decodes_and_starts_as_its_kind() {
    let spec = DaemonSpec::new(DaemonFields {
        restart: Restart::Always,
        ..blob_of(Kind::Simple).spec.into_fields()
    })
    .unwrap();
    // Hand-write the blob: `registration` refuses `always`.
    let parameters = BTreeMap::from([
        ("Marker".to_string(), crate::blob::MARKER.to_string()),
        ("Schema".to_string(), crate::blob::SCHEMA.to_string()),
        ("Version".to_string(), crate::version().to_string()),
        ("Spec".to_string(), crate::blob::encode(&spec)),
    ]);

    let blob = generate::extract(&parameters, &[])
        .expect("the blob decodes")
        .expect("the blob is ours");

    assert_eq!(blob.spec.restart, Restart::Always);
    let (kind, why) = start_kind(Ok(&blob));
    assert_eq!(kind, Kind::Simple);
    assert!(why.is_none(), "{why:?}");
}

// The generator's refusal at its call sites ---------------------------------------------------------------------------

/// `install` and `preview_install` regenerate the desired registration before anything reads the SCM,
/// so the generator's refusal is reachable without one.
#[skuld::test]
fn install_and_preview_install_refuse_restart_always() {
    let mgr = Scm::new();
    let spec = daemon_spec(Restart::Always);

    for result in [
        mgr.install(&spec, false).map(|_| ()),
        mgr.preview_install(&spec).map(|_| ()),
    ] {
        let err = result.expect_err("`restart: always` must be refused for scm");
        assert!(matches!(err, Error::Invalid { .. }), "{err:?}");
        assert!(err.to_string().contains("restart: always"), "{err}");
    }
}

// The shim provider ---------------------------------------------------------------------------------------------------

fn simple_fields() -> DaemonFields {
    DaemonFields {
        kind: Kind::Simple,
        ..plain_fields("frpc", vec![r"C:\bin\frpc.exe".to_string()])
    }
}

fn simple_daemon() -> DaemonSpec {
    DaemonSpec::new(simple_fields()).unwrap()
}

/// A provider that records the calls it gets and answers with the commands it was given.
///
/// The same provider is `Recording` in `tests/shim_integration/provision.rs`, which this module
/// cannot share (an integration test reaches the library only through its public API, and this is
/// `cfg(test)` inside it): keep the two identical in behaviour, but for `later_preview_fails`, which
/// only this module's tests use.
struct Recording {
    calls: Arc<Mutex<Vec<&'static str>>>,
    preview: HostCommand,
    /// What every `preview` after the first answers, for a provider whose answer changes between
    /// the flow's render and the install lock (an upgrade of the sibling shim).
    later_preview: Option<HostCommand>,
    /// Whether every `preview` after the first fails.
    later_preview_fails: bool,
    provide: HostCommand,
}

impl Recording {
    fn new(preview: HostCommand, provide: HostCommand) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::default();
        let provider = Self {
            calls: Arc::clone(&calls),
            preview,
            later_preview: None,
            later_preview_fails: false,
            provide,
        };
        (provider, calls)
    }

    fn previewing_after_the_first(self, later: HostCommand) -> Self {
        Self {
            later_preview: Some(later),
            ..self
        }
    }

    fn failing_to_preview_after_the_first(self) -> Self {
        Self {
            later_preview_fails: true,
            ..self
        }
    }
}

impl ShimProvider for Recording {
    fn preview(&self) -> Result<HostCommand> {
        let mut calls = self.calls.lock().unwrap();
        let first = !calls.contains(&"preview");
        calls.push("preview");
        if !first && self.later_preview_fails {
            return Err(Error::os("locate the host", std::io::Error::other("preview failed")));
        }
        match (&self.later_preview, first) {
            (Some(later), false) => Ok(later.clone()),
            _ => Ok(self.preview.clone()),
        }
    }

    fn provide(&self, _folders: &FolderCheck) -> Result<HostCommand> {
        self.calls.lock().unwrap().push("provide");
        Ok(self.provide.clone())
    }

    fn sweep(&self, _referenced: &[PathBuf]) -> std::result::Result<(), SweepFailure> {
        self.calls.lock().unwrap().push("sweep");
        Ok(())
    }
}

fn command(exe: impl Into<PathBuf>) -> HostCommand {
    HostCommand {
        exe: exe.into(),
        args: vec![],
    }
}

#[skuld::test]
fn new_has_no_provider() {
    let mgr = Scm::new();

    let err = mgr.render(&simple_daemon()).expect_err("a simple service needs a host");
    assert!(matches!(err, Error::NoShimProvider), "{err:?}");
    mgr.render(&daemon_spec(Restart::Never))
        .expect("a managed service needs none");
}

/// With no provider there is nothing of the manager's own to sweep, so the retry of an uninstall
/// for an absent id succeeds without taking the machine-wide lock or reading anything: a library
/// caller that is not elevated keeps getting `NotInstalled` from `uninstall`. (A regression would
/// try to create `Global\goetia-shim`, which fails unelevated, and this would be an error.)
#[skuld::test]
fn uninstall_of_an_absent_id_without_a_provider_takes_no_lock() {
    Scm::new()
        .uninstall_absent(&Id::try_from("never-installed").unwrap())
        .expect("nothing to sweep");
}

#[skuld::test]
fn render_names_the_previewed_host_verbatim() {
    let host = HostCommand {
        exe: PathBuf::from(r"C:\app\app.exe"),
        args: vec!["--service-host".to_string()],
    };
    let (provider, _) = Recording::new(host.clone(), host.clone());
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    let reg = mgr.render(&simple_daemon()).unwrap();

    assert_eq!((reg.executable, reg.arguments), (host.exe, host.args));
}

#[skuld::test]
fn managed_render_never_touches_the_provider() {
    let (provider, calls) = Recording::new(command(r"C:\app\app.exe"), command(r"C:\app\app.exe"));
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    mgr.render(&daemon_spec(Restart::Never)).unwrap();

    assert_eq!(*calls.lock().unwrap(), Vec::<&str>::new());
}

#[skuld::test]
fn managed_begin_install_takes_no_lock_and_calls_no_provider() {
    let (provider, calls) = Recording::new(command(r"C:\app\app.exe"), command(r"C:\app\app.exe"));
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    let spec = daemon_spec(Restart::Never);
    let desired = mgr.render(&spec).unwrap();

    let scope = mgr.begin_install(&spec, &desired).unwrap();

    assert!(scope.is_none(), "a managed install holds no lock");
    assert_eq!(*calls.lock().unwrap(), Vec::<&str>::new());
}

/// A provided host that is the rendered one is accepted without asking the provider again.
#[skuld::test]
fn a_provided_host_that_is_the_rendered_one_is_accepted() {
    let host = command(r"C:\app\host.exe");
    let (provider, calls) = Recording::new(host.clone(), host.clone());
    let rendered = provider.preview().unwrap();

    let accepted = confirm_provided(&provider, rendered, host.clone()).expect("the host agrees");

    assert_eq!(accepted, host);
    assert_eq!(*calls.lock().unwrap(), ["preview"]);
}

/// A `preview` that now names the host `provide` placed says the host changed while the install
/// waited (the sibling shim was upgraded): the caller runs the install again.
#[skuld::test]
fn a_provided_host_the_provider_now_previews_is_a_host_that_changed() {
    let (old, upgraded) = (command(r"C:\app\old.exe"), command(r"C:\app\new.exe"));
    let (provider, _) = Recording::new(old.clone(), upgraded.clone());
    let provider = provider.previewing_after_the_first(upgraded.clone());
    let rendered = provider.preview().unwrap();

    let Err(Error::ShimHostChanged {
        rendered: said,
        provided: gave,
        preview_failed: None,
    }) = confirm_provided(&provider, rendered, upgraded.clone())
    else {
        panic!("a host the provider previews now is a changed host");
    };

    assert_eq!((*said, *gave), (old, upgraded));
}

/// A `preview` that names a third host, neither the rendered nor the provided one, proves no
/// disagreement either: the host changed again, and the install is run again.
#[skuld::test]
fn a_provided_host_that_is_not_the_one_now_previewed_is_still_a_host_that_changed() {
    let (old, upgraded, newer) = (
        command(r"C:\app\old.exe"),
        command(r"C:\app\new.exe"),
        command(r"C:\app\newer.exe"),
    );
    let (provider, _) = Recording::new(old.clone(), upgraded.clone());
    let provider = provider.previewing_after_the_first(newer);
    let rendered = provider.preview().unwrap();

    let Err(Error::ShimHostChanged {
        rendered: said,
        provided: gave,
        preview_failed: None,
    }) = confirm_provided(&provider, rendered, upgraded.clone())
    else {
        panic!("a third host is a changed host");
    };

    assert_eq!((*said, *gave), (old, upgraded));
}

/// A `preview` that fails proves no disagreement: the answer is the changed host, with the failed
/// `preview` as its cause, not the error returned.
#[skuld::test]
fn a_preview_that_fails_after_a_mismatch_is_a_host_that_changed_with_that_cause() {
    let (old, upgraded) = (command(r"C:\app\old.exe"), command(r"C:\app\new.exe"));
    let (provider, _) = Recording::new(old.clone(), upgraded.clone());
    let provider = provider.failing_to_preview_after_the_first();
    let rendered = provider.preview().unwrap();

    let Err(Error::ShimHostChanged {
        rendered: said,
        provided: gave,
        preview_failed: Some(failed),
    }) = confirm_provided(&provider, rendered, upgraded.clone())
    else {
        panic!("a failed preview must leave the changed host, with its error kept");
    };

    assert_eq!((*said, *gave), (old, upgraded));
    assert!(matches!(*failed, Error::Os { .. }), "{failed:?}");
}

/// A `preview` that still names the host it named, while `provide` placed another, is no race: the
/// two disagree, and an install run again would be refused the same way.
#[skuld::test]
fn a_provider_whose_preview_and_provide_disagree_is_refused_as_one_that_disagrees() {
    let (previewed, provided) = (command(r"\\?\C:\app\host.exe"), command(r"C:\app\host.exe"));
    let (provider, _) = Recording::new(previewed.clone(), provided.clone());
    let rendered = provider.preview().unwrap();

    let Err(Error::ShimProviderDisagrees {
        previewed: said,
        provided: gave,
    }) = confirm_provided(&provider, rendered, provided.clone())
    else {
        panic!("a provider that disagrees with itself must be named so");
    };

    assert_eq!((*said, *gave), (previewed, provided));
}

/// Every refusal `render_for_install` makes comes before the lock is taken and before the provider
/// writes: a refused install leaves no file behind. The install flow renders before it begins, and
/// `begin_install` renders nothing.
#[skuld::test]
fn a_refused_simple_install_never_reaches_the_provider() {
    let refused = [
        (
            "restart: always",
            DaemonFields {
                restart: Restart::Always,
                ..simple_fields()
            },
            command(r"C:\app\app.exe"),
        ),
        (
            "an account that does not resolve",
            DaemonFields {
                user: User::Id(AccountId::Uid(1000)),
                ..simple_fields()
            },
            command(r"C:\app\app.exe"),
        ),
        (
            "a host path the registration writer would mangle",
            simple_fields(),
            command(r"C:\Program Files\Goetia\"),
        ),
    ];
    for (what, fields, host) in refused {
        let (provider, calls) = Recording::new(host.clone(), host);
        let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

        let err = mgr
            .install(&DaemonSpec::new(fields).unwrap(), false)
            .map(|_| ())
            .expect_err(what);

        assert!(
            matches!(err, Error::Invalid { .. } | Error::AccountLookup { .. }),
            "{what}: {err:?}"
        );
        let calls = calls.lock().unwrap();
        assert!(
            !calls.contains(&"provide") && !calls.contains(&"sweep"),
            "{what}: a refused install wrote through the provider: {calls:?}"
        );
    }
}

/// A snapshot that has never opened a service, with `config` already queried: any further SCM
/// access would fail or find no such service, so a test that gets an answer shows there was none.
fn snapshot_with_config(spec: &DaemonSpec, command_line: &str) -> ScmSnapshot {
    let blob = Blob {
        schema: 1,
        version: crate::version().to_string(),
        spec: spec.clone(),
    };
    let snapshot = ScmSnapshot::new(spec.id.clone(), Ok(blob), (Parameters::default(), None));
    let config = windows_service::service::ServiceConfig {
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::OnDemand,
        error_control: ServiceErrorControl::Normal,
        executable_path: PathBuf::from(command_line),
        load_order_group: None,
        tag_id: 0,
        dependencies: vec![],
        account_name: None,
        display_name: "frpc".into(),
    };
    assert!(snapshot.config.set(config).is_ok());
    snapshot
}

#[skuld::test]
fn regenerate_keeps_the_installed_host_from_the_configuration_queried_once() {
    let spec = simple_daemon();
    let snapshot = snapshot_with_config(&spec, r#""C:\Program Files\App\app.exe" --service-host"#);
    let (provider, calls) = Recording::new(command(r"C:\elsewhere\host.exe"), command(r"C:\elsewhere\host.exe"));
    let mgr = Scm::with_shim(provider, CheckPermissions::Disable);

    let reg = mgr.regenerate(&spec, &snapshot).unwrap();

    assert_eq!(reg.executable, PathBuf::from(r"C:\Program Files\App\app.exe"));
    assert_eq!(reg.arguments, ["--service-host"]);
    assert_eq!(*calls.lock().unwrap(), Vec::<&str>::new(), "no provider is consulted");
}

/// A stop whose hold expired and whose STOP was sent is the wait that ran out, led by the process SCM
/// still named.
#[skuld::test]
fn an_unconfirmed_stop_names_the_process_scm_still_named_and_is_a_wait_timeout() {
    let budget = Budget::Bounded(std::time::Duration::from_secs(5));

    let error = unconfirmed_stop(&Id::try_from("frpc").unwrap(), 4242, budget);

    assert_eq!(
        error.report().to_string(),
        "goetia could not hold `frpc`'s process before the time ran out (SCM still named process 4242, which was \
         gone), so it cannot confirm that the service's process exited, whether or not SCM reports the service \
         stopped: `frpc` did not report stopped within 5s: goetia stopped waiting. The \
         request was not cancelled, so `frpc` may still reach stopped; `goetia daemon status frpc` shows the \
         manager's current view. Allow longer with `--timeout <duration>`, or wait indefinitely with `--no-timeout`."
    );
    assert!(error.is_indeterminate());
}

// uninstall's failures ================================================================================================

fn uninstall_failure(state: &UninstallFailure) -> String {
    uninstall_failed(
        &Id::try_from("frpc").unwrap(),
        state,
        Error::os("stop `frpc`", std::io::Error::other("does not accept STOP")),
    )
    .report()
    .to_string()
}

#[skuld::test]
fn a_stop_failure_leaves_the_service_disabled_and_names_the_previous_start_type_and_its_command() {
    for (previous, wants) in [
        (
            ServiceStartType::AutoStart,
            ("automatic", "`goetia daemon enable frpc`"),
        ),
        (ServiceStartType::OnDemand, ("manual", "`goetia daemon disable frpc`")),
    ] {
        let text = uninstall_failure(&UninstallFailure::Stop {
            previous: Some(previous),
        });
        assert!(text.contains("does not accept STOP"), "{previous:?}: {text}");
        assert!(text.contains("NOT deleted"), "{previous:?}: {text}");
        assert!(text.contains("disabled"), "{previous:?}: {text}");
        assert!(
            text.contains(&format!("start type was {}", wants.0)),
            "{previous:?}: {text}"
        );
        assert!(text.contains(wants.1), "{previous:?}: {text}");
        assert!(text.contains("goetia daemon uninstall frpc"), "{previous:?}: {text}");
        assert!(text.contains("goetia daemon kill frpc"), "{previous:?}: {text}");
        assert!(!text.contains("as it was"), "nothing is restored: {previous:?}: {text}");
    }
}

#[skuld::test]
fn a_stop_failure_with_an_unknown_previous_start_type_omits_it() {
    let text = uninstall_failure(&UninstallFailure::Stop { previous: None });
    assert!(text.contains("does not accept STOP"), "{text}");
    assert!(text.contains("NOT deleted"), "{text}");
    assert!(text.contains("disabled"), "{text}");
    assert!(text.contains("goetia daemon uninstall frpc"), "{text}");
    assert!(text.contains("goetia daemon kill frpc"), "{text}");
    for absent in ["start type was", "goetia daemon enable", "sets it again"] {
        assert!(!text.contains(absent), "`{absent}` has nothing to name: {text}");
    }
}

/// A retry reads the `disabled` an earlier `uninstall` left, which is not the original type.
#[skuld::test]
fn a_previous_start_type_of_disabled_is_not_presented_as_the_original() {
    for text in [
        uninstall_failure(&UninstallFailure::Stop {
            previous: Some(ServiceStartType::Disabled),
        }),
        uninstall_failure(&UninstallFailure::Delete {
            previous: Some(ServiceStartType::Disabled),
        }),
    ] {
        assert!(text.contains("NOT deleted"), "{text}");
        assert!(text.contains("original start type is unknown"), "{text}");
        assert!(text.contains("earlier `uninstall`"), "{text}");
        assert!(text.contains("`goetia daemon enable frpc`"), "{text}");
        assert!(text.contains("`goetia daemon disable frpc`"), "{text}");
        for absent in ["start type was", "sets it again", "sc.exe"] {
            assert!(
                !text.contains(absent),
                "`{absent}` misleads for a disabled service: {text}"
            );
        }
    }
}

#[skuld::test]
fn is_not_found_io_matches_the_actor_open_error_for_a_gone_service() {
    assert!(is_not_found_io(&std::io::Error::from_raw_os_error(1060)));
    assert!(!is_not_found_io(&std::io::Error::from_raw_os_error(5)));
}

#[skuld::test]
fn a_delete_failure_says_stopped_disabled_not_deleted_and_names_the_previous_start_type() {
    let text = uninstall_failure(&UninstallFailure::Delete {
        previous: Some(ServiceStartType::AutoStart),
    });
    assert!(text.contains("does not accept STOP"), "the cause leads: {text}");
    assert!(text.contains("stopped and disabled"), "{text}");
    assert!(text.contains("NOT deleted"), "{text}");
    assert!(text.contains("goetia daemon uninstall frpc"), "{text}");
    assert!(text.contains("start type was automatic"), "{text}");
    assert!(text.contains("`goetia daemon enable frpc`"), "{text}");
    assert!(!text.contains("goetia daemon kill"), "the service is stopped: {text}");

    let unknown = uninstall_failure(&UninstallFailure::Delete { previous: None });
    assert!(unknown.contains("NOT deleted"), "{unknown}");
    assert!(!unknown.contains("start type was"), "{unknown}");
}

#[skuld::test]
fn a_stop_failure_on_a_service_marked_by_someone_else_does_not_claim_a_disable() {
    let text = uninstall_failure(&UninstallFailure::AlreadyMarked);
    assert!(text.contains("does not accept STOP"), "{text}");
    assert!(text.contains("already marked for deletion"), "{text}");
    assert!(text.contains("`uninstall` did not delete it"), "{text}");
    assert!(!text.contains("changed nothing"), "a STOP may have been sent: {text}");
    assert!(text.contains("goetia daemon kill frpc"), "{text}");
    for stale in ["NOT deleted", "disabled", "goetia daemon enable", "goetia daemon start"] {
        assert!(!text.contains(stale), "`{stale}` is false for a marked service: {text}");
    }
}
