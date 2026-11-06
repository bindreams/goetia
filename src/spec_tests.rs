use super::*;

fn abs(p: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"C:\{p}"))
    } else {
        PathBuf::from(format!("/{p}"))
    }
}

fn fields() -> DaemonFields {
    DaemonFields {
        cwd: Some(abs("opt/frpc")),
        logs: Some(abs("var/log/frpc.log")),
        env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
        user: User::Name("svc-frpc".to_string()),
        ..DaemonSpec::for_test("frpc", vec![abs("bin/frpc").to_string_lossy().into_owned()])
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
fn new_refuses_a_trailing_backslash() {
    let msg = message(DaemonFields {
        name: "frpc\\".to_string(),
        ..fields()
    });
    assert!(msg.contains("ends in a backslash"), "{msg}");
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
fn new_refuses_xml_noncharacters() {
    for c in ['\u{FFFF}', '\u{FDD0}'] {
        let msg = message(DaemonFields {
            command: vec!["frpc".to_string(), format!("bad{c}arg")],
            ..fields()
        });
        assert_eq!(
            msg,
            format!(
                "field `command` contains U+{:04X}, a Unicode noncharacter that XML cannot represent",
                c as u32
            )
        );
    }
}
