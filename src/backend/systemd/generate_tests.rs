use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::decide::{ExternalConfig, Outcome, Ownership, decide};
use crate::spec::DaemonFields;
use crate::spec::{Id, Kind, User};

// Fixtures ============================================================================================================
//
// Every `DaemonSpec` here is built through `DaemonSpec::new`, never `spec::resolve()`, and `new`
// requires `cwd`/`logs` to be absolute, which is platform-defined (`Path::is_absolute()` is `false`
// for a bare `/opt/frpc` on Windows). `abs` builds a path that is absolute on the host running the
// test, matching the pattern `blob_tests.rs` established.

#[cfg(windows)]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\{p}"))
}
#[cfg(not(windows))]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!("/{p}"))
}

fn full_spec() -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
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
        command: vec!["frpc".to_string()],
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

    assert_eq!(unit(&spec, &id).unwrap(), expected);
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

    assert_eq!(unit(&spec, &id).unwrap(), expected);
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
        let text = unit(&spec, &id).unwrap();
        let blob = extract(&text).unwrap().unwrap();

        assert_eq!(blob.spec, spec);
        assert_eq!(blob.schema, blob::SCHEMA);
        assert_eq!(blob.version, crate::version());
    }
}

#[skuld::test]
fn unit_is_the_generation_invariant() {
    for (spec, id) in [(full_spec(), full_identity()), (minimal_spec(), minimal_identity())] {
        let first = unit(&spec, &id).unwrap();
        let recovered = extract(&first).unwrap().unwrap().spec;
        let second = unit(&recovered, &id).unwrap();

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
    let today = unit(&spec, &id).unwrap();

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

    let regenerated = unit(&blob.spec, &id).unwrap();
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
    let one_section = unit(&spec, &minimal_identity()).unwrap();
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
    let mut spec = minimal_spec().into_fields();
    spec.command = vec!["/usr/bin/foo bar".to_string(), "--title=say \"hi\"".to_string()];
    let spec = DaemonSpec::new(spec).unwrap();
    let id = minimal_identity();

    let text = unit(&spec, &id).unwrap();

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
    let mut spec = minimal_spec().into_fields();
    spec.command = vec!["/usr/bin/find".to_string(), "-exec".to_string(), ";".to_string()];
    let spec = DaemonSpec::new(spec).unwrap();

    let text = unit(&spec, &minimal_identity()).unwrap();

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=/usr/bin/find -exec \";\"");
}

#[skuld::test]
fn argv_dollar_sign_is_doubled() {
    // ExecStart= performs `$FOO`/`${FOO}` environment-variable
    // substitution (systemd.service(5), "Command Lines"); an
    // unescaped `$` in a literal argument would be expanded or, if the
    // variable is unset, deleted from argv entirely.
    let mut spec = minimal_spec().into_fields();
    spec.command = vec![
        "/usr/bin/report".to_string(),
        "--home=${HOME}".to_string(),
        "$ARGS".to_string(),
    ];
    let spec = DaemonSpec::new(spec).unwrap();

    let text = unit(&spec, &minimal_identity()).unwrap();

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=/usr/bin/report --home=$${HOME} $$ARGS");
}

#[skuld::test]
fn an_executable_path_with_a_dollar_is_written_literally_and_argv0_separately() {
    // systemd takes the executable path as written, but runs `$FOO`/`${FOO}` substitution and the
    // `$$` collapse over argv[0] (measured on 242 and 255). So the path is written undoubled
    // behind `@`, which makes the next word argv[0], and that word is dollar-doubled. Later
    // arguments keep the doubling (`argv_dollar_sign_is_doubled`).
    for (executable, written) in [
        ("/opt/a$b/bin", "ExecStart=@/opt/a$b/bin /opt/a$$b/bin $$X"),
        ("/opt/a${b}/bin", "ExecStart=@/opt/a${b}/bin /opt/a$${b}/bin $$X"),
        ("/opt/a$$b/bin", "ExecStart=@/opt/a$$b/bin /opt/a$$$$b/bin $$X"),
        ("/opt/bin$", "ExecStart=@/opt/bin$ /opt/bin$$ $$X"),
        // `%` is doubled in both words; a word that needs quoting is quoted whole, `@` inside.
        (
            "/opt/a b$c/bin%x",
            "ExecStart=\"@/opt/a b$c/bin%%x\" \"/opt/a b$$c/bin%%x\" $$X",
        ),
    ] {
        let spec = with_fields(|f| f.command = vec![executable.to_string(), "$X".to_string()]);
        let text = unit(&spec, &minimal_identity()).unwrap();
        assert_eq!(line_of(&text, "ExecStart="), written, "{executable:?}");
        assert_eq!(extract(&text).unwrap().unwrap().spec, spec, "{executable:?}");
    }
}

#[skuld::test]
fn an_executable_path_without_a_dollar_gets_no_separate_argv0() {
    let spec = with_fields(|f| f.command = vec!["/opt/a b/bin".to_string(), "x".to_string()]);
    let text = unit(&spec, &minimal_identity()).unwrap();
    assert_eq!(line_of(&text, "ExecStart="), "ExecStart=\"/opt/a b/bin\" x");
}

#[skuld::test]
fn a_command_prefix_character_on_the_executable_is_refused_because_quoting_does_not_neutralise_it() {
    // systemd reads a leading `-`/`@`/`:`/`+`/`!` of the first word as a command prefix after
    // unquoting (measured on 242 and 255: `ExecStart="+/x"` still runs as root, ignoring `User=`),
    // so no encoding writes such an executable literally.
    for executable in ["+svc", "+svc$x", "-svc", "@svc", ":svc", "!svc"] {
        let spec = with_fields(|f| f.command = vec![executable.to_string()]);
        let err = unit(&spec, &minimal_identity()).unwrap_err();
        let Error::Invalid { daemon, message, .. } = &err else {
            panic!("{executable:?}: expected Error::Invalid, got {err:?}");
        };
        assert_eq!(daemon, "frpc");
        assert!(
            message.contains(&format!("{executable:?}")),
            "{executable:?}: {message}"
        );
        assert!(message.contains("`command[0]`"), "{executable:?}: {message}");
        assert!(message.contains("command prefix"), "{executable:?}: {message}");
        assert!(message.contains("systemd"), "{executable:?}: {message}");
    }
}

#[skuld::test]
fn a_prefix_character_inside_the_executable_is_not_refused() {
    let spec = with_fields(|f| f.command = vec!["/opt/a-b@c:d+e!f/bin".to_string()]);
    assert!(unit(&spec, &minimal_identity()).is_ok());
}

#[skuld::test]
fn argv_command_prefix_character_on_a_later_argument_is_not_quoted() {
    // The prefix-character rule applies only to ExecStart='s first word
    // (the executable); a later argument starting with `-` is an
    // ordinary flag and must not be quoted just because it starts with
    // one of these characters.
    let mut spec = minimal_spec().into_fields();
    spec.command = vec!["/usr/bin/foo".to_string(), "-v".to_string()];
    let spec = DaemonSpec::new(spec).unwrap();

    let text = unit(&spec, &minimal_identity()).unwrap();

    let exec_start_line = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert_eq!(exec_start_line, "ExecStart=/usr/bin/foo -v");
}

#[skuld::test]
fn percent_is_doubled_in_every_value() {
    let spec = DaemonSpec::new(DaemonFields {
        id: Id::try_from("frpc").unwrap(),
        name: "100% Uptime".to_string(),
        command: vec!["cmd%1".to_string(), "arg%2 with space".to_string()],
        cwd: Some(abs("opt/a%b")),
        env: BTreeMap::from([("KEY".to_string(), "v%alue".to_string())]),
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

    let text = unit(&spec, &id).unwrap();

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

// Backslashes =========================================================================================================
//
// Every expectation below was measured against systemd 242 and 255: a line-ending backslash continues the
// line, and the reader drops trailing whitespace, so a raw value gets one trailing space; a
// quoted word reads `\\` as one backslash and needs no guard.

/// How many backslashes end `text`.
fn trailing_backslashes(text: &str) -> usize {
    text.len() - text.trim_end_matches('\\').len()
}

fn with_fields(change: impl FnOnce(&mut DaemonFields)) -> DaemonSpec {
    let mut fields = minimal_spec().into_fields();
    change(&mut fields);
    DaemonSpec::new(fields).unwrap()
}

/// The first line of `text` that starts with `key`.
fn line_of<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(key))
        .unwrap_or_else(|| panic!("no `{key}` line in:\n{text}"))
}

#[skuld::test]
fn a_raw_value_ending_in_an_odd_run_of_backslashes_gets_a_trailing_space() {
    // (value, the line's text after `=`)
    for (name, emitted) in [
        (r"a\", r"a\ "),
        (r"a\\", r"a\\"),
        (r"a\\\", r"a\\\ "),
        (r"a\\\\", r"a\\\\"),
        (r"\", r"\ "),
        (r"a\b", r"a\b"),
        (r"a\\b", r"a\\b"),
        (r"a\nb", r"a\nb"),
    ] {
        let spec = with_fields(|f| f.name = name.to_string());
        let text = unit(&spec, &minimal_identity()).unwrap();
        assert_eq!(
            line_of(&text, "Description="),
            format!("Description={emitted}"),
            "{name:?}"
        );
    }
}

#[skuld::test]
fn a_working_directory_ending_in_backslashes_is_written_so_systemd_reads_it_back() {
    for (suffix, guard) in [(r"\", " "), (r"\\", ""), (r"\\\", " "), (r"\b", ""), (r"\\b", "")] {
        let cwd = format!("{}{suffix}", abs("opt/x").display());
        let spec = with_fields(|f| f.cwd = Some(PathBuf::from(&cwd)));
        let text = unit(&spec, &minimal_identity()).unwrap();
        assert_eq!(
            line_of(&text, "WorkingDirectory="),
            format!("WorkingDirectory={cwd}{guard}"),
            "{cwd:?}"
        );
    }
}

#[skuld::test]
fn a_log_path_ending_in_backslashes_is_written_so_systemd_reads_it_back() {
    for (suffix, guard) in [(r"\", " "), (r"\\", ""), (r"\\\", " "), (r"\b", ""), (r"\\b", "")] {
        let logs = format!("{}{suffix}", abs("var/log/x").display());
        let spec = with_fields(|f| f.logs = Some(PathBuf::from(&logs)));
        let text = unit(&spec, &minimal_identity()).unwrap();
        for key in ["StandardOutput=", "StandardError="] {
            assert_eq!(
                line_of(&text, key),
                format!("{key}append:{logs}{guard}"),
                "{key} {logs:?}"
            );
        }
    }
}

#[skuld::test]
fn a_user_ending_in_backslashes_is_written_so_systemd_reads_it_back() {
    for (user, emitted) in [(r"a\", r"a\ "), (r"a\\", r"a\\"), (r"a\\\", r"a\\\ "), (r"a\b", r"a\b")] {
        let text = unit(&minimal_spec(), &Identity { user: user.to_string() }).unwrap();
        assert_eq!(line_of(&text, "User="), format!("User={emitted}"), "{user:?}");
    }
}

#[skuld::test]
fn an_argument_with_backslashes_is_quoted_with_each_backslash_doubled() {
    let spec = with_fields(|f| {
        f.command = vec![
            "/bin/x".to_string(),
            r"C:\".to_string(),
            r"C:\\".to_string(),
            r"C:\\\".to_string(),
            r"a\b".to_string(),
            r"\".to_string(),
        ]
    });
    let text = unit(&spec, &minimal_identity()).unwrap();
    assert_eq!(
        line_of(&text, "ExecStart="),
        r#"ExecStart=/bin/x "C:\\" "C:\\\\" "C:\\\\\\" "a\\b" "\\""#
    );
}

#[skuld::test]
fn an_environment_value_with_backslashes_is_quoted_with_each_backslash_doubled() {
    let spec = with_fields(|f| {
        f.env = BTreeMap::from([
            ("A".to_string(), r"C:\".to_string()),
            ("B".to_string(), r"C:\\".to_string()),
            ("C".to_string(), r"C:\\\".to_string()),
            ("D".to_string(), r"a\b".to_string()),
        ])
    });
    let text = unit(&spec, &minimal_identity()).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("Environment=")).collect();
    assert_eq!(
        lines,
        [
            r#"Environment="A=C:\\""#,
            r#"Environment="B=C:\\\\""#,
            r#"Environment="C=C:\\\\\\""#,
            r#"Environment="D=a\\b""#,
        ]
    );
}

#[skuld::test]
fn no_directive_ends_in_a_line_continuation_and_the_unit_stays_in_order() {
    // The privilege boundary: a `Description=` that continued onto the next line would swallow
    // `User=` and run the daemon as root.
    for tail in [r"\", r"\\", r"\\\", r"\\\\"] {
        let spec = with_fields(|f| {
            f.name = format!("n{tail}");
            f.command = vec!["/bin/x".to_string(), format!("a{tail}")];
            f.cwd = Some(PathBuf::from(format!("{}{tail}", abs("opt/x").display())));
            f.logs = Some(PathBuf::from(format!("{}{tail}", abs("var/log/x").display())));
            f.env = BTreeMap::from([("K".to_string(), format!("v{tail}"))]);
        });
        let id = Identity {
            user: format!("u{tail}"),
        };
        let text = unit(&spec, &id).unwrap();

        for line in text.lines() {
            assert_eq!(
                trailing_backslashes(line) % 2,
                0,
                "{tail:?}: `{line}` continues onto the next line"
            );
        }
        let keys: Vec<&str> = text.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
        assert_eq!(
            keys,
            [
                "Description",
                "Type",
                "ExecStart",
                "WorkingDirectory",
                "Environment",
                "User",
                "Restart",
                "StandardOutput",
                "StandardError",
                "WantedBy",
                "Marker",
                "Schema",
                "Version",
                "Spec",
            ],
            "{tail:?}"
        );
        assert_eq!(extract(&text).unwrap().unwrap().spec, spec, "{tail:?}");
    }
}

#[skuld::test]
fn an_executable_path_with_a_backslash_or_quote_is_refused_because_systemd_refuses_to_load_it() {
    // systemd 255 rejects the unit with "Executable name contains special characters" however the
    // path is quoted or escaped; there is no encoding to write it in.
    for executable in [r"/bin/x\y", r"/bin/x\", r"/bin/x\\", "/bin/x\"y", "/bin/x'y"] {
        let spec = with_fields(|f| f.command = vec![executable.to_string()]);
        let err = unit(&spec, &minimal_identity()).unwrap_err();
        let Error::Invalid { daemon, message, .. } = &err else {
            panic!("{executable:?}: expected Error::Invalid, got {err:?}");
        };
        assert_eq!(daemon, "frpc");
        assert!(
            message.contains(&format!("{executable:?}")),
            "{executable:?}: {message}"
        );
        assert!(message.contains("`command[0]`"), "{executable:?}: {message}");
        assert!(message.contains("systemd"), "{executable:?}: {message}");
    }
}

#[skuld::test]
fn a_later_argument_may_hold_a_backslash_or_quote() {
    let spec = with_fields(|f| {
        f.command = vec![
            "/bin/x".to_string(),
            r"a\b".to_string(),
            "a\"b".to_string(),
            "a'b".to_string(),
        ]
    });
    assert!(unit(&spec, &minimal_identity()).is_ok());
}

// Environment names ===================================================================================================
//
// Measured on systemd 242 and 255: an `Environment=` assignment whose name is not
// `[A-Za-z_][A-Za-z0-9_]*` is logged "Invalid environment assignment, ignoring" and dropped, while
// the unit still loads and starts. Quoting the word does not change that.

#[skuld::test]
fn an_environment_name_systemd_would_drop_is_refused() {
    for key in [
        r"K\",
        r"K\\",
        r"\",
        r"K\x",
        "1K",
        "K-1",
        "K.1",
        "K:1",
        "K b",
        "K\"1",
        "K'1",
        "K$1",
        "K%1",
        "K;1",
        "K\u{e9}",
        "\u{43a}\u{43b}\u{44e}\u{447}",
        "-K",
    ] {
        let spec = with_fields(|f| f.env = BTreeMap::from([(key.to_string(), "v".to_string())]));
        let err = unit(&spec, &minimal_identity()).unwrap_err();
        let Error::Invalid { daemon, message, .. } = &err else {
            panic!("{key:?}: expected Error::Invalid, got {err:?}");
        };
        assert_eq!(daemon, "frpc");
        assert!(message.contains(&format!("{key:?}")), "{key:?}: {message}");
        assert!(message.contains("Environment="), "{key:?}: {message}");
        assert!(message.contains("systemd"), "{key:?}: {message}");
    }
}

#[skuld::test]
fn an_environment_name_systemd_accepts_is_written_and_read_back() {
    for key in ["K", "k", "_K", "K1", "K_1", "_", "__COSCA_X", "kEy_9"] {
        let spec = with_fields(|f| f.env = BTreeMap::from([(key.to_string(), "v".to_string())]));
        let text = unit(&spec, &minimal_identity()).unwrap();
        assert_eq!(
            line_of(&text, "Environment="),
            format!("Environment={key}=v"),
            "{key:?}"
        );
        assert_eq!(extract(&text).unwrap().unwrap().spec, spec, "{key:?}");
    }
}

// Leading and trailing whitespace =====================================================================================
//
// Measured against systemd 255: `Description=`, `User=`, `WorkingDirectory=` and the `append:` path
// are read raw and stripped of leading and trailing space, tab, LF and CR; no quoting, C escape
// or continuation keeps it. Other whitespace (U+00A0, U+3000, VT, FF) survives. A quoted word keeps
// all of it, except that an executable path ending in whitespace is not run.

/// `unit` must refuse with a message that names `what` and says systemd strips the whitespace.
fn assert_refused_as_stripped(result: Result<String, Error>, what: &str, value: &str) {
    let err = result.expect_err(&format!("{what} {value:?} would read back stripped"));
    let Error::Invalid { daemon, message, .. } = &err else {
        panic!("{what} {value:?}: expected Error::Invalid, got {err:?}");
    };
    assert_eq!(daemon, "frpc");
    assert!(message.contains(what), "{what} {value:?}: {message}");
    assert!(message.contains("systemd"), "{what} {value:?}: {message}");
    assert!(message.contains("whitespace"), "{what} {value:?}: {message}");
}

#[skuld::test]
fn a_description_with_leading_or_trailing_whitespace_is_refused() {
    for name in [" a", "a ", " a ", "  a", "a  ", " "] {
        let spec = with_fields(|f| f.name = name.to_string());
        assert_refused_as_stripped(unit(&spec, &minimal_identity()), "Description=", name);
    }
}

#[skuld::test]
fn a_user_with_leading_or_trailing_whitespace_is_refused() {
    for user in [" a", "a ", "a\t", "\ta", "a\n", "a\r"] {
        let result = unit(&minimal_spec(), &Identity { user: user.to_string() });
        assert_refused_as_stripped(result, "User=", user);
    }
}

#[skuld::test]
fn a_working_directory_ending_in_whitespace_is_refused() {
    for tail in [" ", "  ", r"\ "] {
        let cwd = format!("{}{tail}", abs("opt/x").display());
        let spec = with_fields(|f| f.cwd = Some(PathBuf::from(&cwd)));
        assert_refused_as_stripped(unit(&spec, &minimal_identity()), "WorkingDirectory=", &cwd);
    }
}

#[skuld::test]
fn a_log_path_ending_in_whitespace_is_refused() {
    for tail in [" ", "  ", r"\ "] {
        let logs = format!("{}{tail}", abs("var/log/x").display());
        let spec = with_fields(|f| f.logs = Some(PathBuf::from(&logs)));
        assert_refused_as_stripped(unit(&spec, &minimal_identity()), "StandardOutput=", &logs);
    }
}

#[skuld::test]
fn an_executable_path_ending_in_whitespace_is_refused_because_systemd_does_not_run_it() {
    let spec = with_fields(|f| f.command = vec!["/bin/x ".to_string()]);
    let err = unit(&spec, &minimal_identity()).unwrap_err();
    let Error::Invalid { daemon, message, .. } = &err else {
        panic!("expected Error::Invalid, got {err:?}");
    };
    assert_eq!(daemon, "frpc");
    assert!(message.contains("\"/bin/x \""), "{message}");
    assert!(message.contains("`command[0]`"), "{message}");
    assert!(message.contains("whitespace"), "{message}");
    assert!(message.contains("systemd"), "{message}");
}

#[skuld::test]
fn whitespace_systemd_keeps_is_written_unchanged() {
    // Interior whitespace, and edge whitespace systemd does not strip, in every raw directive.
    for value in ["a b", "a  b", "a\u{a0}", "\u{3000}a", "a\u{2003}"] {
        let spec = with_fields(|f| f.name = value.to_string());
        let text = unit(&spec, &minimal_identity()).unwrap();
        assert_eq!(
            line_of(&text, "Description="),
            format!("Description={value}"),
            "{value:?}"
        );
    }
    let spec = with_fields(|f| {
        f.cwd = Some(PathBuf::from(format!("{}/a b/c", abs("opt/x").display())));
        f.command = vec!["/bin/a b".to_string()];
    });
    assert!(unit(&spec, &minimal_identity()).is_ok());
}

#[skuld::test]
fn an_argument_and_an_environment_value_with_edge_whitespace_are_quoted_whole() {
    let spec = with_fields(|f| {
        f.command = vec![
            "/bin/x".to_string(),
            " a".to_string(),
            "a ".to_string(),
            " a ".to_string(),
            " ".to_string(),
        ];
        f.env = BTreeMap::from([
            ("K".to_string(), " v ".to_string()),
            ("L".to_string(), "v ".to_string()),
        ]);
    });
    let text = unit(&spec, &minimal_identity()).unwrap();
    assert_eq!(line_of(&text, "ExecStart="), r#"ExecStart=/bin/x " a" "a " " a " " ""#);
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("Environment=")).collect();
    assert_eq!(lines, [r#"Environment="K= v ""#, r#"Environment="L=v ""#]);
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
        let text = unit(&spec, &minimal_identity()).unwrap();

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

    // On a Windows host `resolve` makes `command[0]` a `C:\...` path, which systemd cannot load.
    let mut fields = spec.into_fields();
    fields.command[0] = "/bin/frpc".to_string();
    let spec = DaemonSpec::new(fields).unwrap();

    let text = unit(&spec, &minimal_identity()).unwrap();
    let exec_start = text.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
    assert!(
        exec_start.ends_with(" $$ARGS"),
        "ExecStart should re-double the escaped dollar: {exec_start}"
    );
}
