use crate::spec::test_support::abs;
use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::decide::{ExternalConfig, Outcome, Ownership, decide};
use crate::spec::DaemonFields;
use crate::spec::{Id, Kind, User};

// Fixtures ============================================================================================================
//
// `DaemonSpec::new` requires absolute `command[0]`, `cwd` and `logs`, which is platform-defined; `abs` builds one
// for the host.

/// The program every fixture runs.
fn program() -> String {
    abs("bin/frpc").to_string_lossy().into_owned()
}

/// `program()` as `ExecStart=` spells it: a Windows path holds backslashes, which make systemd's
/// quoting wrap the word and double them.
fn program_in_exec_start() -> String {
    if cfg!(windows) {
        format!("\"{}\"", program().replace('\\', "\\\\"))
    } else {
        program()
    }
}

fn full_spec() -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "Frpc Tunnel".to_string(),
        command: vec![program(), "-c".to_string(), "frpc.toml".to_string()],
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
    })
    .unwrap()
}

fn full_identity() -> Identity {
    Identity {
        user: "svc-frpc".to_string(),
    }
}

fn minimal_spec() -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "frpc".to_string(),
        command: vec![program()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    })
    .unwrap()
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
         Type=exec\n\
         ExecStart={exec} -c frpc.toml\n\
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
        exec = program_in_exec_start(),
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
         Type=exec\n\
         ExecStart={exec}\n\
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
        exec = program_in_exec_start(),
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

// Upgrade path ========================================================================================================

/// Pins the mechanism `decide` already has for a generator change that carries no version bump of
/// its own: `0.0.9` is not and will never be this crate's version, so it stands in for "some
/// earlier release". Protects every FUTURE generator change, this one included — see `unit`'s doc
/// comment.
#[skuld::test]
fn a_unit_written_by_an_older_goetia_is_stale_not_a_conflict() {
    let spec = full_spec();
    let id = full_identity();
    let today = unit(&spec, &id);

    // The text an older goetia would have written for the same spec: `Type=simple`, and a blob
    // whose embedded version is `0.0.9` rather than today's.
    let older_spec_b64 = blob::encode_with_version(&spec, "0.0.9");
    let on_disk = today
        .replacen("Type=exec\n", "Type=simple\n", 1)
        .replacen(&format!("Version={}\n", crate::version()), "Version=0.0.9\n", 1)
        .replacen(
            &format!("Spec={}\n", blob::encode(&spec)),
            &format!("Spec={older_spec_b64}\n"),
            1,
        );
    assert_ne!(on_disk, today, "the fixture must actually differ from today's unit");

    let blob = extract(&on_disk).unwrap().unwrap();
    assert_eq!(blob.version, "0.0.9");

    let regenerated = unit(&blob.spec, &id);
    let outcome = decide(
        &Ownership::Ours {
            blob,
            regenerated,
            on_disk,
        },
        &today,
        &spec,
        crate::version(),
        false,
        &ExternalConfig::Absent,
    );

    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: "0.0.9".to_string()
        }
    );
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

// The rendering tests below call `render_exec_start` on literal words: a word the rules refuse
// (`+x` as the program) or a Unix path would not survive `DaemonSpec::new` on every host.

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

#[skuld::test]
fn argv_with_spaces_and_quotes_is_quoted() {
    assert_eq!(
        render_exec_start(&words(&["/usr/bin/foo bar", "--title=say \"hi\""])),
        "\"/usr/bin/foo bar\" \"--title=say \\\"hi\\\"\""
    );
}

#[skuld::test]
fn argv_with_spaces_and_quotes_round_trips() {
    let mut spec = minimal_spec().into_fields();
    spec.command = vec![
        abs("bin/foo bar").to_string_lossy().into_owned(),
        "--title=say \"hi\"".to_string(),
    ];
    let spec = DaemonSpec::new(spec).unwrap();
    let id = minimal_identity();

    let text = unit(&spec, &id);

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
    assert_eq!(
        render_exec_start(&words(&["/usr/bin/find", "-exec", ";"])),
        "/usr/bin/find -exec \";\""
    );
}

#[skuld::test]
fn argv_dollar_sign_is_doubled() {
    // ExecStart= performs `$FOO`/`${FOO}` environment-variable
    // substitution (systemd.service(5), "Command Lines"); an
    // unescaped `$` in a literal argument would be expanded or, if the
    // variable is unset, deleted from argv entirely.
    assert_eq!(
        render_exec_start(&words(&["/usr/bin/report", "--home=${HOME}", "$ARGS"])),
        "/usr/bin/report --home=$${HOME} $$ARGS"
    );
}

#[skuld::test]
fn argv_command_prefix_character_on_the_executable_is_quoted() {
    // A leading `-`/`@`/`:`/`+`/`!` on ExecStart='s first word is a
    // systemd command-prefix directive (e.g. `+` runs with elevated
    // privileges, bypassing `User=`) — a plain executable path that
    // happens to start with one must not be reinterpreted as it.
    assert_eq!(
        render_exec_start(&words(&["+not-a-privilege-directive"])),
        "\"+not-a-privilege-directive\""
    );
}

#[skuld::test]
fn argv_command_prefix_character_on_a_later_argument_is_not_quoted() {
    // The prefix-character rule applies only to ExecStart='s first word
    // (the executable); a later argument starting with `-` is an
    // ordinary flag and must not be quoted just because it starts with
    // one of these characters.
    assert_eq!(render_exec_start(&words(&["/usr/bin/foo", "-v"])), "/usr/bin/foo -v");
}

#[skuld::test]
fn percent_is_doubled_in_every_value() {
    let spec = DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "100% Uptime".to_string(),
        command: vec![
            abs("bin/cmd%1").to_string_lossy().into_owned(),
            "arg%2 with space".to_string(),
        ],
        cwd: Some(abs("opt/a%b")),
        env: BTreeMap::from([("K%EY".to_string(), "v%alue".to_string())]),
        user: User::Name("ignored-by-generate".to_string()),
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: Some(abs("var/log/a%b.log")),
        kind: Kind::Simple,
    })
    .unwrap();
    let id = Identity {
        user: "u%ser".to_string(),
    };

    let text = unit(&spec, &id);

    // One line the doubling must have reached, asserted outright: the loop
    // below checks only what `unit` emitted, so text it stopped emitting
    // would leave this test passing with nothing checked.
    assert!(text.contains("Description=100%% Uptime"), "{text}");
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
        let mut spec = minimal_spec().into_fields();
        spec.restart = restart;
        let spec = DaemonSpec::new(spec).unwrap();
        let text = unit(&spec, &minimal_identity());

        assert_eq!(
            text.lines().any(|l| l == "StartLimitIntervalSec=0"),
            expect_start_limit,
            "restart={restart:?}"
        );
    }
}

// Manifest escapes composing with generator escapes ===================================================================

#[skuld::test]
fn manifest_dollar_escape_survives_into_exec_start() {
    // Two independent `$` escape layers meet here: the manifest's `$$` ->
    // `$`, and `render_exec_arg`'s `$` -> `$$` for systemd's own
    // environment substitution. A user who writes `$$ARGS` must get the
    // literal argv element `$ARGS` and a unit that still says `$$ARGS`.
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::write(
        dir.path().join("goetia.yaml"),
        "id: frpc\ncommand: [/bin/frpc, $$ARGS]\n",
    )
    .expect("fixture manifest should be writable");

    let (spec, _warnings) = crate::spec::load(dir.path()).expect("manifest should load");
    assert_eq!(spec.command[1], "$ARGS");

    let text = unit(&spec, &minimal_identity());
    let exec_start = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert!(
        exec_start.ends_with(" $$ARGS"),
        "ExecStart should re-double the escaped dollar: {exec_start}"
    );
}
