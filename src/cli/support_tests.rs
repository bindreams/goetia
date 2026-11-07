use super::{print_warnings, require_elevation};
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

#[skuld::test]
fn a_subcommand_that_needs_elevation_is_named_in_backticks() {
    let mut err: Vec<u8> = Vec::new();

    assert!(require_elevation("daemon install", &|| false, &mut err).is_err());

    assert_eq!(
        String::from_utf8(err).unwrap(),
        "error: `daemon install` requires elevation (root/Administrator): re-run as root or Administrator\n"
    );
}
