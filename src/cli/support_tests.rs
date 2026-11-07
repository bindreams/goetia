use super::{UndeterminedEntry, print_undetermined_warnings, print_unusable_warnings, print_warnings};
use crate::error::Reason;
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
fn a_warning_is_printed_with_control_and_direction_characters_escaped() {
    let warnings = [Warning {
        id: None,
        message: "manifest directory `-f a\nb\u{202e}`".to_string(),
    }];
    let mut out: Vec<u8> = Vec::new();
    print_warnings(&warnings, &mut out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        concat!(r"warning: manifest directory `-f a\u{a}b\u{202e}`", "\n")
    );
}

/// A service name read back from the manager, and the reason goetia gives for it, are text from
/// outside: the same sink rule as any other warning.
#[skuld::test]
fn an_unusable_or_undetermined_entry_is_printed_with_control_and_direction_characters_escaped() {
    let mut out: Vec<u8> = Vec::new();
    let unusable = std::collections::BTreeMap::from([("a\nb".to_string(), Reason::said("why\u{1b}[0m"))]);
    print_unusable_warnings(&unusable, &mut out);
    let undetermined = [
        UndeterminedEntry {
            name: Some("c\u{202e}d".to_string()),
            reason: Reason::said("no\u{2028}pe"),
        },
        UndeterminedEntry {
            name: None,
            reason: Reason::said("all\rgone"),
        },
    ];
    print_undetermined_warnings(&undetermined, &mut out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        concat!(
            r"warning: a\u{a}b: installed but unusable: why\u{1b}[0m",
            "\n",
            r"warning: c\u{202e}d: installation state could not be determined: no\u{2028}pe",
            "\n",
            r"warning: all\u{d}gone",
            "\n",
        )
    );
}

/// The index feeds `--json`, whose encoder escapes the text itself: it holds a reason as it is, and
/// the sinks that print it escape.
#[skuld::test]
fn the_index_holds_a_reason_as_it_is() {
    use crate::manager::Installed;

    let index = super::partition_installed(vec![
        Installed::OursUnusable {
            name: "a".to_string(),
            reason: Reason::said("why\u{1b}[0m"),
        },
        Installed::Undetermined {
            name: None,
            reason: Reason::said("all\rgone"),
        },
    ]);
    assert_eq!(index.unusable["a"].unescaped().to_string(), "why\u{1b}[0m");
    assert_eq!(index.undetermined[0].reason.unescaped().to_string(), "all\rgone");
}

/// A child's stderr looks the same in `list`, `status` and `show` as in the report: its line
/// breaks stay, and the lines after the first are indented.
#[skuld::test]
fn a_reason_wrapping_a_command_error_keeps_the_stderr_layout_of_the_report() {
    use crate::error::Error;
    use crate::error::error_tests::exit_status;

    let command = || Error::command("systemctl show x", exit_status(1), b"first\nerror: forged");
    let report = command().report().to_string();
    assert!(report.ends_with(": first\n  error: forged"), "{report:?}");

    let mut out: Vec<u8> = Vec::new();
    let unusable = std::collections::BTreeMap::from([("x".to_string(), Reason::failed(command()))]);
    print_unusable_warnings(&unusable, &mut out);
    let undetermined = [
        UndeterminedEntry {
            name: Some("y".to_string()),
            reason: Reason::failed(command()),
        },
        UndeterminedEntry {
            name: None,
            reason: Reason::failed(command()),
        },
    ];
    print_undetermined_warnings(&undetermined, &mut out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "warning: x: installed but unusable: {report}\nwarning: y: installation state could not be determined: {report}\nwarning: {report}\n"
        )
    );
}
