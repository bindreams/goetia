use super::test_support::{abs, non_utf8_absolute_path, plain_fields};
use super::*;

fn fields() -> DaemonFields {
    DaemonFields {
        cwd: Some(abs("opt/frpc")),
        logs: Some(abs("var/log/frpc.log")),
        env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
        user: User::Name("svc-frpc".to_string()),
        ..plain_fields("frpc", vec![abs("bin/frpc").to_string_lossy().into_owned()])
    }
}

fn message(fields: DaemonFields) -> String {
    match DaemonSpec::new(fields) {
        Err(Error::Invalid { message, .. }) => message,
        other => panic!("expected Error::Invalid, got {other:?}"),
    }
}

#[skuld::test]
fn new_accepts_a_valid_spec_and_reads_through_deref() {
    let spec = DaemonSpec::new(fields()).expect("a valid spec");
    assert_eq!(spec.id.as_str(), "frpc");
    assert_eq!(spec.restart, Restart::Never);
    assert_eq!(spec.into_fields(), fields());
}

#[skuld::test]
fn new_refuses_a_control_character() {
    let msg = message(DaemonFields {
        name: "a\nExecStart=/bin/evil".to_string(),
        ..fields()
    });
    assert_eq!(msg, "field `name` contains a control character, which is forbidden");
}

#[skuld::test]
fn new_accepts_a_trailing_backslash() {
    let spec = DaemonSpec::new(DaemonFields {
        name: "frpc\\".to_string(),
        ..fields()
    })
    .expect("a trailing backslash is a legitimate value");
    assert_eq!(spec.name, "frpc\\");
}

#[skuld::test]
fn new_refuses_an_empty_command() {
    let msg = message(DaemonFields {
        command: vec![],
        ..fields()
    });
    assert_eq!(msg, "command must not be empty");
}

#[skuld::test]
fn new_refuses_an_empty_command_program() {
    let msg = message(DaemonFields {
        command: vec![String::new(), "-c".to_string()],
        ..fields()
    });
    assert_eq!(msg, "field `command[0]` must not be empty");
}

#[skuld::test]
fn new_refuses_a_relative_command_program_and_accepts_a_relative_argument() {
    let msg = message(DaemonFields {
        command: vec!["bin/frpc".to_string()],
        ..fields()
    });
    assert_eq!(msg, "field `command[0]` must be an absolute path, got `bin/frpc`");

    let program = abs("bin/frpc").to_string_lossy().into_owned();
    DaemonSpec::new(DaemonFields {
        command: vec![program, "conf/frpc.toml".to_string()],
        ..fields()
    })
    .expect("only the program must be absolute");
}

#[skuld::test]
fn new_refuses_a_relative_cwd_and_logs() {
    let msg = message(DaemonFields {
        cwd: Some(PathBuf::from("relative")),
        ..fields()
    });
    assert!(msg.starts_with("field `cwd` must be an absolute path"), "{msg}");

    let msg = message(DaemonFields {
        logs: Some(PathBuf::from("relative.log")),
        ..fields()
    });
    assert!(msg.starts_with("field `logs` must be an absolute path"), "{msg}");
}

#[skuld::test]
fn new_refuses_a_cwd_and_logs_that_are_not_valid_utf8() {
    let bad = non_utf8_absolute_path();
    let msg = message(DaemonFields {
        cwd: Some(bad.clone()),
        ..fields()
    });
    assert!(
        msg.starts_with("field `cwd`") && msg.ends_with("is not valid UTF-8"),
        "{msg}"
    );

    let msg = message(DaemonFields {
        logs: Some(bad),
        ..fields()
    });
    assert!(
        msg.starts_with("field `logs`") && msg.ends_with("is not valid UTF-8"),
        "{msg}"
    );
}

#[skuld::test]
fn new_refuses_an_env_key_with_equals() {
    let msg = message(DaemonFields {
        env: BTreeMap::from([("A=B".to_string(), "v".to_string())]),
        ..fields()
    });
    assert_eq!(msg, "env key `A=B` must not contain `=`");
}

#[skuld::test]
fn new_refuses_a_control_character_in_an_env_value() {
    let msg = message(DaemonFields {
        env: BTreeMap::from([("A".to_string(), "x\ny".to_string())]),
        ..fields()
    });
    assert_eq!(msg, "field `env[A]` contains a control character, which is forbidden");
}

#[skuld::test]
fn new_refuses_a_blank_user_name() {
    let msg = message(DaemonFields {
        user: User::Name("  ".to_string()),
        ..fields()
    });
    assert_eq!(msg, "field `user.name` must not be empty");
}

#[skuld::test]
fn new_refuses_xml_noncharacters_in_every_emitted_field() {
    for c in ['\u{FFFF}', '\u{FDD0}'] {
        let cases = [
            (
                "name",
                DaemonFields {
                    name: format!("bad{c}name"),
                    ..fields()
                },
            ),
            (
                "command",
                DaemonFields {
                    command: vec!["frpc".to_string(), format!("bad{c}arg")],
                    ..fields()
                },
            ),
            (
                "env key",
                DaemonFields {
                    env: BTreeMap::from([(format!("BAD{c}"), "v".to_string())]),
                    ..fields()
                },
            ),
            (
                "env[A]",
                DaemonFields {
                    env: BTreeMap::from([("A".to_string(), format!("bad{c}value"))]),
                    ..fields()
                },
            ),
            (
                "cwd",
                DaemonFields {
                    cwd: Some(abs(&format!("bad{c}dir"))),
                    ..fields()
                },
            ),
            (
                "logs",
                DaemonFields {
                    logs: Some(abs(&format!("bad{c}log"))),
                    ..fields()
                },
            ),
            (
                "user.name",
                DaemonFields {
                    user: User::Name(format!("bad{c}name")),
                    ..fields()
                },
            ),
            (
                "user.id",
                DaemonFields {
                    user: User::Id(AccountId::Sid(format!("bad{c}sid"))),
                    ..fields()
                },
            ),
        ];
        for (field, bad) in cases {
            assert_eq!(
                message(bad),
                format!(
                    "field `{field}` contains U+{:04X}, a Unicode noncharacter that XML cannot represent",
                    c as u32
                ),
                "{field}"
            );
        }
    }
}

#[skuld::test]
fn new_accepts_a_dollar_as_ordinary_text() {
    // A finished daemon holds no references: a `$` an env key or a SID carries, or an argument
    // like `${X}`, is what the daemon is told, so only a manifest's text is held to the grammar.
    let spec = DaemonSpec::new(DaemonFields {
        command: vec![abs("bin/frpc").to_string_lossy().into_owned(), "${X}".to_string()],
        env: BTreeMap::from([("A$B".to_string(), "$C".to_string())]),
        user: User::Id(AccountId::Sid("S-1-5-$".to_string())),
        ..fields()
    })
    .expect("`$` is ordinary text in a finished daemon");
    let decoded = crate::blob::decode(&crate::blob::encode(&spec)).expect("a valid spec round-trips");
    assert_eq!(decoded.spec, spec);
}
