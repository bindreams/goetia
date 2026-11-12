use super::*;

use windows_sys::Win32::Foundation::ERROR_KEY_DELETED;
use winreg::RegValue;
use winreg::enums::RegType;
use winreg::types::ToRegValue as _;

use crate::testing::elevation::{ELEVATED, elevated};
use crate::testing::raw_registry::{create_link, create_raw_key};
use crate::testing::scratch::ScratchKey;

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

/// A read opens `Parameters` in one open, relative to `Services`, and asks nothing of the service
/// key it passes through, which a service that is not goetia's may deny to the reader.
#[skuld::test]
fn parameters_are_opened_in_one_step_below_services_for_reading_only() {
    let (name, access) = parameters_read_request("goetia-test");

    assert_eq!(name, utf16(r"goetia-test\Parameters"));
    assert_eq!(access, KEY_READ);
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[skuld::test]
fn a_stray_is_a_present_name_that_is_none_of_the_wanted_fields_in_any_case() {
    let wanted = BTreeMap::from([
        ("Marker".to_string(), "m".to_string()),
        ("Spec".to_string(), "s".to_string()),
    ]);
    // `Schema` is one of Goetia's fields but not wanted here, so it is as stray as `Stray`; a name
    // that is not valid UTF-16 is none of the fields.
    let invalid = vec![0x4D, 0xD800];
    let present: Vec<RawEntry> = ["marker", "SPEC", "Stray", "Marker2", "schema"]
        .map(entry)
        .into_iter()
        .chain([RawEntry::new(invalid.clone(), 1)])
        .collect();
    let found: Vec<&Vec<u16>> = strays(&present, &wanted).map(|entry| &entry.name).collect();
    assert_eq!(found, [&utf16("Stray"), &utf16("Marker2"), &utf16("schema"), &invalid]);
}

fn entry(name: &str) -> RawEntry {
    RawEntry::new(utf16(name), 1)
}

/// The decode happens once, when the entry is made, and says which field the name spells.
#[skuld::test]
fn an_entry_knows_which_field_its_name_spells_in_any_case() {
    assert_eq!(entry("sPeC").field, Some("Spec"));
    assert_eq!(entry("Spec\0").field, None);
    assert_eq!(RawEntry::new(vec![0x4D, 0xD800], 1).field, None);
}

#[skuld::test]
fn a_field_is_named_as_the_enumeration_stored_it() {
    let entries = [entry("Stray"), entry("sPeC"), entry("Spec\0")];

    assert_eq!(stored_name(&entries, "Spec"), utf16("sPeC"));
}

#[skuld::test]
fn a_field_the_enumeration_missed_is_named_as_goetia_spells_it() {
    assert_eq!(stored_name(&[entry("Stray")], "Spec"), utf16("Spec"));
}

const NOT_FOUND: i32 = 2;
const KEY_DELETED: i32 = ERROR_KEY_DELETED as i32;

fn failed_with(code: i32) -> Failure {
    failure("open", "Services", std::io::Error::from_raw_os_error(code))
}

fn os_code(error: &Error) -> Option<i32> {
    error.raw_os_error()
}

#[skuld::test]
fn a_read_that_finds_no_optional_key_under_a_service_that_is_gone_is_service_gone() {
    let read = read_settled(
        || Ok(false),
        |f| registry_error(&f.op, &f.path, f.source),
        || Err(Stop::at_open(failed_with(NOT_FOUND), Some(7))),
    );

    assert!(matches!(read, Ok(RegistryReadOutcome::ServiceGone)));
}

#[skuld::test]
fn a_read_that_finds_no_optional_key_under_a_service_that_stands_is_the_empty_value() {
    let read = read_settled(
        || Ok(true),
        |f| registry_error(&f.op, &f.path, f.source),
        || Err(Stop::at_open(failed_with(NOT_FOUND), Some(7))),
    );

    assert!(matches!(read, Ok(RegistryReadOutcome::Read(7))));
}

#[skuld::test]
fn a_key_that_vanishes_under_a_service_that_stands_fails_with_the_cause_it_met() {
    let read = read_settled(
        || Ok(true),
        |f| registry_error(&f.op, &f.path, f.source),
        || Err::<(), _>(Stop::mid_read(failed_with(KEY_DELETED))),
    );

    assert_eq!(read.err().and_then(|e| os_code(&e)), Some(KEY_DELETED));
}

#[skuld::test]
fn a_pass_that_completes_is_its_value_without_asking_whether_the_service_stands() {
    let read = read_settled(
        || panic!("the service's existence is asked only after a pass stops"),
        |f| registry_error(&f.op, &f.path, f.source),
        || Ok(7),
    );

    assert!(matches!(read, Ok(RegistryReadOutcome::Read(7))));
}

/// What a scripted call does: sets the size and type it reports, and returns its status.
fn scripted<const N: usize>(steps: [(u32, u32, u32); N]) -> impl FnMut(*mut u8, &mut u32, &mut u32) -> u32 {
    let mut steps = steps.into_iter();
    move |_data, len, vtype| {
        let (status, size, reported_type) = steps.next().expect("the call ran more often than scripted");
        *len = size;
        *vtype = reported_type;
        status
    }
}

#[skuld::test]
fn a_read_that_reports_more_data_grows_the_buffer_to_the_size_reported_and_asks_again() {
    let mut buffer = vec![0u8; 4];
    let mut call = scripted([(ERROR_MORE_DATA, 600, 0), (ERROR_SUCCESS, 10, 1)]);

    let read = read_growing(&mut buffer, &mut call);

    assert_eq!(read, Ok((1, 10)));
    assert_eq!(buffer.len(), 600);
}

#[skuld::test]
fn a_read_that_reports_more_data_without_a_larger_size_returns_that_status() {
    let mut buffer = vec![0u8; 4];
    let mut call = scripted([(ERROR_MORE_DATA, 4, 0)]);

    assert_eq!(read_growing(&mut buffer, &mut call), Err(ERROR_MORE_DATA));
    assert_eq!(buffer.len(), 4);
}

#[skuld::test]
fn a_read_that_fails_otherwise_returns_that_status() {
    let mut buffer = vec![0u8; 4];
    let mut call = scripted([(5, 0, 0)]);

    assert_eq!(read_growing(&mut buffer, &mut call), Err(5));
}

#[skuld::test]
fn an_absent_environment_value_is_no_lines() {
    assert_eq!(environment_lines(None), Vec::<String>::new());
}

/// A `REG_MULTI_SZ` made of exactly `units`, with no terminator added.
fn multi_sz(units: &[u16]) -> (u32, Vec<u8>) {
    (REG_MULTI_SZ, units.iter().flat_map(|u| u.to_le_bytes()).collect())
}

/// The bytes `write_environment` writes for `lines`, through the same `winreg` encoder.
fn goetia_multi_sz(lines: &[&str]) -> (u32, Vec<u8>) {
    let owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let written = owned.to_reg_value();
    (written.vtype as u32, written.bytes.into_owned())
}

fn lines_of((vtype, bytes): &(u32, Vec<u8>)) -> Vec<String> {
    environment_lines(Some((*vtype, bytes)))
}

/// Only a value byte-identical to what goetia writes may read as the lines it was written from, so
/// that `decide` calls nothing else `UpToDate`.
fn assert_not_read_as(what: &str, value: (u32, Vec<u8>), desired: &[&str]) {
    let lines = lines_of(&value);
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
    assert_eq!(lines_of(&goetia_multi_sz(&lines)), lines);
}

/// `winreg` reads an empty `REG_MULTI_SZ` as one empty string, and `write_environment` writes an empty
/// list as no value at all: a present empty value is not what goetia writes.
#[skuld::test]
fn an_empty_multi_string_environment_value_is_not_the_absent_value() {
    assert_not_read_as("an empty value", goetia_multi_sz(&[]), &[]);
}

/// A line that is not valid UTF-16 reads lossily, so the lone surrogate reads as the `A=\u{FFFD}`
/// goetia could have written, and the layout line is what tells them apart.
#[skuld::test]
fn a_line_that_is_not_valid_utf16_reads_lossily_and_adds_a_layout_line() {
    for surrogate in [0xD800u16, 0xDBFF, 0xDC00, 0xDFFF] {
        let mut units = utf16("A=");
        units.push(surrogate);
        units.extend([0, 0]);

        assert_eq!(
            lines_of(&multi_sz(&units)),
            [
                "A=\u{FFFD}",
                "(REG_MULTI_SZ not laid out as goetia writes it: a line that is not valid UTF-16)"
            ],
            "{surrogate:04X}"
        );
    }
}

#[skuld::test]
fn a_surrogate_pair_is_one_character() {
    let mut units = utf16("A=\u{1F600}");
    units.extend([0, 0]);
    assert_eq!(lines_of(&multi_sz(&units)), ["A=\u{1F600}"]);
}

/// Dropping a trailing odd byte reads `A=1` plus half a unit as `A=1`.
#[skuld::test]
fn an_odd_trailing_byte_is_not_dropped() {
    let mut value = goetia_multi_sz(&["A=1"]);
    value.1.push(0x41);
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
    let lines = lines_of(&(written.vtype as u32, written.bytes.into_owned()));

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].contains('='), "{lines:?}");
    assert!(lines[0].contains("REG_SZ"), "{lines:?}");
}

/// A type `winreg` cannot represent: `get_raw_value` fails on it, so the read that finds it must not.
#[skuld::test]
fn an_environment_value_of_an_unknown_type_is_one_marker_line_naming_its_number() {
    for vtype in [12u32, 0xFFFF] {
        let lines = lines_of(&(vtype, b"A=1".to_vec()));

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
        let Error::Undetermined { id, reason, recovery } = registry_undetermined("x", "open", &path, source) else {
            panic!("{rendered}: a read that did not complete establishes nothing about the id");
        };

        assert_eq!(id, "x");
        let reason = reason.report().to_string();
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

    let incomplete = scan_services(|name, _| names.push(name));

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

    let incomplete = scan_names_under(&format!(r"{SERVICES_KEY}\goetia-no-such-key-b6f0a1c2"), |name, _| {
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

    let incomplete = scan_names_under("SECURITY", |name, _| names.push(name));

    assert!(names.is_empty(), "{names:?}");
    match incomplete {
        Some(Installed::Undetermined { name: None, reason }) => {
            let reason = reason.to_string();
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
        let Err(e @ Error::Os { .. }) = event_message_file(Err(missing)) else {
            panic!("a SystemRoot that cannot be read must be an error");
        };
        let message = e.to_string();
        assert!(message.contains("SystemRoot"), "{message}");
    }
}

/// Writes keys under `HKLM\SOFTWARE`, a scratch key standing for `Services`. The image is read
/// below the root the scan hands over: a service marked for deletion, one with no key, and one whose
/// `ImagePath` is not a string start nothing.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_registered_image_is_read_below_the_root_it_is_given() {
    let scratch = ScratchKey::new(&format!("image-{}-root", std::process::id()));
    let root = scratch.create();
    let set_image = |name: &str, image: RegValue| {
        let (key, _) = root.create_subkey(name).expect("create the service key");
        key.set_raw_value(IMAGE_PATH_VALUE, &image).expect("write ImagePath");
        key
    };
    set_image("runs", r"C:\a\host.exe --x".to_reg_value());
    set_image("marked", r"C:\a\host.exe".to_reg_value())
        .set_value(DELETE_FLAG_VALUE, &1u32)
        .expect("write DeleteFlag");
    set_image("not-a-string", 7u32.to_reg_value());
    root.create_subkey("no-image").expect("create the service key");
    let services = ServicesRoot::new(&root);

    let read = |name: &str| match read_registered_image(&services, name).expect(name) {
        RegistryReadOutcome::Read(image) => Some(image),
        RegistryReadOutcome::ServiceGone => None,
    };

    assert_eq!(read("runs"), Some(Some(r"C:\a\host.exe --x".to_string())));
    assert_eq!(read("marked"), Some(None));
    assert_eq!(read("not-a-string"), Some(None));
    assert_eq!(read("no-image"), Some(None));
    assert_eq!(read("not-listed-any-more"), None);
}

/// Writes keys under `HKLM\SOFTWARE`. SCM follows a registry link at `Services\<name>`, and so the
/// sweep reads the target's `ImagePath`: left out of what is referenced, the host the service starts
/// would be deleted.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_service_key_that_is_a_link_is_read_through_to_its_target() {
    let unique = std::process::id();
    // The target first, so that it is removed last.
    let target = ScratchKey::new(&format!("image-{unique}-link-target"));
    let root_scratch = ScratchKey::new(&format!("image-{unique}-link-root"));
    target
        .create()
        .set_raw_value(IMAGE_PATH_VALUE, &r"C:\linked\host.exe --x".to_reg_value())
        .expect("write ImagePath");
    let root = root_scratch.create();
    let _link = create_link(&root, "linked", &target.nt_path());
    let services = ServicesRoot::new(&root);

    let read = read_registered_image(&services, "linked").expect("read the linked service");

    assert!(
        matches!(&read, RegistryReadOutcome::Read(Some(image)) if image == r"C:\linked\host.exe --x"),
        "the target's ImagePath was not read"
    );
}

/// Writes keys under `HKLM\SOFTWARE`. A `Services` key that is marked deleted while the scan holds
/// it (another handle deleted it) fails the open of a service below it with KEY_DELETED, which the
/// read settles the service as gone, not as a failure, as it does when its own key is gone.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_root_marked_deleted_under_the_scan_reads_the_service_as_gone() {
    let scratch = ScratchKey::new(&format!("image-{}-deleted-root", std::process::id()));
    let root = scratch.create();
    let services = ServicesRoot::new(&root);
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .delete_subkey(scratch.path())
        .expect("delete the root through another handle");

    let read = read_registered_image(
        &services,
        &format!("goetia-test-no-such-service-{}", std::process::id()),
    );

    assert!(
        matches!(
            read.expect("a service whose key is gone is not a failure"),
            RegistryReadOutcome::ServiceGone
        ),
        "a service whose key is gone reads as gone"
    );
}

/// Writes keys under `HKLM\SOFTWARE`. A service key that is a link to a missing key fails the image
/// read, as it fails the other reads: the registry cannot say the service is gone where SCM may have
/// one, and the sweep then asks SCM (`image_of`).
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_service_key_that_is_a_link_to_a_missing_key_fails_an_image_read() {
    let unique = std::process::id();
    let root_scratch = ScratchKey::new(&format!("image-{unique}-dangling-root"));
    let target = ScratchKey::new(&format!("image-{unique}-dangling-target"));
    let root = root_scratch.create();
    let _link = create_link(&root, "dangling", &target.nt_path());
    let services = ServicesRoot::new(&root);

    let read = read_registered_image(&services, "dangling");

    let error = read
        .err()
        .expect("a service key whose target is missing is not a service that is gone");
    assert!(matches!(error, Error::Os { .. }), "{error:?}");
    assert_eq!(os_code(&error), Some(NOT_FOUND), "{error:?}");
}

/// Writes keys under `HKLM\SOFTWARE`. A service key that is a link to a missing key reads as an
/// error: neither Foreign nor gone.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_service_key_that_is_a_link_to_a_missing_key_fails_a_parameters_read() {
    let unique = std::process::id();
    let root_scratch = ScratchKey::new(&format!("parameters-{unique}-dangling-root"));
    let target = ScratchKey::new(&format!("parameters-{unique}-dangling-target"));
    let root = root_scratch.create();
    let _link = create_link(&root, "dangling", &target.nt_path());
    let services = ServicesRoot::new(&root);

    let read = read_parameters_in(&services, "dangling");

    assert!(
        matches!(read, Err(Error::Undetermined { .. })),
        "a service key whose target is missing is neither foreign nor gone"
    );
}

/// Writes keys under `HKLM\SOFTWARE`. `read_environment` on a dangling-link service key is an error.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_service_key_that_is_a_link_to_a_missing_key_fails_an_environment_read() {
    let unique = std::process::id();
    let root_scratch = ScratchKey::new(&format!("environment-{unique}-dangling-root"));
    let target = ScratchKey::new(&format!("environment-{unique}-dangling-target"));
    let root = root_scratch.create();
    let _link = create_link(&root, "dangling", &target.nt_path());
    let services = ServicesRoot::new(&root);

    let read = read_environment_in(&services, "dangling");

    let error = read
        .err()
        .expect("a service key whose target is missing is not a service that is gone");
    assert!(matches!(error, Error::Os { .. }), "{error:?}");
    assert_eq!(os_code(&error), Some(NOT_FOUND), "{error:?}");
}

/// Writes keys under `HKLM\SOFTWARE`. A service key that is there with no `Parameters` stands without
/// it, which reads as empty `Parameters`: not goetia's.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_service_key_with_no_parameters_reads_as_empty_parameters() {
    let scratch = ScratchKey::new(&format!("bare-{}-root", std::process::id()));
    let root = scratch.create();
    root.create_subkey("bare").expect("create the service key");
    let services = ServicesRoot::new(&root);

    let read = read_parameters_in(&services, "bare").expect("read Parameters");

    assert!(matches!(read, RegistryReadOutcome::Read(_)));
}

/// Writes keys under `HKLM\SOFTWARE`. A service whose key is not there is gone.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_service_key_that_is_not_there_is_gone_for_a_parameters_read() {
    let scratch = ScratchKey::new(&format!("absent-{}-root", std::process::id()));
    let root = scratch.create();
    let services = ServicesRoot::new(&root);

    let read = read_parameters_in(&services, "absent").expect("read Parameters");

    assert!(matches!(read, RegistryReadOutcome::ServiceGone));
}

/// Writes keys under `HKLM\SOFTWARE`. A read classifies a `Parameters` that is a link as not
/// goetia's, so a write meets one only when it appeared after the read: this drives
/// `create_parameters_key` directly, with a scratch key for the service key. Writing through the
/// link is denied (a link key takes no value but its `SymbolicLinkValue`), and would change a key
/// goetia does not own: the link is deleted, a real key made, and the target left as it was.
#[skuld::test(requires = [elevated], labels = [ELEVATED])]
fn a_parameters_that_is_a_link_is_replaced_by_a_real_key_and_its_target_is_left_alone() {
    let unique = std::process::id();
    // The target first, so that it is removed last.
    let target = ScratchKey::new(&format!("parameters-link-target-{unique}"));
    let service = ScratchKey::new(&format!("parameters-link-service-{unique}"));
    let target_key = target.create();
    target_key.set_value("Keep", &"x").expect("write Keep");
    target_key.set_value("Marker", &"not goetia's").expect("write Marker");
    let service_key = service.create();
    let _link = create_link(&service_key, "Parameters", &target.nt_path());

    let key = create_parameters_key(&service_key).expect("replace the link");
    key.set_value("Marker", &"goetia's").expect("write through the new key");
    drop(key);

    // Opened as itself, `Parameters` is a real key: it holds no `SymbolicLinkValue`, and the value
    // written is in it. Opened as every other reader does, it is the same key, not the target.
    let as_itself = create_raw_key(&service_key, &utf16("Parameters"), 0);
    let link_value = as_itself.get_raw_value("SymbolicLinkValue");
    assert!(
        link_value
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "Parameters is still a link: {link_value:?}"
    );
    assert_eq!(
        as_itself.get_value::<String, _>("Marker").expect("read Marker"),
        "goetia's"
    );
    let followed = service_key.open_subkey("Parameters").expect("open Parameters");
    assert_eq!(
        followed.get_value::<String, _>("Marker").expect("read Marker"),
        "goetia's"
    );
    assert!(
        followed.get_value::<String, _>("Keep").is_err(),
        "Parameters shows the target's `Keep`"
    );

    // The target holds exactly what it did.
    let mut names: Vec<String> = target_key.enum_values().map(|v| v.expect("enumerate").0).collect();
    names.sort();
    assert_eq!(names, ["Keep", "Marker"]);
    assert_eq!(target_key.get_value::<String, _>("Keep").expect("read Keep"), "x");
    assert_eq!(
        target_key.get_value::<String, _>("Marker").expect("read Marker"),
        "not goetia's"
    );
}

fn raw_string(vtype: &RegType, text: &str) -> RegValue<'static> {
    RegValue {
        bytes: text
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>()
            .into(),
        vtype: vtype.clone(),
    }
}

#[skuld::test]
fn an_image_is_read_up_to_its_first_nul() {
    for vtype in &[RegType::REG_SZ, RegType::REG_EXPAND_SZ, RegType::REG_MULTI_SZ] {
        assert_eq!(
            image_text(&raw_string(vtype, "C:\\a\\host.exe --x\0tail\0\0")).as_deref(),
            Some(r"C:\a\host.exe --x"),
            "{vtype:?}"
        );
        assert_eq!(
            image_text(&raw_string(vtype, "C:\\a\\host.exe\0")).as_deref(),
            Some(r"C:\a\host.exe"),
            "{vtype:?}"
        );
        assert_eq!(image_text(&raw_string(vtype, "")).as_deref(), Some(""), "{vtype:?}");
    }
    assert_eq!(image_text(&7u32.to_reg_value()), None);
}

#[skuld::test]
fn an_image_with_a_trailing_odd_byte_drops_that_byte() {
    let mut raw = raw_string(&RegType::REG_SZ, "ab");
    raw.bytes.to_mut().push(b'c');

    assert_eq!(image_text(&raw).as_deref(), Some("ab"));
}

#[skuld::test]
fn an_image_with_a_lone_surrogate_reads_it_as_the_replacement_character() {
    let raw = RegValue {
        bytes: [u16::from(b'a'), 0xD800, u16::from(b'b')]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>()
            .into(),
        vtype: RegType::REG_SZ,
    };

    assert_eq!(image_text(&raw).as_deref(), Some("a\u{FFFD}b"));
}

#[skuld::test]
fn text_with_a_nul_is_refused_as_invalid_input_and_text_without_one_is_not() {
    assert!(refuse_nul(["a", "b=c", ""]).is_ok());
    for texts in [["a\0b"], ["\0"], ["a\0"]] {
        assert_eq!(refuse_nul(texts).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
    assert_eq!(
        refuse_nul(["fine", "NAME=va\0lue"]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}
