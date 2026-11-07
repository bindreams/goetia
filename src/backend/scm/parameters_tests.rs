use super::*;
use crate::blob;

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// The bytes goetia stores for a string value: UTF-16LE and one closing NUL.
fn sz(text: &str) -> Vec<u8> {
    units_bytes(&[utf16(text), vec![0]].concat())
}

fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

fn parameters_of(name: Vec<u16>, vtype: u32, data: &[u8]) -> Parameters {
    let mut parameters = Parameters::default();
    parameters.add_value(name, vtype, data);
    parameters
}

fn value_stray(name: &[u16], vtype: u32) -> Stray {
    Stray::Value {
        name: name.to_vec(),
        vtype,
    }
}

// Fields --------------------------------------------------------------------------------------------------------------

fn layout_stray(field: &'static str, vtype: u32) -> Stray {
    Stray::Layout { name: field, vtype }
}

#[skuld::test]
fn a_goetia_field_stored_as_goetia_stores_it_is_a_field_under_goetias_spelling() {
    for (name, field) in [
        ("Marker", "Marker"),
        ("marker", "Marker"),
        ("SPEC", "Spec"),
        ("sChEmA", "Schema"),
        ("Version", "Version"),
    ] {
        let parameters = parameters_of(utf16(name), vtype::REG_SZ, &sz("text"));

        assert_eq!(
            parameters.fields,
            BTreeMap::from([(field.to_string(), "text".to_string())]),
            "{name}"
        );
        assert!(parameters.strays.is_empty(), "{name}");
    }
}

#[skuld::test]
fn an_empty_field_value_is_a_field() {
    let parameters = parameters_of(utf16("Version"), vtype::REG_SZ, &sz(""));

    assert_eq!(parameters.fields.get("Version").map(String::as_str), Some(""));
    assert!(parameters.strays.is_empty());
}

// A field a tool rewrote is still ours, and drift ---------------------------------------------------------------------

/// A tool that rewrites a value may store it as another string type or with other terminators. The
/// text is still goetia's, so ownership and the spec read; the layout is drift, which `install
/// --force` rewrites as the exact `REG_SZ`.
#[skuld::test]
fn a_goetia_field_in_another_string_layout_is_a_field_and_a_layout_stray() {
    for (name, text) in [("Marker", blob::MARKER), ("sPeC", "text")] {
        let units = utf16(text);
        let spellings: [(&str, u32, Vec<u8>); 7] = [
            ("REG_EXPAND_SZ", vtype::REG_EXPAND_SZ, sz(text)),
            (
                "REG_MULTI_SZ",
                vtype::REG_MULTI_SZ,
                units_bytes(&[units.clone(), vec![0, 0]].concat()),
            ),
            ("REG_MULTI_SZ with one NUL", vtype::REG_MULTI_SZ, sz(text)),
            (
                "REG_SZ with two NULs",
                vtype::REG_SZ,
                units_bytes(&[units.clone(), vec![0, 0]].concat()),
            ),
            (
                "REG_SZ with three NULs",
                vtype::REG_SZ,
                units_bytes(&[units.clone(), vec![0, 0, 0]].concat()),
            ),
            ("REG_SZ with no NUL", vtype::REG_SZ, units_bytes(&units)),
            ("REG_EXPAND_SZ with no NUL", vtype::REG_EXPAND_SZ, units_bytes(&units)),
        ];
        for (what, vtype, data) in spellings {
            let parameters = parameters_of(utf16(name), vtype, &data);

            let field = field_named(name).unwrap();
            assert_eq!(
                parameters.fields,
                BTreeMap::from([(field.to_string(), text.to_string())]),
                "{what}, {name}"
            );
            assert_eq!(parameters.strays, [layout_stray(field, vtype)], "{what}, {name}");
        }
    }
}

/// A third-party service may hold its own `Marker`, in any string type. Only goetia's text makes
/// it goetia's field, so such a service stays foreign instead of reading as a broken goetia one.
#[skuld::test]
fn a_marker_in_another_string_layout_with_other_text_is_a_stray_and_no_field() {
    let units = utf16("theirs");
    let layouts: [(&str, u32, Vec<u8>); 4] = [
        (
            "REG_MULTI_SZ",
            vtype::REG_MULTI_SZ,
            units_bytes(&[units.clone(), vec![0, 0]].concat()),
        ),
        ("REG_EXPAND_SZ", vtype::REG_EXPAND_SZ, sz("theirs")),
        (
            "REG_SZ with two NULs",
            vtype::REG_SZ,
            units_bytes(&[units.clone(), vec![0, 0]].concat()),
        ),
        ("REG_SZ with no NUL", vtype::REG_SZ, units_bytes(&units)),
    ];
    for (what, vtype, data) in layouts {
        let parameters = parameters_of(utf16("Marker"), vtype, &data);

        assert!(parameters.fields.is_empty(), "{what}: {parameters:?}");
        assert_eq!(parameters.strays, [value_stray(&utf16("Marker"), vtype)], "{what}");
    }
}

#[skuld::test]
fn an_empty_field_with_no_data_is_a_field_and_a_layout_stray() {
    let parameters = parameters_of(utf16("Version"), vtype::REG_SZ, &[]);

    assert_eq!(parameters.fields.get("Version").map(String::as_str), Some(""));
    assert_eq!(parameters.strays, [layout_stray("Version", vtype::REG_SZ)]);
}

// Strays: the value cannot be what goetia wrote -----------------------------------------------------------------------

/// A field that is not a string has no text to read: a foreign service's `Marker` of type
/// `REG_DWORD` stays foreign.
#[skuld::test]
fn a_goetia_field_that_is_not_a_string_is_a_stray_and_no_field() {
    for vtype in [
        vtype::REG_DWORD,
        vtype::REG_BINARY,
        vtype::REG_NONE,
        vtype::REG_QWORD,
        0x20,
    ] {
        let parameters = parameters_of(utf16("Marker"), vtype, &sz("text"));

        assert!(parameters.fields.is_empty(), "{vtype}: {parameters:?}");
        assert_eq!(parameters.strays, [value_stray(&utf16("Marker"), vtype)], "{vtype}");
    }
}

/// Text that cannot be one string of UTF-16 must not read as the text it resembles.
#[skuld::test]
fn a_goetia_field_without_one_string_of_text_is_a_stray() {
    let text = utf16("text");
    let layouts: [(&str, Vec<u8>); 3] = [
        (
            "a NUL inside",
            units_bytes(&[text.clone(), vec![0], text.clone(), vec![0]].concat()),
        ),
        ("an odd trailing byte", [sz("text"), vec![0x41]].concat()),
        ("a lone surrogate", units_bytes(&[0xD800, 0])),
    ];
    for vtype in [vtype::REG_SZ, vtype::REG_EXPAND_SZ, vtype::REG_MULTI_SZ] {
        for (what, data) in &layouts {
            let parameters = parameters_of(utf16("Spec"), vtype, data);

            assert!(parameters.fields.is_empty(), "{what}: {parameters:?}");
            assert_eq!(parameters.strays, [value_stray(&utf16("Spec"), vtype)], "{what}");
        }
    }
}

#[skuld::test]
fn a_value_that_is_none_of_goetias_fields_is_a_stray_whatever_it_holds() {
    for name in ["Stray", "Markers", ""] {
        let parameters = parameters_of(utf16(name), vtype::REG_SZ, &sz("text"));

        assert!(parameters.fields.is_empty(), "{name:?}");
        assert_eq!(
            parameters.strays,
            [value_stray(&utf16(name), vtype::REG_SZ)],
            "{name:?}"
        );
    }
}

// Strays winreg cannot describe ---------------------------------------------------------------------------------------

/// `winreg` fails to enumerate a value of a type above `REG_QWORD`; here it is only a number.
#[skuld::test]
fn a_value_of_a_type_no_registry_api_knows_is_a_stray_with_its_number() {
    for vtype in [12, 0x20, 0xFFFF, u32::MAX] {
        let parameters = parameters_of(utf16("Custom"), vtype, b"bytes");

        assert_eq!(parameters.strays, [value_stray(&utf16("Custom"), vtype)], "{vtype}");
    }
}

/// `winreg` fails to enumerate a name that is not valid UTF-16; here the units are kept as they are.
#[skuld::test]
fn a_value_name_that_is_not_valid_utf16_is_a_stray_with_its_units() {
    let name = vec![0x41, 0xD800, 0x42];

    let parameters = parameters_of(name.clone(), vtype::REG_SZ, &sz("text"));

    assert!(parameters.fields.is_empty());
    assert_eq!(parameters.strays, [value_stray(&name, vtype::REG_SZ)]);
}

#[skuld::test]
fn a_subkey_is_a_stray_whatever_its_name_and_never_a_field() {
    let mut parameters = Parameters::default();
    parameters.add_subkey(utf16("Marker"));
    parameters.add_subkey(vec![0xDC00]);

    assert!(parameters.fields.is_empty());
    assert_eq!(
        parameters.strays,
        [
            Stray::Subkey { name: utf16("Marker") },
            Stray::Subkey { name: vec![0xDC00] }
        ]
    );
}

/// Strays are data beside the fields, not names inside their map: no stray can clash with a field
/// or with another stray.
#[skuld::test]
fn strays_of_one_name_and_a_field_of_that_name_coexist() {
    let mut parameters = Parameters::default();
    parameters.add_value(utf16("Marker"), vtype::REG_SZ, &sz("m"));
    parameters.add_value(utf16("Marker"), vtype::REG_DWORD, &[1, 0, 0, 0]);
    parameters.add_value(utf16("Marker"), vtype::REG_QWORD, &[1; 8]);
    parameters.add_subkey(utf16("Marker"));

    assert_eq!(parameters.fields.len(), 1);
    assert_eq!(parameters.strays.len(), 3);
}

// Names and rendering -------------------------------------------------------------------------------------------------

#[skuld::test]
fn field_named_utf16_folds_ascii_case_and_knows_no_other_name() {
    assert_eq!(field_named_utf16(&utf16("sPeC")), Some("Spec"));
    assert_eq!(field_named_utf16(&utf16("Markers")), None);
    assert_eq!(field_named_utf16(&[]), None);
    assert_eq!(field_named_utf16(&[0x4D, 0xD800]), None);
}

#[skuld::test]
fn a_stray_renders_its_name_as_it_is_stored_and_its_type() {
    assert_eq!(
        value_stray(&utf16("Marker"), vtype::REG_EXPAND_SZ).to_string(),
        r#"value "Marker" of type REG_EXPAND_SZ"#
    );
    assert_eq!(value_stray(&utf16("x"), 0x20).to_string(), r#"value "x" of type 32"#);
    assert_eq!(Stray::Subkey { name: utf16("Sub") }.to_string(), r#"subkey "Sub""#);
    assert_eq!(
        layout_stray("Spec", vtype::REG_MULTI_SZ).to_string(),
        r#"value "Spec" stored as REG_MULTI_SZ, not as REG_SZ with one closing NUL"#
    );
}

/// A name shows as it can be typed into regedit: a backslash is not doubled, but a quote is
/// escaped so that the name cannot close the quotes it is shown in.
#[skuld::test]
fn a_stray_name_shows_its_backslash_as_it_is_and_escapes_its_quote() {
    assert_eq!(
        value_stray(&utf16(r#"a\b"c"#), vtype::REG_SZ).to_string(),
        r#"value "a\b\"c" of type REG_SZ"#
    );
}

/// A name with a line break must not break the line a diff is made of.
#[skuld::test]
fn a_stray_name_with_a_control_character_is_escaped_on_one_line() {
    let shown = Stray::Subkey { name: utf16("a\nb") }.to_string();

    assert_eq!(shown, r#"subkey "a\nb""#);
}

/// The characters escaped, and how, do not depend on what else the name holds.
#[skuld::test]
fn a_name_is_escaped_the_same_whether_or_not_it_is_valid_utf16() {
    let escaped = ["a\nb", "a\tb", "a\u{7f}b", r#"a"b"#, r"a\b", "a=b"];
    let want = [r"a\nb", r"a\tb", r"a\u{7f}b", r#"a\"b"#, r"a\b", "a=b"];
    for (text, want) in escaped.iter().zip(want) {
        let mut broken = utf16(text);
        broken.push(0xD800);

        assert_eq!(shown_name(&utf16(text)), want, "{text:?}");
        assert_eq!(
            shown_name(&broken),
            format!(r"(not valid UTF-16: {want}\u{{D800}})"),
            "{text:?}"
        );
    }
}

#[skuld::test]
fn a_stray_name_that_is_not_valid_utf16_renders_without_losing_the_unit() {
    let shown = Stray::Subkey {
        name: vec![0x41, 0xD800, 0x3D],
    }
    .to_string();

    assert_eq!(shown, r#"subkey "(not valid UTF-16: A\u{D800}=)""#);
}

#[skuld::test]
fn text_from_utf16_is_the_text_when_valid() {
    assert_eq!(text_from_utf16(&utf16("A=\u{1F600}")), "A=\u{1F600}");
}

#[skuld::test]
fn the_registrys_type_names_are_used_and_the_rest_are_numbers() {
    assert_eq!(type_name(vtype::REG_SZ), "REG_SZ");
    assert_eq!(type_name(vtype::REG_QWORD), "REG_QWORD");
    assert_eq!(type_name(12), "12");
}
