use super::print_warnings;
use crate::spec::{Id, Warning};

fn rendered(id: Option<Id>) -> String {
    let warnings = [Warning {
        id,
        message: "a message".to_string(),
    }];
    let mut out: Vec<u8> = Vec::new();
    print_warnings(&warnings, &mut out);
    String::from_utf8(out).expect("print_warnings writes utf-8")
}

#[skuld::test]
fn a_daemons_warning_is_printed_with_its_id() {
    let id = Id::try_from("frpc").expect("`frpc` is a valid id");
    assert_eq!(rendered(Some(id)), "warning: frpc: a message\n");
}

#[skuld::test]
fn a_manifest_level_warning_is_printed_with_no_id() {
    // `Warning::id` became `Option<Id>` for the drive-relative `-f`
    // advisory, which belongs to no daemon. Byte-exact, because the whole
    // of this arm is the absence of a prefix.
    assert_eq!(rendered(None), "warning: a message\n");
}
