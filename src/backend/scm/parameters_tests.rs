use super::*;

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

#[skuld::test]
fn a_goetia_field_stored_as_goetia_stores_it_is_a_field_in_whatever_case() {
    for name in ["Marker", "marker", "SPEC", "sChEmA", "Version"] {
        let parameters = parameters_of(utf16(name), vtype::REG_SZ, &sz("text"));

        assert_eq!(
            parameters.fields,
            BTreeMap::from([(name.to_string(), "text".to_string())]),
            "{name}"
        );
        assert!(parameters.strays.is_empty(), "{name}");
    }
}

#[skuld::test]
fn an_empty_field_value_is_a_field() {
    let parameters = parameters_of(utf16("Version"), vtype::REG_SZ, &sz(""));

    assert_eq!(parameters.fields.get("Version").map(String::as_str), Some(""));
}

// Strays: the value is not what goetia writes -------------------------------------------------------------------------

/// A field of another type is not equal to what goetia wants, and not a field `extract` can take
/// for a marker: a foreign service's `Marker` of type `REG_DWORD` stays foreign.
#[skuld::test]
fn a_goetia_field_of_another_type_is_a_stray_and_no_field() {
    for vtype in [
        vtype::REG_EXPAND_SZ,
        vtype::REG_MULTI_SZ,
        vtype::REG_DWORD,
        vtype::REG_BINARY,
        vtype::REG_NONE,
    ] {
        let parameters = parameters_of(utf16("Marker"), vtype, &sz("text"));

        assert!(parameters.fields.is_empty(), "{vtype}: {parameters:?}");
        assert_eq!(parameters.strays, [value_stray(&utf16("Marker"), vtype)], "{vtype}");
    }
}

/// A string layout goetia never writes: it must not read as the text it resembles.
#[skuld::test]
fn a_goetia_field_that_is_not_plain_nul_terminated_text_is_a_stray() {
    let text = utf16("text");
    let layouts: [(&str, Vec<u8>); 6] = [
        ("no terminator", units_bytes(&text)),
        ("two terminators", units_bytes(&[text.clone(), vec![0, 0]].concat())),
        (
            "a NUL inside",
            units_bytes(&[text.clone(), vec![0], text.clone(), vec![0]].concat()),
        ),
        ("an odd trailing byte", [sz("text"), vec![0x41]].concat()),
        ("a lone surrogate", units_bytes(&[0xD800, 0])),
        ("no data", Vec::new()),
    ];
    for (what, data) in layouts {
        let parameters = parameters_of(utf16("Spec"), vtype::REG_SZ, &data);

        assert!(parameters.fields.is_empty(), "{what}: {parameters:?}");
        assert_eq!(
            parameters.strays,
            [value_stray(&utf16("Spec"), vtype::REG_SZ)],
            "{what}"
        );
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
fn a_stray_renders_its_name_and_type() {
    assert_eq!(
        value_stray(&utf16("Marker"), vtype::REG_EXPAND_SZ).to_string(),
        r#"value "Marker" of type REG_EXPAND_SZ"#
    );
    assert_eq!(value_stray(&utf16("x"), 0x20).to_string(), r#"value "x" of type 32"#);
    assert_eq!(Stray::Subkey { name: utf16("Sub") }.to_string(), r#"subkey "Sub""#);
}

#[skuld::test]
fn a_stray_name_that_is_not_valid_utf16_renders_without_losing_the_unit() {
    let shown = Stray::Subkey {
        name: vec![0x41, 0xD800, 0x3D],
    }
    .to_string();

    assert!(shown.contains("D800") && shown.contains('A'), "{shown}");
    assert!(!shown.contains('\u{FFFD}'), "{shown}");
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
