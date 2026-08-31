use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::spec::{Id, Kind, User};

// Fixtures ============================================================================================================
//
// Every `DaemonSpec` here is constructed literally, never via
// `spec::resolve()`. But `blob::decode` (which `extract` calls) re-checks
// `DaemonSpec`'s invariants, including that `cwd`/`logs` are absolute —
// and absoluteness is platform-defined (`Path::is_absolute()` is `false`
// for a bare `/opt/frpc` on Windows). `abs` builds a path that is
// actually absolute on the host running the test, matching the pattern
// `blob_tests.rs` already established.

#[cfg(windows)]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\{p}"))
}
#[cfg(not(windows))]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!("/{p}"))
}

fn full_spec() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "Frpc Tunnel".to_string(),
        command: vec!["frpc".to_string(), "-c".to_string(), "frpc.toml".to_string()],
        cwd: Some(abs("opt/frpc")),
        env: BTreeMap::from([
            ("RUST_LOG".to_string(), "info".to_string()),
            ("URL".to_string(), "http://x/a%20b".to_string()),
        ]),
        user: User::Name("svc-frpc".to_string()),
        restart: Restart::Always,
        restart_delay: Some(Duration::new(2, 500_000_000)),
        logs: Some(abs("var/log/frpc.log")),
        kind: Kind::Managed,
    }
}

fn full_identity() -> Identity {
    Identity {
        user: "svc-frpc".to_string(),
    }
}

fn minimal_spec() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "frpc".to_string(),
        command: vec!["frpc".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

fn minimal_identity() -> Identity {
    Identity { user: "0".to_string() }
}

// Snapshots ===========================================================================================================

#[skuld::test]
fn unit_snapshot_full() {
    let spec = full_spec();
    let id = full_identity();

    let expected = format!(
        "[Unit]\n\
         Description=Frpc Tunnel\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=frpc -c frpc.toml\n\
         WorkingDirectory={cwd}\n\
         Environment=RUST_LOG=info\n\
         Environment=URL=http://x/a%%20b\n\
         User=svc-frpc\n\
         Restart=always\n\
         RestartSec=2.5\n\
         StandardOutput=append:{logs}\n\
         StandardError=append:{logs}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n\
         \n\
         [X-Goetia]\n\
         Marker=goetia\n\
         Schema=1\n\
         Version={version}\n\
         Spec={spec_b64}\n",
        cwd = abs("opt/frpc").display(),
        logs = abs("var/log/frpc.log").display(),
        version = crate::version(),
        spec_b64 = blob::encode(&spec),
    );

    assert_eq!(unit(&spec, &id), expected);
}

#[skuld::test]
fn unit_snapshot_minimal() {
    let spec = minimal_spec();
    let id = minimal_identity();

    let expected = format!(
        "[Unit]\n\
         Description=frpc\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=frpc\n\
         User=0\n\
         Restart=no\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n\
         \n\
         [X-Goetia]\n\
         Marker=goetia\n\
         Schema=1\n\
         Version={version}\n\
         Spec={spec_b64}\n",
        version = crate::version(),
        spec_b64 = blob::encode(&spec),
    );

    assert_eq!(unit(&spec, &id), expected);
}

// The generation invariant ============================================================================================
//
// Exercised over both fixtures: `full_spec` (every optional field
// populated) and `minimal_spec` (every optional field at its `None`/empty
// default) — a bug in how `extract` reconstructs an absent field would
// not show up against `full_spec` alone.

#[skuld::test]
fn unit_round_trips_through_extract() {
    for (spec, id) in [(full_spec(), full_identity()), (minimal_spec(), minimal_identity())] {
        let text = unit(&spec, &id);
        let blob = extract(&text).unwrap().unwrap();

        assert_eq!(blob.spec, spec);
        assert_eq!(blob.schema, blob::SCHEMA);
        assert_eq!(blob.version, crate::version());
    }
}

#[skuld::test]
fn unit_is_the_generation_invariant() {
    for (spec, id) in [(full_spec(), full_identity()), (minimal_spec(), minimal_identity())] {
        let first = unit(&spec, &id);
        let recovered = extract(&first).unwrap().unwrap().spec;
        let second = unit(&recovered, &id);

        assert_eq!(first, second);
    }
}

// Extract dispositions ================================================================================================

#[skuld::test]
fn extract_returns_none_for_foreign_unit() {
    let text = "[Unit]\nDescription=Some other service\n\n[Service]\nExecStart=/usr/bin/true\n";

    assert!(extract(text).unwrap().is_none());
}

#[skuld::test]
fn extract_errors_on_corrupt_blob() {
    let text =
        "[Unit]\nDescription=frpc\n\n[X-Goetia]\nMarker=goetia\nSchema=1\nVersion=0.1.0\nSpec=not-valid-base64!!!\n";

    assert!(extract(text).is_err());
}

#[skuld::test]
fn extract_errors_on_duplicate_x_goetia_section() {
    let spec = minimal_spec();
    let one_section = unit(&spec, &minimal_identity());
    // Both copies decode individually; duplication alone must still be refused.
    let text = format!("{one_section}\n{one_section}");

    let err = extract(&text).unwrap_err();
    assert!(format!("{err}").contains("X-Goetia"));
}

#[skuld::test]
fn extract_errors_on_duplicate_marker_line_within_one_section() {
    // A second `Marker=` (or `Spec=`, below) inside a single section is
    // the same attack as a duplicate section — a forged extra line
    // picking which value wins — reached through a cheaper edit.
    let text = "[X-Goetia]\nMarker=goetia\nMarker=goetia\nSpec=abcd\n";

    let err = extract(text).unwrap_err();
    assert!(format!("{err}").contains("Marker="));
}

#[skuld::test]
fn extract_errors_on_duplicate_spec_line_within_one_section() {
    let spec = minimal_spec();
    let genuine = blob::encode(&spec);
    let forged = blob::encode(&full_spec());
    let text = format!("[X-Goetia]\nMarker=goetia\nSpec={genuine}\nSpec={forged}\n");

    let err = extract(&text).unwrap_err();
    assert!(format!("{err}").contains("Spec="));
}

#[skuld::test]
fn extract_errors_on_spec_value_with_continuation() {
    // A trailing backslash before the newline is systemd's own line
    // continuation syntax; extract() rejects a hand-edited one rather
    // than silently decoding a truncated Spec= (see the check in
    // generate.rs, which this asserts by message rather than by mere
    // `is_err()` — the input is also invalid base64, so an `is_err()`
    // check alone would pass even with the guard deleted).
    let text = "[X-Goetia]\nMarker=goetia\nSpec=abcd\\\nefgh\n";

    let err = extract(text).unwrap_err();
    assert!(format!("{err}").contains("whitespace or a line continuation"));
}

#[skuld::test]
fn extract_errors_on_spec_value_with_embedded_whitespace() {
    let text = "[X-Goetia]\nMarker=goetia\nSpec=abcd efgh\n";

    let err = extract(text).unwrap_err();
    assert!(format!("{err}").contains("whitespace or a line continuation"));
}

#[skuld::test]
fn extract_errors_on_missing_marker_line() {
    let text = "[X-Goetia]\nSpec=abcd\n";

    let err = extract(text).unwrap_err();
    assert!(format!("{err}").contains("Marker="));
}

#[skuld::test]
fn extract_errors_on_wrong_marker_value() {
    let text = "[X-Goetia]\nMarker=not-goetia\nSpec=abcd\n";

    let err = extract(text).unwrap_err();
    assert!(format!("{err}").contains("Marker"));
}

#[skuld::test]
fn extract_errors_on_missing_spec_line() {
    let text = "[X-Goetia]\nMarker=goetia\n";

    let err = extract(text).unwrap_err();
    assert!(format!("{err}").contains("Spec="));
}

#[skuld::test]
fn extract_finds_the_section_when_it_is_not_the_last_in_the_file() {
    let spec = minimal_spec();
    let spec_b64 = blob::encode(&spec);
    let text = format!("[X-Goetia]\nMarker=goetia\nSpec={spec_b64}\n\n[SomeOtherSection]\nFoo=bar\n");

    let blob = extract(&text).unwrap().unwrap();
    assert_eq!(blob.spec, spec);
}

// Escaping ============================================================================================================

#[skuld::test]
fn argv_with_spaces_and_quotes_round_trips() {
    let mut spec = minimal_spec();
    spec.command = vec!["/usr/bin/foo bar".to_string(), "--title=say \"hi\"".to_string()];
    let id = minimal_identity();

    let text = unit(&spec, &id);

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(
        exec_start_line,
        "ExecStart=\"/usr/bin/foo bar\" \"--title=say \\\"hi\\\"\""
    );

    // The escaping is purely for systemd's own command-line parser; the
    // blob carries the real argv independently, so it must round-trip
    // exactly regardless of how ExecStart= had to be quoted.
    let recovered = extract(&text).unwrap().unwrap().spec;
    assert_eq!(recovered.command, spec.command);
}

#[skuld::test]
fn argv_lone_semicolon_is_quoted() {
    // A bare `;` word is systemd's ExecStart= command-line separator
    // (systemd.service(5)): "An argument solely consisting of ';' must
    // be escaped". Left unquoted, `find ... ;` would have everything
    // after the `;` parsed as a second command line.
    let mut spec = minimal_spec();
    spec.command = vec!["/usr/bin/find".to_string(), "-exec".to_string(), ";".to_string()];

    let text = unit(&spec, &minimal_identity());

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=/usr/bin/find -exec \";\"");
}

#[skuld::test]
fn argv_dollar_sign_is_doubled() {
    // ExecStart= performs `$FOO`/`${FOO}` environment-variable
    // substitution (systemd.service(5), "Command Lines"); an
    // unescaped `$` in a literal argument would be expanded or, if the
    // variable is unset, deleted from argv entirely.
    let mut spec = minimal_spec();
    spec.command = vec![
        "/usr/bin/report".to_string(),
        "--home=${HOME}".to_string(),
        "$ARGS".to_string(),
    ];

    let text = unit(&spec, &minimal_identity());

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=/usr/bin/report --home=$${HOME} $$ARGS");
}

#[skuld::test]
fn argv_command_prefix_character_on_the_executable_is_quoted() {
    // A leading `-`/`@`/`:`/`+`/`!` on ExecStart='s first word is a
    // systemd command-prefix directive (e.g. `+` runs with elevated
    // privileges, bypassing `User=`) — a plain executable path that
    // happens to start with one must not be reinterpreted as it.
    let mut spec = minimal_spec();
    spec.command = vec!["+not-a-privilege-directive".to_string()];

    let text = unit(&spec, &minimal_identity());

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=\"+not-a-privilege-directive\"");
}

#[skuld::test]
fn argv_command_prefix_character_on_a_later_argument_is_not_quoted() {
    // The prefix-character rule applies only to ExecStart='s first word
    // (the executable); a later argument starting with `-` is an
    // ordinary flag and must not be quoted just because it starts with
    // one of these characters.
    let mut spec = minimal_spec();
    spec.command = vec!["/usr/bin/foo".to_string(), "-v".to_string()];

    let text = unit(&spec, &minimal_identity());

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=/usr/bin/foo -v");
}

#[skuld::test]
fn percent_is_doubled_in_every_value() {
    let spec = DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "100% Uptime".to_string(),
        command: vec!["cmd%1".to_string(), "arg%2 with space".to_string()],
        cwd: Some(abs("opt/a%b")),
        env: BTreeMap::from([("K%EY".to_string(), "v%alue".to_string())]),
        user: User::Name("ignored-by-generate".to_string()),
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: Some(abs("var/log/a%b.log")),
        kind: Kind::Simple,
    };
    let id = Identity {
        user: "u%ser".to_string(),
    };

    let text = unit(&spec, &id);

    for line in text.lines() {
        // `Spec=`'s base64 alphabet never contains `%`, so it needs no
        // doubling and is exempt from this check.
        if line.starts_with("Spec=") {
            continue;
        }
        let collapsed = line.replace("%%", "");
        assert!(!collapsed.contains('%'), "line has an unescaped `%`: {line}");
    }
}

// StartLimitIntervalSec ===============================================================================================

#[skuld::test]
fn start_limit_disabled_when_restart_enabled() {
    for (restart, expect_start_limit) in [
        (Restart::Never, false),
        (Restart::OnFailure, true),
        (Restart::Always, true),
    ] {
        let mut spec = minimal_spec();
        spec.restart = restart;
        let text = unit(&spec, &minimal_identity());

        assert_eq!(
            text.lines().any(|l| l == "StartLimitIntervalSec=0"),
            expect_start_limit,
            "restart={restart:?}"
        );
    }
}
