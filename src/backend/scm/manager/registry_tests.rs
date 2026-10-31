use super::*;

use winreg::types::ToRegValue as _;

#[skuld::test]
fn service_key_path_is_under_current_control_set_services() {
    assert_eq!(
        service_key_path("goetia-test"),
        r"SYSTEM\CurrentControlSet\Services\goetia-test"
    );
}

#[skuld::test]
fn parameters_key_path_is_the_service_key_plus_parameters() {
    assert_eq!(
        parameters_key_path("goetia-test"),
        r"SYSTEM\CurrentControlSet\Services\goetia-test\Parameters"
    );
}

#[skuld::test]
fn an_absent_environment_value_is_no_lines() {
    assert_eq!(environment_lines(None), Vec::<String>::new());
}

#[skuld::test]
fn a_multi_string_environment_value_is_its_lines() {
    let lines = vec!["A=1".to_string(), "B=x y".to_string()];
    let value = lines.to_reg_value();
    assert_eq!(environment_lines(Some(value)), ["A=1", "B=x y"]);
}

/// `winreg` reads an empty `REG_MULTI_SZ` as one empty string; it is no lines, as `write_environment`
/// treats it (an empty list deletes the value).
#[skuld::test]
fn an_empty_multi_string_environment_value_is_no_lines() {
    let lines = Vec::<String>::new();
    let value = lines.to_reg_value();
    assert_eq!(environment_lines(Some(value)), Vec::<String>::new());
}

/// A rendered line is always `NAME=value`, so a line without `=` can never equal desired text: a
/// value of the wrong type is a `Conflict` that names its type, and `--force` rewrites it.
#[skuld::test]
fn an_environment_value_of_another_type_is_one_marker_line_naming_its_type() {
    let value = "A=1".to_reg_value();
    let lines = environment_lines(Some(value));

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].contains('='), "{lines:?}");
    assert!(lines[0].contains("REG_SZ"), "{lines:?}");
}

/// The `Parameters` half of this backend's [`Error::Undetermined`] keying. `winreg` surfaces
/// `ERROR_ACCESS_DENIED` as `ErrorKind::PermissionDenied`, and that chooses the *recovery* alone —
/// the class is the same either way, because a corrupt hive leaves goetia exactly as ignorant of
/// the id as a denial does.
#[skuld::test]
fn a_parameters_read_that_did_not_complete_is_undetermined() {
    let path = parameters_key_path("x");
    for (source, wants_elevation) in [
        (std::io::Error::from(std::io::ErrorKind::PermissionDenied), true),
        (std::io::Error::other("the hive is corrupt"), false),
    ] {
        let rendered = format!("{source:?}");
        let Error::Undetermined { id, reason, recovery } = registry_undetermined("x", "open", &path, &source) else {
            panic!("{rendered}: a read that did not complete establishes nothing about the id");
        };

        assert_eq!(id, "x");
        assert!(
            reason.contains(&path),
            "{rendered}: the reason must name the key: {reason}"
        );
        assert_eq!(
            recovery.contains("re-run as Administrator"),
            wants_elevation,
            "{rendered}: {recovery}"
        );
    }
}

/// The mid-pass fault, and the reason `list` no longer propagates one: a key that could not be
/// enumerated must not take down the services the very same pass already named. Injected through
/// `collect_service_names`' iterator because no real `enum_keys` fails on request.
#[skuld::test]
fn a_key_that_cannot_be_enumerated_keeps_what_the_scan_already_named() {
    let keys = vec![
        Ok("named-before-the-fault".to_string()),
        Err(std::io::Error::other("injected mid-scan failure")),
        Ok("never-reached".to_string()),
    ];

    let scan = collect_service_names(SERVICES_KEY, keys.into_iter());

    assert_eq!(
        scan.names,
        ["named-before-the-fault"],
        "what the pass named before the fault survives it, and the pass stops there: {scan:?}"
    );
    let detail = scan.incomplete.expect("a pass that stopped early must say so");
    assert!(detail.contains("injected mid-scan failure"), "{detail}");
    assert!(
        detail.contains(SERVICES_KEY),
        "the detail must name what was being scanned: {detail}"
    );
}

#[skuld::test]
fn a_pass_that_finishes_reports_nothing_undetermined() {
    let keys = vec![Ok("a".to_string()), Ok("b".to_string())];

    let scan = collect_service_names(SERVICES_KEY, keys.into_iter());

    assert_eq!(scan.names, ["a", "b"]);
    assert!(
        scan.incomplete.is_none(),
        "a pass that finished has nothing to report: {scan:?}"
    );
}

// The open, through the real `RegKey` =================================================================================
//
// `collect_service_names` above covers the mid-pass fault with a hand-built iterator. These three
// cover the outcome *before* it — what `scan_names_under` makes of the open itself — through the
// real `RegOpenKeyEx`, because the arm that decides "empty and settled" against "empty and
// unfinished" is exactly the one this branch exists to get right, and a hand-built value cannot
// reach it.

/// The production call, against the production key. Every Windows host that boots has `Services`,
/// with services under it, readable at any privilege level — so a pass over it names some and
/// reports nothing outstanding. Without this, no test opens that key at all.
#[skuld::test]
fn the_services_key_enumerates_and_finishes() {
    let scan = list_service_names();

    assert!(
        !scan.names.is_empty(),
        "every Windows host runs services: {:?}",
        scan.names
    );
    assert!(
        scan.incomplete.is_none(),
        "the `Services` key opens and enumerates for any caller: {:?}",
        scan.incomplete
    );
}

/// A key that is not there is *absence, established* — an empty scan with nothing outstanding, so
/// `list` returns an empty document at exit `0` rather than one aggregate entry and a permanent
/// exit `4`. `ServiceManager::list`'s doc comment requires this of every backend's absent container.
#[skuld::test]
fn a_services_key_that_does_not_exist_is_an_empty_settled_scan() {
    let scan = scan_names_under(&format!(r"{SERVICES_KEY}\goetia-no-such-key-b6f0a1c2"));

    assert!(scan.names.is_empty(), "{:?}", scan.names);
    assert!(
        scan.incomplete.is_none(),
        "nothing is registered under a key that does not exist, and that is an answer: {:?}",
        scan.incomplete
    );
}

/// The other outcome, and the one that must never be confused with it: a key that *is* there and
/// will not open leaves the pass unfinished, so the scan carries the entry that forbids concluding
/// any id absent.
///
/// `HKLM\SECURITY` is the probe because its DACL grants `LocalSystem` alone — an elevated
/// Administrator is refused, which is the token CI runs these under, and an unelevated one is
/// refused too, so this holds under both. A runner whose token really is `LocalSystem` opens it and
/// fails here loudly rather than skipping, which is this repo's rule for a missing precondition.
#[skuld::test]
fn a_services_key_that_will_not_open_leaves_the_pass_unfinished() {
    // The probe really is refused. Stated separately so a runner whose token *does* open it says
    // so, rather than surfacing as a confusing failure about the scan.
    let refusal = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags("SECURITY", KEY_READ)
        .err()
        .unwrap_or_else(|| {
            panic!(r"this runner's token opens HKLM\SECURITY, so it is LocalSystem; this probe needs any other token")
        });
    assert_ne!(
        refusal.kind(),
        std::io::ErrorKind::NotFound,
        r"HKLM\SECURITY exists on every Windows host: {refusal}"
    );

    let scan = scan_names_under("SECURITY");

    assert!(scan.names.is_empty(), "{:?}", scan.names);
    let detail = scan
        .incomplete
        .expect("a key that exists and would not open established no absence");
    assert!(
        detail.contains("SECURITY"),
        "the detail must name what was being scanned: {detail}"
    );
}

#[skuld::test]
fn the_event_message_file_is_eventcreate_under_system_root() {
    assert_eq!(
        event_message_file(Ok(r"C:\Windows".to_string())).expect("a set SystemRoot"),
        r"C:\Windows\System32\EventCreate.exe"
    );
}

/// A registration with no message file is the one Event Viewer cannot render, so a missing
/// `SystemRoot` fails the install instead of leaving it.
#[skuld::test]
fn an_event_message_file_needs_a_system_root() {
    for missing in [
        std::env::VarError::NotPresent,
        std::env::VarError::NotUnicode(std::ffi::OsString::new()),
    ] {
        let Err(Error::Other(message)) = event_message_file(Err(missing)) else {
            panic!("a SystemRoot that cannot be read must be an error");
        };
        assert!(message.contains("SystemRoot"), "{message}");
    }
}
