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

/// A `REG_MULTI_SZ` made of exactly `units`, with no terminator added.
fn multi_sz(units: &[u16]) -> RawValue {
    RawValue {
        vtype: REG_MULTI_SZ,
        bytes: units.iter().flat_map(|u| u.to_le_bytes()).collect(),
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The bytes `write_environment` writes for `lines`, through the same `winreg` encoder.
fn goetia_multi_sz(lines: &[&str]) -> RawValue {
    let owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let written = owned.to_reg_value();
    RawValue {
        vtype: written.vtype as u32,
        bytes: written.bytes.into_owned(),
    }
}

/// Only a value byte-identical to what goetia writes may read as the lines it was written from, so
/// that `decide` calls nothing else `UpToDate`.
fn assert_not_read_as(what: &str, value: RawValue, desired: &[&str]) {
    let lines = environment_lines(Some(value));
    assert_ne!(
        lines, desired,
        "{what}: {lines:?} must differ from what goetia would write"
    );
    assert!(
        lines.iter().any(|l| !l.contains('=')),
        "{what}: the difference must be a line no desired text can equal: {lines:?}"
    );
}

#[skuld::test]
fn the_bytes_goetia_writes_read_back_as_the_lines() {
    let lines = ["A=1", "B=x y", "C=\u{FFFD}"];
    assert_eq!(environment_lines(Some(goetia_multi_sz(&lines))), lines);
}

/// `winreg` reads an empty `REG_MULTI_SZ` as one empty string, and `write_environment` writes an empty
/// list as no value at all: a present empty value is not what goetia writes.
#[skuld::test]
fn an_empty_multi_string_environment_value_is_not_the_absent_value() {
    assert_not_read_as("an empty value", goetia_multi_sz(&[]), &[]);
}

/// Lossy decoding turns a lone surrogate into U+FFFD, so a hand-written `D800` would read as the
/// `A=\u{FFFD}` goetia could have written.
#[skuld::test]
fn a_lone_surrogate_is_not_the_replacement_character() {
    for surrogate in [0xD800u16, 0xDBFF, 0xDC00, 0xDFFF] {
        let mut units = utf16("A=");
        units.push(surrogate);
        units.extend([0, 0]);
        assert_not_read_as(&format!("{surrogate:04X}"), multi_sz(&units), &["A=\u{FFFD}"]);
    }
}

#[skuld::test]
fn a_surrogate_pair_is_one_character() {
    let mut units = utf16("A=\u{1F600}");
    units.extend([0, 0]);
    assert_eq!(environment_lines(Some(multi_sz(&units))), ["A=\u{1F600}"]);
}

/// Dropping a trailing odd byte reads `A=1` plus half a unit as `A=1`.
#[skuld::test]
fn an_odd_trailing_byte_is_not_dropped() {
    let mut value = goetia_multi_sz(&["A=1"]);
    value.bytes.push(0x41);
    assert_not_read_as("an odd byte", value, &["A=1"]);
}

/// Stripping every trailing NUL reads each of these as `A=1`.
#[skuld::test]
fn a_nul_layout_other_than_goetias_is_not_read_as_the_lines() {
    let a1 = utf16("A=1");
    let layouts: [(&str, Vec<u16>); 6] = [
        ("no terminators", a1.clone()),
        ("one terminator", [a1.clone(), vec![0]].concat()),
        ("three terminators", [a1.clone(), vec![0, 0, 0]].concat()),
        ("a leading NUL", [vec![0], a1.clone(), vec![0, 0]].concat()),
        (
            "an empty string between",
            [a1.clone(), vec![0, 0], a1.clone(), vec![0, 0]].concat(),
        ),
        ("no strings", vec![0, 0]),
    ];
    for (what, units) in layouts {
        assert_not_read_as(what, multi_sz(&units), &["A=1"]);
    }
}

/// A rendered line is always `NAME=value`, so a line without `=` can never equal desired text: a
/// value of the wrong type is a `Conflict` that names its type, and `--force` rewrites it.
#[skuld::test]
fn an_environment_value_of_another_type_is_one_marker_line_naming_its_type() {
    let written = "A=1".to_reg_value();
    let value = RawValue {
        vtype: written.vtype as u32,
        bytes: written.bytes.into_owned(),
    };
    let lines = environment_lines(Some(value));

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].contains('='), "{lines:?}");
    assert!(lines[0].contains("REG_SZ"), "{lines:?}");
}

/// A type `winreg` cannot represent: `get_raw_value` fails on it, so the read that finds it must not.
#[skuld::test]
fn an_environment_value_of_an_unknown_type_is_one_marker_line_naming_its_number() {
    for vtype in [12u32, 0xFFFF] {
        let value = RawValue {
            vtype,
            bytes: b"A=1".to_vec(),
        };
        let lines = environment_lines(Some(value));

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!lines[0].contains('='), "{lines:?}");
        assert!(lines[0].contains(&vtype.to_string()), "{lines:?}");
    }
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

// The open, through the real `RegKey` =================================================================================
//
// The mid-pass fault and the shape of a finished pass are `flow_tests.rs`'s, over plain iterators.
// These cover what only the real `RegOpenKeyEx` can: which `io::ErrorKind` each open outcome
// reaches `flow::scan` as.

/// The production call, against the production key. Every Windows host that boots has `Services`,
/// with services under it, readable at any privilege level — so a pass over it names some and
/// reports nothing outstanding. Without this, no test opens that key at all.
#[skuld::test]
fn the_services_key_enumerates_and_finishes() {
    let mut names = Vec::new();

    let incomplete = scan_services(|name| names.push(name));

    assert!(!names.is_empty(), "every Windows host runs services: {names:?}");
    assert!(
        incomplete.is_none(),
        "the `Services` key opens and enumerates for any caller: {incomplete:?}"
    );
}

/// A key that is not there is *absence, established* — an empty scan with nothing outstanding, so
/// `list` returns an empty document at exit `0` rather than one aggregate entry and a permanent
/// exit `4`. `ServiceManager::list`'s doc comment requires this of every backend's absent container.
#[skuld::test]
fn a_services_key_that_does_not_exist_is_an_empty_settled_scan() {
    let mut names = Vec::new();

    let incomplete = scan_names_under(&format!(r"{SERVICES_KEY}\goetia-no-such-key-b6f0a1c2"), |name| {
        names.push(name)
    });

    assert!(names.is_empty(), "{names:?}");
    assert!(
        incomplete.is_none(),
        "nothing is registered under a key that does not exist, and that is an answer: {incomplete:?}"
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
    let mut names = Vec::new();

    let incomplete = scan_names_under("SECURITY", |name| names.push(name));

    assert!(names.is_empty(), "{names:?}");
    match incomplete {
        Some(Installed::Undetermined { name: None, reason }) => {
            assert!(
                reason.contains("SECURITY"),
                "the entry must name what was being scanned: {reason}"
            );
        }
        other => panic!("a key that exists and would not open established no absence, got {other:?}"),
    }
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
