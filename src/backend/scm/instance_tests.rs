use super::*;

fn id(text: &str) -> Id {
    Id::try_from(text).unwrap()
}

#[skuld::test]
fn task_path_is_the_folder_and_the_id() {
    assert_eq!(TaskPath::of(&id("frpc")).as_str(), r"\Goetia\frpc");
    assert_eq!(TaskPath::of(&id("Back-up_2.x")).as_str(), r"\Goetia\Back-up_2.x");
}

#[skuld::test]
fn task_path_id_round_trips() {
    for text in ["frpc", "Foo", "a", "a.b_c-d", &"x".repeat(80)] {
        let path = TaskPath::of(&id(text));
        assert_eq!(path.name(), text);
        assert_eq!(TaskPath::id(path.name()), Some(id(text)), "{text}");
    }
}

#[skuld::test]
fn a_name_in_the_folder_is_an_id_only_if_it_is_a_valid_id() {
    for name in ["", "a b", r"a\b", "a/b", "caf\u{e9}", &"x".repeat(81), "a\0b"] {
        assert_eq!(TaskPath::id(name), None, "{name:?}");
    }
}

#[skuld::test]
fn paths_of_ids_that_differ_only_in_case_are_one_path() {
    // Task Scheduler matches a path case-insensitively, as the SCM matches a service name; the id
    // compare is exact, so the path does the folding.
    assert_ne!(id("Foo"), id("foo"));
    assert_eq!(TaskPath::of(&id("Foo")), TaskPath::of(&id("foo")));
    assert_ne!(TaskPath::of(&id("Foo")), TaskPath::of(&id("fox")));
    // The path keeps the case it was given.
    assert_eq!(TaskPath::of(&id("Foo")).as_str(), r"\Goetia\Foo");
}

const INSTANCE: InstanceId = InstanceId([
    0x00, 0x1a, 0xff, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0,
]);

#[skuld::test]
fn a_runs_log_marker_is_goetia_run_and_the_instances_digits() {
    assert_eq!(INSTANCE.marker(), "goetia-run 001aff102030405060708090a0b0c0d0");
}

#[skuld::test]
fn instance_id_hex_is_32_lowercase_digits() {
    assert_eq!(INSTANCE.hex(), "001aff102030405060708090a0b0c0d0");
    assert_eq!(InstanceId([0; 16]).hex(), "0".repeat(32));
}

#[skuld::test]
fn instance_id_parses_the_braced_form_com_returns() {
    let braced = "{001AFF10-2030-4050-6070-8090A0B0C0D0}";

    assert_eq!(InstanceId::parse(braced), Some(INSTANCE));
}

#[skuld::test]
fn instance_id_parses_the_plain_forms_and_ignores_case() {
    for text in [
        "001aff102030405060708090a0b0c0d0",
        "001AFF102030405060708090A0B0C0D0",
        "001aff10-2030-4050-6070-8090a0b0c0d0",
    ] {
        assert_eq!(InstanceId::parse(text), Some(INSTANCE), "{text}");
    }
}

#[skuld::test]
fn instance_id_refuses_what_is_not_a_guid() {
    let fullwidth = "\u{ff11}".repeat(32);
    for text in [
        "",
        "{}",
        "001aff102030405060708090a0b0c0d",          // 31 digits
        "001aff102030405060708090a0b0c0d00",        // 33 digits
        "{001aff10-2030-4050-6070-8090a0b0c0d0",    // unmatched brace
        "001aff10-2030-4050-6070-8090a0b0c0d0}",    // unmatched brace
        "{{001aff10-2030-4050-6070-8090a0b0c0d0}}", // two braces
        "001aff1020-30-4050-6070-8090a0b0c0d0",     // dashes in the wrong places
        "{001aff10-2030-4050-6070-8090a0b0c0dg}",   // not hex
        " 001aff102030405060708090a0b0c0d0",        // space
        &fullwidth,                                 // not ASCII digits
    ] {
        assert_eq!(InstanceId::parse(text), None, "{text:?}");
    }
}

#[skuld::test]
fn instance_ids_order_by_their_bytes() {
    assert!(InstanceId([0; 16]) < INSTANCE);
}

#[skuld::test]
fn instance_id_hex_parses_back() {
    assert_eq!(InstanceId::parse(&INSTANCE.hex()), Some(INSTANCE));
}
