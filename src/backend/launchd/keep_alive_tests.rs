use super::*;
use crate::backend::Identity;
use crate::backend::launchd::generate::plists;
use crate::spec::test_support::program;
use crate::spec::{DaemonSpec, Restart, ServiceFields, ServiceSpec};
use crate::testing::{base_fields, task};

/// A job's plist whose dict holds `body`, with goetia's metadata comment above it.
fn plist_with(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- goetia:begin\nMarker: x\nSpec: a--b\ngoetia:end -->\n\
         <plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>x</string>\n{body}</dict>\n</plist>\n"
    )
}

fn rendered(spec: &DaemonSpec) -> String {
    plists(
        spec,
        &Identity {
            user: "root".to_string(),
        },
    )
    .unwrap()
    .job
}

fn service(restart: Restart) -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        restart,
        ..base_fields("job", vec![program("true")])
    })
    .map(DaemonSpec::from)
    .unwrap()
}

/// What each form the generator writes means: `restart: never` and a task write no key, `always` writes
/// `true`, and `on-failure` a dict.
#[skuld::test]
fn the_forms_the_generator_writes_are_decided() {
    let cases = [
        (
            "a service with restart: never",
            rendered(&service(Restart::Never)),
            false,
        ),
        (
            "a service with restart: always",
            rendered(&service(Restart::Always)),
            true,
        ),
        (
            "a service with restart: on-failure",
            rendered(&service(Restart::OnFailure)),
            true,
        ),
        (
            "a task",
            rendered(&task("job", vec![program("true")], Some("30 3 * * *"))),
            false,
        ),
        (
            "a task with no schedule",
            rendered(&task("job", vec![program("true")], None)),
            false,
        ),
    ];
    for (what, text, kept_alive) in cases {
        assert_eq!(keeps_alive(&text).unwrap(), kept_alive, "{what}:\n{text}");
    }
}

/// Every form a plist can give the key that is readable, beside the generator's.
#[skuld::test]
fn a_readable_key_is_decided_by_its_value() {
    let cases = [
        ("no key", "", false),
        ("false", "\t<key>KeepAlive</key>\n\t<false/>\n", false),
        ("true", "\t<key>KeepAlive</key>\n\t<true/>\n", true),
        (
            "a dict of one condition",
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n",
            true,
        ),
        (
            "a dict of conditions launchd may not meet",
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>PathState</key>\n\t\t<dict>\n\t\t\t<key>/x</key>\n\t\t\t<true/>\n\t\t</dict>\n\t</dict>\n",
            true,
        ),
        (
            "the key is a nested dict's, not the job's",
            "\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>KeepAlive</key>\n\t\t<true/>\n\t</dict>\n",
            false,
        ),
    ];
    for (what, body, kept_alive) in cases {
        assert_eq!(keeps_alive(&plist_with(body)).unwrap(), kept_alive, "{what}");
    }
}

/// The metadata comment holds the encoded spec, which may hold `--` (XML forbids it in a comment): the
/// part of the plist launchd reads is what is decided.
#[skuld::test]
fn the_metadata_comment_is_not_read() {
    let text = plist_with("\t<key>KeepAlive</key>\n\t<true/>\n");
    assert!(text.contains("a--b"));
    assert!(keeps_alive(&text).unwrap());
}

/// A plist that cannot be read as the job's is an error saying why, never a guess either way.
#[skuld::test]
fn a_plist_that_cannot_be_read_is_an_error() {
    let key = |value: &str| format!("\t<key>KeepAlive</key>\n\t{value}\n");
    let cases = [
        ("not XML", "this is not a plist".to_string(), "Xml"),
        ("an empty file", String::new(), "Xml"),
        (
            "the root is an array",
            "<plist version=\"1.0\"><array/></plist>".to_string(),
            "NoDict",
        ),
        (
            "the key is last, with no value",
            plist_with("\t<key>KeepAlive</key>\n"),
            "NoValue",
        ),
        (
            "the key twice",
            plist_with(&format!("{}{}", key("<true/>"), key("<false/>"))),
            "Twice",
        ),
        ("a string", plist_with(&key("<string>true</string>")), "Type"),
        ("an integer", plist_with(&key("<integer>1</integer>")), "Type"),
        ("an array", plist_with(&key("<array/>")), "Type"),
    ];
    for (what, text, expected) in cases {
        let got = match keeps_alive(&text) {
            Ok(decided) => panic!("{what}: decided {decided}"),
            Err(Unreadable::Xml(_)) => "Xml",
            Err(Unreadable::NoDict) => "NoDict",
            Err(Unreadable::NoValue) => "NoValue",
            Err(Unreadable::Twice) => "Twice",
            Err(Unreadable::Type(_)) => "Type",
        };
        assert_eq!(got, expected, "{what}");
    }
}
