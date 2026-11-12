use std::path::PathBuf;
use std::time::Duration;

use skuld::temp_dir;

use super::*;
use crate::spec::interpolate;
use crate::spec::test_support::{HOST_BACKENDS, abs, program};
use crate::spec::vars::Vars;
use crate::testing::plain_fields;

// `Path::new("/opt/rt").is_absolute()` is `false` on Windows, and
// `PathBuf::from(r"C:\base").join("/opt/rt")` yields `C:/opt/rt` — so every
// test below that exercises absolute-path resolution needs a base dir the
// host platform actually considers absolute.
fn base_dir() -> PathBuf {
    abs("base")
}

fn parse_manifest(yaml: &str) -> RawSpec {
    RawSpec::parse(yaml).expect("fixture yaml should parse")
}

fn resolve_yaml(yaml: &str) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    resolve(parse_manifest(yaml), &base_dir())
}

// Id ==================================================================================================================

#[skuld::test]
fn id_accepts_valid_pattern() {
    assert!(Id::try_from("frpc-2.local_v1").is_ok());
}

#[skuld::test]
fn id_rejects_out_of_pattern() {
    assert!(Id::try_from("").is_err(), "empty id should be rejected");

    let too_long = "a".repeat(81);
    assert!(
        Id::try_from(too_long.as_str()).is_err(),
        "81-char id should be rejected"
    );

    assert!(
        Id::try_from("has/slash").is_err(),
        "slash-bearing id should be rejected"
    );
}

// The injection gate ==================================================================================================

#[skuld::test]
fn rejects_control_characters_in_name() {
    let yaml = r#"
id: frpc
name: "evil\nExecStart=/bin/evil"
command: [bin/frpc]
"#;
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frpc"), "error should name the daemon: {msg}");
    assert!(msg.contains("name"), "error should name the field: {msg}");
}

#[skuld::test]
fn rejects_control_characters_in_env() {
    let yaml = r#"
id: frpc
command: ["bin/frpc"]
env:
  URL: "http://x/a\nEnvironment=EVIL=1"
"#;
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frpc"), "error should name the daemon: {msg}");
    assert!(msg.contains("env"), "error should name the field: {msg}");
}

#[skuld::test]
fn rejects_newline_in_argv() {
    let yaml = r#"
id: frpc
command: ["bin/frpc", "-c", "evil\nExecStart=/bin/evil"]
"#;
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frpc"), "error should name the daemon: {msg}");
    assert!(msg.contains("command"), "error should name the field: {msg}");
}

#[skuld::test]
fn rejects_equals_in_env_key() {
    let yaml = r#"
id: frpc
command: ["bin/frpc"]
env:
  "FOO=BAR": "1"
"#;
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frpc"), "error should name the daemon: {msg}");
    assert!(msg.contains('='), "error should call out the `=`: {msg}");
}

/// The characters that cut a string short or start a new directive, as the escape a YAML
/// double-quoted scalar takes for each.
const CONTROL_CHARACTERS: [(char, &str); 2] = [('\n', r"\n"), ('\0', r"\0")];

#[skuld::test]
fn a_control_character_in_a_free_text_field_is_refused_on_every_backend() {
    // A newline ends one systemd directive and starts another; a NUL cuts a string short wherever
    // a backend hands it to the OS as a C string, so `user: "svc\0x"` would name the account `svc`.
    let rows: [(&str, &str); 10] = [
        ("name", r#"name: "a{e}b""#),
        ("command", r#"command: ["/bin/frpc", "a{e}b"]"#),
        ("command", r#"command: ["{e}"]"#),
        ("cwd", r#"cwd: "a{e}b""#),
        ("logs", r#"logs: "a{e}b""#),
        ("env[K]", r#"env: {K: "a{e}b"}"#),
        ("env key", r#"env: {"a{e}b": v}"#),
        ("user.name", r#"user: "svc{e}x""#),
        ("user.name", r#"user: {name: "svc{e}x"}"#),
        ("user.id", r#"user: {id: "S-1-5-{e}"}"#),
    ];
    for (c, escape) in CONTROL_CHARACTERS {
        for (field, row_line) in rows {
            let predicate = format!("field `{field}` contains a control character");
            let line = row_line.replace("{e}", escape);
            // A `command` row replaces the base command, so the base omits it.
            let base = if line.starts_with("command:") {
                "id: frpc\n"
            } else {
                "id: frpc\ncommand: [/bin/frpc]\n"
            };
            let mut manifests = vec![(None, format!("{base}{line}\n"))];
            for backend in BackendKind::ALL {
                let yaml = format!("{base}backend-specific:\n  {backend}:\n    {line}\n");
                manifests.push((Some(backend), yaml));
            }
            for (overridden, yaml) in &manifests {
                for &native in HOST_BACKENDS {
                    let context = format!("{c:?} as native `{native}`: {yaml}");
                    let err = resolve_yaml_as(native, yaml).expect_err(&context).to_string();
                    let context = format!("{context}\n{err}");
                    assert!(err.contains("daemon `frpc`"), "{context}");
                    assert!(!err.contains(c), "{context}");
                    match overridden {
                        Some(backend) => {
                            let blamed = format!("backend-specific.{backend}: {predicate}");
                            assert!(err.contains(&blamed), "{context}");
                        }
                        None => {
                            assert!(err.contains(&predicate), "{context}");
                            assert!(!err.contains("backend-specific"), "{context}");
                        }
                    }
                }
            }
        }
    }
}

#[skuld::test]
fn a_control_character_in_the_id_is_refused() {
    for (c, escape) in CONTROL_CHARACTERS {
        let yaml = format!("id: \"fr{escape}pc\"\ncommand: [/bin/frpc]\n");
        assert!(parse_manifest(&yaml).id.contains(c), "the fixture carries {c:?}");
        // The id is checked before any backend is consulted.
        for &native in HOST_BACKENDS {
            let context = format!("{c:?} as native `{native}`");
            let err = resolve_yaml_as(native, &yaml).expect_err(&context).to_string();
            // Like every other free-text field, the refusal names the defect and omits the value.
            assert!(err.contains("field `id` contains a control character"), "{context}: {err}");
            assert!(!err.contains(c), "{context}: {err:?}");
        }
    }
}

#[skuld::test]
fn a_nul_that_arrives_through_env_substitution_is_refused_before_any_backend_verdict(#[fixture(temp_dir)] dir: &Path) {
    // `.env` is read after the manifest, so the step-5 gate is the only one that sees this value.
    let predicate = "field `user.name` contains a control character";
    let base = "id: frpc\ncommand: [/bin/frpc]\nuser: ${U}\n";
    write_fixture(dir, base, Some("U=svc\0x\n"));
    for &native in HOST_BACKENDS {
        let msg = resolve_as(parse_manifest(base), dir, Some(native))
            .expect_err("a NUL in a substituted value is refused")
            .to_string();
        assert!(msg.contains("after substituting from .env"), "native `{native}`: {msg}");
        assert!(msg.contains(predicate), "native `{native}`: {msg}");
        assert!(!msg.contains('\0'), "native `{native}`: {msg:?}");
        assert!(
            !msg.contains("backend-specific"),
            "no override wrote it, native `{native}`: {msg}"
        );

        let overridden = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    user: ${{U}}\n");
        let msg = resolve_as(parse_manifest(&overridden), dir, Some(native))
            .expect_err("a NUL substituted into the native override is refused")
            .to_string();
        let blamed = format!("backend-specific.{native}: after substituting from .env: {predicate}");
        assert!(msg.contains(&blamed), "native `{native}`: {msg}");
        assert!(!msg.contains('\0'), "native `{native}`: {msg:?}");
    }
}

#[skuld::test]
fn a_control_character_is_refused_when_building_a_daemon_spec_directly() {
    // The library's other way in (the blob decode path): a caller who builds `DaemonFields`
    // without a manifest.
    type Set = fn(&mut DaemonFields, String);
    let rows: [(&str, Set); 8] = [
        ("name", |f, v| f.name = v),
        ("command", |f, v| f.command.push(v)),
        ("cwd", |f, v| f.cwd = Some(abs(&v))),
        ("logs", |f, v| f.logs = Some(abs(&v))),
        ("env[K]", |f, v| {
            f.env.insert("K".to_string(), v);
        }),
        ("env key", |f, v| {
            f.env.insert(v, "x".to_string());
        }),
        ("user.name", |f, v| f.user = User::Name(v)),
        ("user.id", |f, v| f.user = User::Id(AccountId::Sid(v))),
    ];
    for (c, _) in CONTROL_CHARACTERS {
        for (field, set) in &rows {
            let mut fields = plain_fields("frpc", vec![program("frpc")]);
            set(&mut fields, format!("a{c}b"));
            let err = DaemonSpec::new(fields).expect_err(field).to_string();
            let expected = format!("field `{field}` contains a control character");
            assert!(err.contains(&expected), "{c:?} in {field}: {err}");
        }
    }
}

// The rules check what the user wrote ---------------------------------------------------------------------------------

#[skuld::test]
fn accepts_a_relative_cwd_and_logs_ending_in_a_backslash_on_every_host() {
    // Whether a backend can write the path is that backend's question, not the field's.
    for field in ["cwd", "logs"] {
        let yaml = format!("id: frpc\ncommand: [bin/frpc]\n{field}: 'data\\'\n");
        resolve_yaml(&yaml).unwrap_or_else(|e| panic!("`{field}` ending in a backslash: {e}"));
    }
}

#[cfg(windows)]
#[skuld::test]
fn a_drive_relative_path_with_nul_gets_the_control_character_message() {
    // `std::path::absolute` fails on the NUL and would print it; the check on the authored text
    // comes first.
    let yaml = r#"
id: frpc
command: [bin/frpc]
cwd: "C:x\0y"
"#;
    let msg = resolve_yaml(yaml).unwrap_err().to_string();
    assert!(msg.contains("field `cwd` contains a control character"), "{msg}");
    assert!(!msg.contains('\0'), "{msg:?}");
}

#[skuld::test]
fn a_manifest_with_two_defects_reports_the_earlier_field_first() {
    let cases = [
        ("name: \"a\\nb\"\ncwd: \"\"", "field `name`"),
        ("cwd: \"\"\nlogs: \"a\\nb\"", "field `cwd` must not be empty"),
        ("cwd: \"a\\nb\"\nlogs: \"c\\nd\"", "field `cwd`"),
        ("cwd: \"a\\nb\"\nlogs: \"\"", "field `cwd` contains"),
        ("logs: \"a\\nb\"\nenv: {\"A=B\": x}", "field `logs`"),
        ("env: {A: \"a\\nb\"}\nuser: {name: \" \"}", "env[A]"),
    ];
    for (extra, expected) in cases {
        let msg = resolve_yaml(&format!("id: frpc\ncommand: [bin/frpc]\n{extra}\n"))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains(expected),
            "`{extra}`: expected `{expected}` first, got: {msg}"
        );
    }
}

#[cfg(unix)]
#[skuld::test]
fn a_manifest_directory_that_is_not_valid_utf8_is_refused_by_the_field_joined_to_it() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = PathBuf::from(OsStr::from_bytes(b"/base/\xff"));
    let msg = resolve(parse_manifest("id: frpc\ncommand: [bin/frpc]\n"), &dir)
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("field `command` is `bin/frpc`, which resolves under the manifest directory to"),
        "{msg}"
    );
    assert!(msg.contains("is not valid UTF-8"), "{msg}");
}

#[cfg(unix)]
#[skuld::test]
fn a_non_utf8_manifest_directory_is_fine_when_every_path_is_absolute() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = PathBuf::from(OsStr::from_bytes(b"/srv/caf\xe9/"));
    let yaml = "id: frpc\ncommand: [/opt/frpc]\ncwd: /opt\nlogs: /var/log/frpc.log\n";
    let (spec, _) = resolve(parse_manifest(yaml), &dir).expect("no field is joined to the directory");
    assert_eq!(spec.cwd, Some(PathBuf::from("/opt")));
}

#[skuld::test]
fn a_manifest_directory_with_a_newline_is_refused_by_the_field_joined_to_it() {
    let dir = base_dir().join("a\nb");
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    for (extra, field) in [
        ("command: [bin/frpc]".to_string(), "command"),
        (format!("command: ['{abs}']\ncwd: data"), "cwd"),
        (format!("command: ['{abs}']\nlogs: data"), "logs"),
    ] {
        let msg = resolve(parse_manifest(&format!("id: frpc\n{extra}\n")), &dir)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains(&format!("field `{field}` is ")) && msg.contains("resolves under the manifest directory to"),
            "{msg}"
        );
        assert!(msg.contains("control character"), "{msg}");
    }
}

#[skuld::test]
fn a_resolution_error_is_reported_in_field_order() {
    // The directory is bad, so each relative path fails to resolve.
    let dir = base_dir().join("a\nb");
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    let cases = [
        (
            "command: [bin/frpc]\nlogs: data\nenv: {\"A=B\": x}".to_string(),
            "command",
        ),
        (format!("command: ['{abs}']\ncwd: data\nenv: {{\"A=B\": x}}"), "cwd"),
        (format!("command: ['{abs}']\ncwd: data\nlogs: \"a\\nb\""), "cwd"),
    ];
    for (body, field) in cases {
        let msg = resolve(parse_manifest(&format!("id: frpc\n{body}\n")), &dir)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains(&format!("field `{field}` is ")) && msg.contains("resolves under the manifest directory to"),
            "`{body}`: {msg}"
        );
    }
}

#[skuld::test]
fn a_rule_error_is_reported_before_a_later_fields_resolution_error() {
    let dir = base_dir().join("a\nb");
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    let msg = resolve(
        parse_manifest(&format!("id: frpc\ncommand: ['{abs}']\ncwd: \"c\\nd\"\nlogs: data\n")),
        &dir,
    )
    .unwrap_err()
    .to_string();
    assert!(msg.contains("field `cwd` contains a control character"), "{msg}");
}

#[skuld::test]
fn a_bad_id_is_reported_first_whatever_the_manifest_directory() {
    // The id is checked before any path is resolved, so the directory's defect never answers first.
    let dir = base_dir().join("a\nb");
    for (id, expected) in [
        ("${X}", "a daemon id cannot be interpolated"),
        ("\"a b\"", "does not match the required pattern"),
        ("\"a\\nb\"", "field `id` contains a control character"),
    ] {
        let msg = resolve(parse_manifest(&format!("id: {id}\ncommand: [bin/frpc]\n")), &dir)
            .unwrap_err()
            .to_string();
        assert!(msg.contains(expected), "id {id}: {msg}");
        assert!(!msg.contains("manifest directory"), "id {id}: {msg}");
    }
}

#[skuld::test]
fn a_resolved_path_the_field_rules_would_refuse_is_attributed_to_the_resolution() {
    // A drive-relative `cwd: C:` resolves through the per-drive directory to a value the authored
    // rules never saw; the message must say so rather than blame the text the user wrote.
    let id = Id::try_from("frpc").unwrap();
    let err =
        Error::from(check_resolved(&id, PathField::Cwd, "C:", Path::new("/a\u{FFFE}"), DirJoin::Not).unwrap_err());
    let msg = err.to_string();
    assert!(
        msg.contains("field `cwd` is `C:`, which resolves to `/a\u{FFFE}`, which contains U+FFFE"),
        "{msg}"
    );
    check_resolved(&id, PathField::Cwd, "C:", Path::new("/ordinary"), DirJoin::Not).expect("an ordinary value passes");
}

#[skuld::test]
fn a_defect_only_the_directory_adds_is_attributed_to_the_directory() {
    let id = Id::try_from("frpc").unwrap();
    let msg = Error::from(
        check_resolved(&id, PathField::Cwd, "bin", Path::new("/a\u{FFFE}/bin"), DirJoin::Whole).unwrap_err(),
    )
    .to_string();
    assert!(
        msg.contains("field `cwd` is `bin`, which resolves under the manifest directory to `/a\u{FFFE}/bin`, which contains U+FFFE"),
        "{msg}"
    );
}

#[skuld::test]
fn a_directory_defect_is_not_attributed_to_env_when_env_supplied_the_field() {
    let id = Id::try_from("frpc").unwrap();
    let from_env = BTreeSet::from([Field::Cwd]);
    let directory =
        check_resolved(&id, PathField::Cwd, "bin", Path::new("/a\u{FFFE}/bin"), DirJoin::Whole).unwrap_err();
    let msg = directory.attributed(&from_env).to_string();
    assert!(!msg.contains(".env") && !msg.contains("substituting"), "{msg}");

    let value = check_resolved(&id, PathField::Cwd, "C:", Path::new("/a\u{FFFE}"), DirJoin::Not).unwrap_err();
    let msg = value.attributed(&from_env).to_string();
    assert!(msg.contains("after substituting from .env: "), "{msg}");
}

/// A drive-relative `Q:` resolves through the per-drive directory, which no manifest can set, so the
/// resolution is named and `.env` is blamed only when a reference supplied the `Q:`.
#[skuld::test]
fn a_resolved_drive_relative_defect_names_the_resolution_and_blames_env_only_when_env_wrote_it() {
    let id = Id::try_from("frpc").unwrap();
    let resolved = Path::new("/a\u{7}");
    let refused = || check_resolved(&id, PathField::Cwd, "Q:", resolved, DirJoin::Not).unwrap_err();

    let typed = refused().attributed(&BTreeSet::new()).to_string();
    assert!(
        typed.contains("field `cwd` is `Q:`, which resolves to `/a\u{7}`"),
        "{typed}"
    );
    assert!(!typed.contains(".env") && !typed.contains("substituting"), "{typed}");

    let from_env = refused().attributed(&BTreeSet::from([Field::Cwd])).to_string();
    assert!(
        from_env.contains("after substituting from .env: field `cwd` is `Q:`, which resolves to `/a\u{7}`"),
        "{from_env}"
    );
}

/// A drive root is a legitimate working directory: nothing refuses the backslash it ends in,
/// written out or reached through the manifest directory.
#[cfg(windows)]
#[skuld::test]
fn a_drive_root_cwd_resolves() {
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    let written = format!("id: frpc\ncommand: ['{abs}']\ncwd: 'C:\\'\n");
    let (spec, _) = resolve(parse_manifest(&written), &base_dir()).expect("`C:\\` resolves");
    assert_eq!(spec.cwd, Some(PathBuf::from(r"C:\")));

    let dotted = "id: frpc\ncommand: [bin/frpc.exe]\ncwd: .\n";
    let (spec, _) = resolve(parse_manifest(dotted), Path::new(r"C:\")).expect("`.` in `C:\\` resolves");
    assert_eq!(spec.cwd, Some(PathBuf::from(r"C:\")));
}

// A substituted path is judged as it is -------------------------------------------------------------------------------

/// A manifest directory whose name is not valid UTF-8.
fn non_utf8_dir() -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(b"/base/\xff"))
    }
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let mut name: Vec<u16> = r"C:\base\".encode_utf16().collect();
        name.push(0xD800);
        PathBuf::from(OsString::from_wide(&name))
    }
}

/// `$$` is an escape: after step 5 the value is `data/$x`, which holds a `$` and no reference.
#[skuld::test]
fn a_path_with_an_escaped_dollar_is_judged_against_the_manifest_directory() {
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    for dir in [non_utf8_dir(), base_dir().join("a\nb")] {
        let msg = resolve(
            parse_manifest(&format!("id: frpc\ncommand: ['{abs}']\ncwd: 'data/$$x'\n")),
            &dir,
        )
        .unwrap_err()
        .to_string();
        assert!(
            msg.contains("field `cwd` is `data/$x`, which resolves under the manifest directory to "),
            "{dir:?}: {msg}"
        );
    }
}

#[skuld::test]
fn a_manifest_directory_defect_is_not_blamed_on_the_env_file() {
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    let dir = base_dir().join("a\nb");
    let msg = resolve(
        parse_manifest(&format!("id: frpc\ncommand: ['{abs}']\ncwd: 'data/$$x'\n")),
        &dir,
    )
    .unwrap_err()
    .to_string();
    assert!(!msg.contains(".env") && !msg.contains("substituting"), "{msg}");
}

/// `type: simple$$` substitutes to `simple$`, which holds a `$` and no reference: the shape phase
/// judges it, with `command`, rather than leaving it to completeness.
#[skuld::test]
fn a_substituted_enum_with_a_dollar_left_is_judged_in_the_shape_phase() {
    let msg = resolve_yaml(
        "id: frpc
type: 'simple$$'
",
    )
    .unwrap_err()
    .to_string();
    assert!(
        msg.contains("field `type` is `simple$`"),
        "an absent `command` is completeness's to report, after the shape phase: {msg}"
    );
    assert!(!msg.contains(".env"), "`$$` is not `.env`'s: {msg}");
}

// Which defects are `.env`'s ------------------------------------------------------------------------------------------

#[skuld::test]
fn a_defect_in_a_value_a_reference_supplied_is_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\ntype: ${T}\n", Some("T=simple$\n"));
    let msg = load(dir).unwrap_err().to_string();
    assert!(
        msg.contains("after substituting from .env: field `type` is `simple$`"),
        "{msg}"
    );
}

#[skuld::test]
fn a_defect_in_a_field_no_reference_touched_is_not_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    // `name` is substituted; the `type` defect is in a field that holds only a `$$` escape.
    write_fixture(dir, "id: frpc\nname: ${N}\ntype: 'simple$$'\n", Some("N=frpc\n"));
    let msg = load(dir).unwrap_err().to_string();
    assert!(msg.contains("field `type` is `simple$`"), "{msg}");
    assert!(!msg.contains(".env"), "{msg}");
}

#[skuld::test]
fn a_defect_in_a_default_is_not_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\nrestart: ${R:-sometimes}\n", None);
    let msg = load(dir).unwrap_err().to_string();
    assert!(msg.contains("field `restart` is `sometimes`"), "{msg}");
    assert!(!msg.contains(".env"), "{msg}");
}

#[skuld::test]
fn an_empty_env_value_is_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    write_fixture(
        dir,
        &format!("id: frpc\ncommand: ['{abs}']\ncwd: ${{D}}\n"),
        Some("D=\n"),
    );
    let msg = load(dir).unwrap_err().to_string();
    assert!(
        msg.contains("after substituting from .env: field `cwd` must not be empty"),
        "{msg}"
    );
}

#[skuld::test]
fn a_default_that_replaces_an_empty_env_value_is_not_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\nrestart: ${R:-sometimes}\n", Some("R=\n"));
    let msg = load(dir).unwrap_err().to_string();
    assert!(msg.contains("field `restart` is `sometimes`"), "{msg}");
    assert!(!msg.contains(".env"), "{msg}");
}

#[skuld::test]
fn a_defect_in_a_value_that_overrides_a_default_is_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\nrestart: ${R:-never}\n", Some("R=sometimes\n"));
    let msg = load(dir).unwrap_err().to_string();
    assert!(
        msg.contains("after substituting from .env: field `restart` is `sometimes`"),
        "{msg}"
    );
}

/// `.env` supplied the value, whatever it looks like: here it equals the reference that asked for it.
#[skuld::test]
fn a_defect_in_a_value_equal_to_its_reference_is_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\ntype: ${T}\n", Some("T=${T}\n"));
    let msg = load(dir).unwrap_err().to_string();
    assert!(
        msg.contains("after substituting from .env: field `type` is `${T}`"),
        "{msg}"
    );
}

/// The `$` is the manifest's `$$` and the rest is a default: no reference read `.env`.
#[skuld::test]
fn a_defect_in_a_default_followed_by_an_escape_is_not_attributed_to_env(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\ntype: '${T:-simple}$$'\n", None);
    let msg = load(dir).unwrap_err().to_string();
    assert!(msg.contains("field `type` is `simple$`"), "{msg}");
    assert!(!msg.contains(".env"), "{msg}");
}

#[cfg(unix)]
#[skuld::test]
fn a_user_name_from_env_is_attributed_to_the_override_and_to_env(#[fixture(temp_dir)] dir: &Path) {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  systemd:\n    user: ${U}\n";
    write_fixture(dir, yaml, Some("U=LocalService\n"));
    let msg = resolve_as(parse_manifest(yaml), dir, Some(BackendKind::Systemd))
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("backend-specific.systemd: after substituting from .env: "),
        "{msg}"
    );
    assert!(msg.contains("Windows built-in"), "{msg}");
}

#[cfg(unix)]
#[skuld::test]
fn a_user_name_written_out_is_attributed_to_the_override_alone() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  systemd:\n    user: LocalService\n";
    let msg = resolve_as(parse_manifest(yaml), &base_dir(), Some(BackendKind::Systemd))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("backend-specific.systemd"), "{msg}");
    assert!(!msg.contains(".env"), "{msg}");
}

#[skuld::test]
fn a_manifest_directory_defect_under_a_substituted_path_is_attributed_to_the_directory() {
    // The field's text changed (`${D:-bin}` became `bin`), but the defect is the directory's. The
    // directory is not created: no file system need accept its name.
    let dir = base_dir().join("a\u{FFFE}b");
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    let yaml = format!("id: frpc\ncommand: ['{abs}']\ncwd: ${{D:-bin}}\n");
    let msg = resolve(parse_manifest(&yaml), &dir).unwrap_err().to_string();
    assert!(msg.contains("which resolves under the manifest directory to"), "{msg}");
    assert!(!msg.contains(".env") && !msg.contains("substituting"), "{msg}");
}

#[cfg(windows)]
#[skuld::test]
fn a_pending_value_with_a_dollar_raises_nothing() {
    let id = Id::try_from("frpc").unwrap();
    let mut warnings = Vec::new();
    let resolved = resolve_path_string(
        &id,
        PathField::Cwd,
        "C:bin/$x",
        &base_dir(),
        Substitution::Pending,
        None,
        &mut warnings,
    )
    .unwrap();
    assert_eq!(resolved, "C:bin/$x");
    assert!(warnings.is_empty(), "{warnings:?}");
}

// What the directory alone cannot refuse ------------------------------------------------------------------------------

#[skuld::test]
fn a_path_a_substitution_may_make_absolute_is_not_judged_against_the_manifest_directory() {
    // `${D}` is absolute if `.env` says so; the shape phase cannot know, and `DaemonSpec::new`
    // judges the final value.
    let id = Id::try_from("frpc").unwrap();
    let dir = base_dir().join("a\nb");
    let abs = base_dir().join("frpc").to_string_lossy().into_owned();
    for body in [
        "command: ['${D}']".to_string(),
        format!("command: ['{abs}']\ncwd: ${{D}}"),
        format!("command: ['{abs}']\nlogs: ${{D}}"),
    ] {
        let raw = parse_manifest(&format!("id: frpc\n{body}\n"));
        resolve_shape(&id, raw, &dir, Substitution::Pending, None)
            .unwrap_or_else(|e| panic!("`{body}`: {}", Error::from(e)));
    }
}

#[cfg(windows)]
#[skuld::test]
fn a_root_relative_path_uses_only_the_directorys_drive() {
    // `C:\base\a<newline>b` joined with `\data` is `C:\data`: nothing of the directory survives.
    let dir = base_dir().join("a\nb");
    let yaml = "id: frpc\ncommand: ['C:\\bin\\x.exe']\ncwd: '\\data'\n";
    let (spec, _) = resolve(parse_manifest(yaml), &dir).expect("the directory does not appear in the result");
    assert_eq!(spec.cwd, Some(PathBuf::from(r"C:\data")));
}

/// A root-relative path joined with a share directory keeps the share, so the share's defect is the
/// directory's, put there by `-f`, not by `.env`; a defect beside it is the field's.
#[cfg(windows)]
#[skuld::test]
fn a_share_defect_a_root_relative_path_keeps_is_not_blamed_on_env_but_one_beside_it_is() {
    let id = Id::try_from("frpc").unwrap();
    let from_env = BTreeSet::from([Field::Cwd]);
    let refused = |resolved: &str| {
        check_resolved(&id, PathField::Cwd, r"\data", Path::new(resolved), DirJoin::PrefixOnly).unwrap_err()
    };

    let msg = refused("\\\\server\\sh\u{FFFE}re\\data")
        .attributed(&from_env)
        .to_string();
    assert!(msg.contains("the manifest directory's drive or share"), "{msg}");
    assert!(!msg.contains(".env") && !msg.contains("substituting"), "{msg}");

    let msg = refused("C:\\da\u{7}ta").attributed(&from_env).to_string();
    assert!(msg.contains("after substituting from .env: "), "{msg}");

    // End to end. No `$` in the manifest, so `.env` is never read from the share.
    let dir = PathBuf::from("\\\\server\\sh\u{FFFE}re\\app");
    let yaml = "id: frpc\ncommand: ['C:\\bin\\x.exe']\ncwd: '\\data'\n";
    let msg = resolve_as(parse_manifest(yaml), &dir, Some(BackendKind::Scm))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("the manifest directory's drive or share"), "{msg}");
    assert!(msg.contains("U+FFFE"), "{msg}");
}

#[skuld::test]
fn the_directory_is_blamed_only_for_a_defect_it_put_in_the_joined_path() {
    let bang = |p: &Path| p.to_string_lossy().contains('!');
    let blamed = |join, written, joined| directory_is_to_blame(join, Path::new(written), Path::new(joined), bang);

    assert!(blamed(DirJoin::Whole, "bin", "/a!/bin"));
    assert!(!blamed(DirJoin::Whole, "b!in", "/a!/b!in"), "the field wrote it too");
    assert!(!blamed(DirJoin::Whole, "bin", "/a/bin"), "no defect");
    assert!(
        !blamed(DirJoin::Not, "/a!/bin", "/a!/bin"),
        "nothing of the directory is in the result"
    );
    assert!(
        !blamed(DirJoin::PrefixOnly, "/bin", "/a!/bin"),
        "no prefix to hold the defect"
    );
}

#[cfg(windows)]
#[skuld::test]
fn only_a_defect_in_the_prefix_a_root_relative_path_keeps_is_the_directorys() {
    let bang = |p: &Path| p.to_string_lossy().contains('!');
    let blamed =
        |written, joined| directory_is_to_blame(DirJoin::PrefixOnly, Path::new(written), Path::new(joined), bang);

    assert!(blamed(r"\data", r"\\server\sh!re\data"));
    assert!(!blamed(r"\data", r"C:\da!ta"), "the defect is outside the prefix");
    assert!(!blamed(r"\d!ata", r"\\server\sh!re\d!ata"), "the field wrote it too");
}

#[skuld::test]
fn a_manifest_directory_is_joined_whole_to_a_relative_path_only() {
    assert_eq!(dir_join(Path::new("bin/x")), DirJoin::Whole);
    assert_eq!(dir_join(Path::new("./bin")), DirJoin::Whole);
    assert_eq!(dir_join(Path::new("")), DirJoin::Whole);
    assert_eq!(dir_join(&abs("bin")), DirJoin::Not);
    #[cfg(windows)]
    {
        assert_eq!(dir_join(Path::new(r"\data")), DirJoin::PrefixOnly);
        assert_eq!(dir_join(Path::new("/bin/x")), DirJoin::PrefixOnly);
        assert_eq!(dir_join(Path::new("C:bin")), DirJoin::Not);
        assert_eq!(dir_join(Path::new(r"\\s\h\x")), DirJoin::Not);
    }
}

#[cfg(windows)]
#[skuld::test]
fn a_drive_root_manifest_directory_joins_cleanly() {
    let (spec, _) = resolve(parse_manifest("id: frpc\ncommand: [bin/frpc.exe]\n"), Path::new(r"C:\"))
        .expect("`C:\\` joined with `bin` is `C:\\bin`");
    assert_eq!(spec.command[0], r"C:\bin\frpc.exe");
}

// Contract violations =================================================================================================

#[skuld::test]
fn a_contract_violation_names_the_daemon_once() {
    let id = Id::try_from("frpc").unwrap();
    let cause = DaemonSpec::new(DaemonFields {
        name: "a\nb".to_string(),
        ..plain_fields("frpc", vec![program("frpc")])
    })
    .expect_err("a newline in `name` is refused");

    let printed = contract_violation_from(&id, "the resolved daemon failed its own field rules", cause)
        .report()
        .to_string();

    assert_eq!(printed.matches("daemon `frpc`").count(), 1, "{printed}");
    assert!(printed.contains("internal error"), "{printed}");
    assert!(printed.contains("field `name`"), "the cause's field is kept: {printed}");
}

#[skuld::test]
fn a_contract_violation_from_a_non_rule_error_keeps_it_as_the_source() {
    let id = Id::try_from("frpc").unwrap();
    let cause = Error::os("read a file", std::io::Error::other("gone"));

    let err = contract_violation_from(&id, "the resolved daemon failed its own field rules", cause);

    let Error::Invalid {
        daemon,
        message,
        source,
    } = &err
    else {
        panic!("a contract violation is `Invalid`: {err:?}");
    };
    assert_eq!(daemon, "frpc");
    assert_eq!(
        message,
        "internal error: the resolved daemon failed its own field rules; this is a bug in goetia"
    );
    assert!(source.is_some(), "the cause is kept as the source");
}

#[cfg(unix)]
#[skuld::test]
fn a_manifest_directory_ending_in_a_backslash_is_accepted() {
    let dir = PathBuf::from("/base/a\\");
    // Not as a systemd host: it refuses a backslash in the executable path.
    let yaml = "id: frpc\ncommand: [bin/frpc]\ncwd: .\n";
    let (spec, _) = resolve_as(parse_manifest(yaml), &dir, Some(BackendKind::Launchd)).expect("resolves");
    assert_eq!(spec.cwd, Some(dir));
}

// Resolution ==========================================================================================================

#[skuld::test]
fn resolve_makes_paths_absolute_against_base_dir() {
    let yaml = r#"
id: frpc
command: ["bin/frpc.exe", "-c", "frpc.toml"]
cwd: "."
logs: "logs/frpc.log"
"#;
    let (spec, _warnings) = resolve_yaml(yaml).expect("valid manifest should resolve");

    let expected_command0 = base_dir().join("bin").join("frpc.exe").to_string_lossy().into_owned();
    assert_eq!(spec.command[0], expected_command0);
    assert_eq!(spec.command[1], "-c", "later argv entries are untouched");
    // `cwd: .` normalizes to the base dir itself, not `<base>/.`.
    assert_eq!(spec.cwd, Some(base_dir()));
    assert_eq!(spec.logs, Some(base_dir().join("logs").join("frpc.log")));
}

#[skuld::test]
fn resolve_leaves_an_already_absolute_command_alone() {
    let absolute = base_dir().join("bin/frpc.exe").to_string_lossy().into_owned();
    // Single-quoted, not double-quoted: a Windows absolute path's `\`
    // would otherwise be read as a YAML double-quote escape introducer
    // (`\b` is backspace) rather than a literal backslash.
    let yaml = format!("id: frpc\ncommand: ['{absolute}']\n");
    let (spec, _warnings) = resolve_yaml(&yaml).expect("valid manifest should resolve");
    assert_eq!(spec.command[0], absolute);
}

#[skuld::test]
fn resolve_defaults_name_to_id() {
    let (spec, _) = resolve_yaml("id: frpc\ncommand: [bin/frpc]\n").unwrap();
    assert_eq!(spec.name, "frpc");
}

#[skuld::test]
fn resolve_defaults_user_to_root() {
    let (spec, _) = resolve_yaml("id: frpc\ncommand: [bin/frpc]\n").unwrap();
    assert_eq!(spec.user, User::Root);
}

#[skuld::test]
fn resolve_defaults_kind_to_simple() {
    let (spec, _) = resolve_yaml("id: frpc\ncommand: [bin/frpc]\n").unwrap();
    assert_eq!(spec.kind, Kind::Simple);
}

#[skuld::test]
fn resolve_defaults_restart_to_never() {
    let (spec, _) = resolve_yaml("id: frpc\ncommand: [bin/frpc]\n").unwrap();
    assert_eq!(spec.restart, Restart::Never);
}

#[skuld::test]
fn resolve_defaults_restart_delay_to_none() {
    let (spec, _) = resolve_yaml("id: frpc\ncommand: [bin/frpc]\n").unwrap();
    assert_eq!(spec.restart_delay, None);
}

#[skuld::test]
fn resolve_rejects_empty_command() {
    let err = resolve_yaml("id: frpc\ncommand: []\n").unwrap_err();
    assert!(err.to_string().contains("command"));
}

// Field parsing (restart, type, restart-delay) ========================================================================

#[skuld::test]
fn resolve_parses_each_restart_policy() {
    for (raw, expected) in [
        ("never", Restart::Never),
        ("on-failure", Restart::OnFailure),
        ("always", Restart::Always),
    ] {
        let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nrestart: {raw}\n");
        let (spec, _) = resolve_yaml(&yaml).unwrap_or_else(|e| panic!("`restart: {raw}` should resolve: {e}"));
        assert_eq!(spec.restart, expected, "restart: {raw}");
    }
}

#[skuld::test]
fn resolve_parses_each_type() {
    for (raw, expected) in [("simple", Kind::Simple), ("managed", Kind::Managed)] {
        let yaml = format!("id: frpc\ncommand: [bin/frpc]\ntype: {raw}\n");
        let (spec, _) = resolve_yaml(&yaml).unwrap_or_else(|e| panic!("`type: {raw}` should resolve: {e}"));
        assert_eq!(spec.kind, expected, "type: {raw}");
    }
}

#[skuld::test]
fn an_unknown_restart_policy_names_the_daemon_and_the_valid_values() {
    let yaml = "id: frpc\ncommand: [bin/frpc]\nrestart: sometimes\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemon `frpc`: field `restart` is `sometimes`; expected one of `never`, `on-failure`, `always`"
    );
}

#[skuld::test]
fn an_unknown_type_names_the_daemon_and_the_valid_values() {
    let yaml = "id: frpc\ncommand: [bin/frpc]\ntype: complicated\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemon `frpc`: field `type` is `complicated`; expected one of `simple`, `managed`"
    );
}

#[skuld::test]
fn a_malformed_restart_delay_names_the_daemon_and_shows_an_example() {
    let yaml = "id: frpc\ncommand: [bin/frpc]\nrestart-delay: not-a-duration\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.report().to_string(),
        "daemon `frpc`: field `restart-delay` is `not-a-duration`, which is not a duration \
         (e.g. `30s`, `1m 30s`): expected number at 0"
    );
    assert!(
        std::error::Error::source(&err).is_some(),
        "the parser's error is the source"
    );
}

// Warnings ============================================================================================================

#[cfg(windows)]
#[skuld::test]
fn managed_on_windows_warns_for_cwd_and_logs() {
    let yaml = r#"
id: svc
command: ["bin/svc.exe"]
cwd: "."
logs: "logs/svc.log"
type: managed
"#;
    let (spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("accepted with a warning, not rejected");
    assert_eq!(spec.kind, Kind::Managed);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].id.as_ref().map(Id::as_str), Some("svc"));
    assert!(warnings[0].message.contains("cwd") || warnings[0].message.contains("logs"));
}

#[cfg(windows)]
#[skuld::test]
fn managed_on_windows_warning_names_the_argument_consequence() {
    let yaml = r#"
id: svc
command: ["bin/svc.exe"]
cwd: "."
logs: "logs/svc.log"
type: managed
"#;
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("accepted with a warning, not rejected");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("System32"));
    assert!(warnings[0].message.contains("argument"));
}

#[cfg(windows)]
#[skuld::test]
fn simple_on_windows_does_not_warn_for_cwd_and_logs() {
    let yaml = r#"
id: svc
command: ["bin/svc.exe"]
cwd: "."
logs: "logs/svc.log"
type: simple
"#;
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).unwrap();
    assert!(warnings.is_empty());
}

// `restart: always` ===================================================================================================
//
// Resolution accepts it everywhere: whether a backend can honour it is the generator's verdict, at install and
// preview (`backend::scm::generate::registration`).

#[skuld::test]
fn an_scm_override_of_on_failure_applies_only_where_scm_is_native() {
    let yaml = r#"
id: svc
command: [/bin/svc.exe]
restart: always
backend-specific:
  scm: { restart: on-failure }
"#;
    for &native in HOST_BACKENDS {
        let (spec, warnings) = resolve_yaml_as(native, yaml).expect("resolves");
        let expected = if native == BackendKind::Scm {
            Restart::OnFailure
        } else {
            Restart::Always
        };
        assert_eq!(spec.restart, expected, "native {native}");
        assert!(warnings.is_empty(), "native {native}: {warnings:?}");
    }
}

#[skuld::test]
fn an_scm_override_that_writes_always_resolves_on_each_host_backend() {
    let yaml = r#"
id: svc
command: [/bin/svc.exe]
backend-specific:
  scm: { restart: always }
"#;
    for &native in HOST_BACKENDS {
        let (spec, _warnings) = resolve_yaml_as(native, yaml).unwrap_or_else(|e| panic!("native {native}: {e}"));
        let expected = if native == BackendKind::Scm {
            Restart::Always
        } else {
            Restart::Never
        };
        assert_eq!(spec.restart, expected, "native {native}");
    }
}

#[skuld::test]
fn a_templated_always_resolves_on_each_host_backend(#[fixture(temp_dir)] dir: &Path) {
    let yaml = r#"
id: svc
command: [/bin/svc]
restart: ${R}
"#;
    write_fixture(dir, yaml, Some("R=always\n"));
    for &native in HOST_BACKENDS {
        let (spec, _warnings) =
            resolve_as(parse_manifest(yaml), dir, Some(native)).unwrap_or_else(|e| panic!("native {native}: {e}"));
        assert_eq!(spec.restart, Restart::Always, "native {native}");
    }
}

#[cfg(unix)]
#[skuld::test]
fn sub_second_restart_delay_rounds_up_and_warns() {
    let yaml = r#"
id: frpc
command: ["bin/frpc"]
restart-delay: "1.5s"
"#;
    let (spec, warnings) = resolve_yaml_as(BackendKind::Launchd, yaml).expect("accepted with a warning, not rejected");

    // The blob keeps the authored value unrounded, so generation stays
    // deterministic; only `backend::launchd::generate` rounds up.
    assert_eq!(spec.restart_delay, Some(Duration::from_millis(1500)));
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].message.contains('2'),
        "warning should name the rounded value: {}",
        warnings[0].message
    );
}

#[cfg(unix)]
#[skuld::test]
fn whole_second_restart_delay_does_not_warn() {
    let yaml = r#"
id: frpc
command: ["bin/frpc"]
restart-delay: "2s"
"#;
    let (spec, warnings) = resolve_yaml_as(BackendKind::Launchd, yaml).unwrap();
    assert_eq!(spec.restart_delay, Some(Duration::from_secs(2)));
    assert!(warnings.is_empty());
}

/// The warning above runs on a `Duration` parsed by `parse_restart_delay`, not one
/// `humantime_serde` produced during deserialization: pin the threshold.
#[cfg(unix)]
#[skuld::test]
fn a_sub_second_restart_delay_still_warns() {
    let yaml = "id: frpc\ncommand: [bin/frpc]\nrestart-delay: 500ms\n";
    let (spec, warnings) = resolve_yaml_as(BackendKind::Launchd, yaml).expect("accepted with a warning, not rejected");
    assert_eq!(spec.restart_delay, Some(Duration::from_millis(500)));
    assert_eq!(warnings.len(), 1);
    // Not just "some warning fired": pin that it is *this* warning, not an
    // unrelated one (e.g. a Windows-divergence warning) that happens to be
    // the only one present.
    assert!(
        warnings[0].message.contains("not a whole number of seconds") && warnings[0].message.contains("500ms"),
        "warning should identify the sub-second restart-delay: {}",
        warnings[0].message
    );
}

#[cfg(unix)]
#[skuld::test]
fn a_whole_second_restart_delay_still_does_not_warn() {
    let yaml = "id: frpc\ncommand: [bin/frpc]\nrestart-delay: 3s\n";
    let (spec, warnings) = resolve_yaml_as(BackendKind::Launchd, yaml).unwrap();
    assert_eq!(spec.restart_delay, Some(Duration::from_secs(3)));
    assert!(warnings.is_empty());
}

// load() ==============================================================================================================

#[skuld::test]
fn load_reads_a_file_path(#[fixture(temp_dir)] dir: &Path) {
    let manifest_path = dir.join("goetia.yaml");
    std::fs::write(&manifest_path, "id: frpc\ncommand: [bin/frpc]\n").unwrap();

    let (spec, _warnings) = load(&manifest_path).expect("file path should load");
    assert_eq!(spec.id.as_str(), "frpc");
    // Relative paths resolve against the manifest's own directory, not the
    // process's current directory.
    let expected = dir.join("bin").join("frpc").to_string_lossy().into_owned();
    assert_eq!(spec.command[0], expected);
}

#[skuld::test]
fn load_reads_a_directory_containing_goetia_yaml(#[fixture(temp_dir)] dir: &Path) {
    std::fs::write(dir.join("goetia.yaml"), "id: frpc\ncommand: [bin/frpc]\n").unwrap();

    let (spec, _warnings) = load(dir).expect("directory should load");
    assert_eq!(spec.id.as_str(), "frpc");
}

#[skuld::test]
fn load_reports_io_error_for_missing_path(#[fixture(temp_dir)] dir: &Path) {
    let missing = dir.join("does-not-exist.yaml");

    let err = load(&missing).unwrap_err();
    assert!(matches!(err, Error::Os { .. }), "expected an Os error, got: {err:?}");
}

// Emission hardening ==================================================================================================

/// A manifest whose `name` is `value`, resolved. Drives the validation gate
/// from one field without repeating the YAML in every test.
fn resolve_with_name(value: &str) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    let yaml = format!("id: frpc\nname: {value:?}\ncommand: [/bin/frpc]\n");
    resolve_yaml(&yaml)
}

/// The same for an env value, which reaches `Environment=` on systemd and
/// `EnvironmentVariables` on launchd.
fn resolve_with_env_value(value: &str) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nenv:\n  FOO: {value:?}\n");
    resolve_yaml(&yaml)
}

#[skuld::test]
fn accepts_trailing_backslashes_in_name() {
    // Keeping a line-ending backslash from continuing the line is the systemd generator's job.
    for name in ["FRP client\\", "FRP client\\\\", "FRP client\\\\\\"] {
        let (spec, _) = resolve_with_name(name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        assert_eq!(spec.name, name);
    }
}

#[skuld::test]
fn accepts_trailing_backslashes_in_env_value() {
    for value in ["C:\\opt\\rt\\", "C:\\opt\\rt\\\\"] {
        let (spec, _) = resolve_with_env_value(value).unwrap_or_else(|e| panic!("{value:?}: {e}"));
        assert_eq!(spec.env["FOO"], value);
    }
}

#[skuld::test]
fn accepts_interior_backslashes() {
    let (spec, _) = resolve_with_env_value("C:\\opt\\rt").expect("interior backslashes are fine");
    assert_eq!(spec.env["FOO"], "C:\\opt\\rt");
}

#[skuld::test]
fn rejects_xml_noncharacters() {
    // U+FFFE/U+FFFF are not control characters, so `char::is_control()` lets
    // them through — but XML 1.0 cannot represent a noncharacter at all, not
    // even as a numeric entity, so one reaching the launchd generator yields
    // an unparseable plist and a daemon that refuses to load.
    //
    // Driven straight at the gate rather than through YAML: serde's debug
    // form of these code points is `\u{fffe}`, which YAML rejects (it wants
    // four bare hex digits), so a round-trip would fail for the wrong reason.
    let id = Id::try_from("frpc").expect("valid id");
    for bad in ['\u{FFFE}', '\u{FFFF}', '\u{FDD0}', '\u{1FFFE}'] {
        let value = format!("frp{bad}");
        let err = reject_control_chars(&id, "name", &value).expect_err("noncharacter must be rejected");
        assert!(
            err.to_string().contains("noncharacter"),
            "message should name the cause for U+{:04X}: {err}",
            bad as u32
        );
    }
}

#[skuld::test]
fn accepts_ordinary_text() {
    // Guards the noncharacter check against over-rejecting: it must not
    // catch ordinary non-ASCII, which is legitimate in a display name.
    let id = Id::try_from("frpc").expect("valid id");
    reject_control_chars(&id, "name", "FRP — клиент 日本語").expect("ordinary text is fine");
}

#[cfg(unix)]
#[skuld::test]
fn huge_restart_delay_saturates_instead_of_overflowing() {
    // `as_secs() + 1` on a near-`Duration::MAX` value panics in debug and
    // wraps to a false "rounds to 0s" warning in release. Assert the
    // saturated outcome, not merely the absence of a panic: a wrap is
    // silent in release, and a debug panic is the only half a
    // result-discarding test could ever catch.
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nrestart-delay: {}s 1ns\n", u64::MAX);
    let (spec, warnings) =
        resolve_yaml_as(BackendKind::Launchd, &yaml).expect("a near-`Duration::MAX` delay is accepted, with a warning");

    assert_eq!(spec.restart_delay, Some(Duration::new(u64::MAX, 1)));
    let [warning] = warnings.as_slice() else {
        panic!("expected exactly one warning, got {warnings:?}");
    };
    assert!(
        warning.message.contains(&format!("round it up to {}s", u64::MAX)),
        "the warning must name the saturated value, not a wrapped one: {}",
        warning.message
    );
}

/// The canonicalised, verbatim-prefix-stripped process working directory —
/// the same helper `absolutize` itself uses to turn a relative `-f` into an
/// absolute `base_dir`.
fn canonical_cwd() -> PathBuf {
    strip_verbatim_prefix(std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("cwd canonicalizes"))
}

#[skuld::test]
fn relative_base_dir_still_yields_absolute_paths() {
    // Same invariant as `resolve`'s absolutize step: every emitted path must
    // come out absolute even when `base_dir` (e.g. `-f .`) is not.
    let yaml = "id: frpc\ncommand: [bin/frpc]\ncwd: .\nlogs: logs/frpc.log\n";
    let (spec, _) = resolve(parse_manifest(yaml), Path::new(".")).expect("resolves");

    // Assert *which* directory was used, not merely that some absolute one
    // was: an `absolutize` that ignored its argument and returned a constant
    // would still satisfy `is_absolute()` on every field. The working
    // directory is canonicalised (see `canonicalize_cwd`), so this compares
    // against the same canonical, prefix-stripped form `absolutize` itself
    // produces rather than the raw `current_dir()` value.
    let cwd = canonical_cwd();
    assert_eq!(spec.command[0], cwd.join("bin").join("frpc").to_string_lossy());
    assert_eq!(spec.cwd.as_deref(), Some(cwd.as_path()));
    assert_eq!(spec.logs, Some(cwd.join("logs").join("frpc.log")));
}

#[skuld::test]
fn a_relative_manifest_directory_resolves_against_the_canonical_working_directory() {
    // Runs everywhere; on Unix it is close to a no-op, which is the point —
    // it pins that the canonicalisation did not change Unix behaviour.
    let mut warnings = Vec::new();
    let resolved = absolutize(Path::new("sub"), &mut warnings).expect("resolves");
    assert_eq!(resolved, canonical_cwd().join("sub"));
}

#[skuld::test]
fn the_leading_parent_dirs_of_a_relative_manifest_directory_collapse_against_the_working_directory() {
    let cwd = canonical_cwd();
    let mut warnings = Vec::new();
    let one = absolutize(Path::new("../app"), &mut warnings).expect("resolves");
    assert_eq!(one, cwd.parent().expect("the test runs below a root").join("app"));

    // One component of the working directory per `..`, `.` between them ignored.
    let two = absolutize(Path::new("../.././app"), &mut warnings).expect("resolves");
    let grand = cwd
        .parent()
        .and_then(Path::parent)
        .expect("the test runs two below a root");
    assert_eq!(two, grand.join("app"));

    let bare = absolutize(Path::new(".."), &mut warnings).expect("resolves");
    assert_eq!(bare, cwd.parent().expect("parent"));
}

#[skuld::test]
fn a_parent_dir_after_a_named_component_of_a_manifest_directory_is_kept() {
    let mut warnings = Vec::new();
    let resolved = absolutize(Path::new("a/../b"), &mut warnings).expect("resolves");
    assert_eq!(resolved, canonical_cwd().join("a").join("..").join("b"));
}

#[skuld::test]
fn an_authored_base_dir_is_not_canonicalised(#[fixture(temp_dir)] dir: &Path) {
    // The guard on the deduced/authored line: an absolute `-f` pointing
    // through a symlinked directory must resolve to the path as written,
    // not to the link target. No fallback, no conditional assertion — if
    // the symlink cannot be created, this must fail loudly rather than
    // quietly assert something else.
    let real = dir.join("real");
    std::fs::create_dir(&real).expect("real dir should be creatable");
    let link = dir.join("link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).expect("symlink should be creatable");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&real, &link).expect("symlink should be creatable");

    let mut warnings = Vec::new();
    let resolved = absolutize(&link, &mut warnings).expect("resolves");
    assert_eq!(
        resolved, link,
        "an authored base_dir must be honoured as written, not resolved through its symlink"
    );
    assert_ne!(
        resolved, real,
        "resolving through the symlink would defeat the point of this test"
    );
}

#[skuld::test]
fn normalize_preserves_parent_dir_components() {
    // `.` is dropped but `..` is deliberately kept: collapsing `..`
    // lexically is wrong when a component is a symlink, since `a/b/..` is
    // only `a` if `b` is a real directory. Pin the distinction so a future
    // "completion" of the match arm cannot quietly change it. The one
    // deliberate exception to this rule lives in
    // `drive_relative_resolution_collapses_dot_dot_the_windows_way` below.
    let yaml = "id: frpc\ncommand: [bin/frpc]\ncwd: ../sibling\n";
    // No backend named: systemd refuses a `..`, and the property is not systemd's.
    let (spec, _) = resolve_as(parse_manifest(yaml), &base_dir(), None).expect("resolves");
    let cwd = spec.cwd.as_deref().expect("cwd is set");

    assert!(
        cwd.components().any(|c| matches!(c, std::path::Component::ParentDir)),
        "`..` must survive normalization, got {cwd:?}"
    );
}

// The emptiness gate ==================================================================================================

#[skuld::test]
fn rejects_an_empty_user_name() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nuser: \"\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("user.name"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_user_id() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nuser:\n  id: \"\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("user.id"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_a_whitespace_only_user_name() {
    // `systemd-analyze verify` accepts `User=` with a bare space exactly as
    // silently as it accepts `User=` with nothing after it — systemd is not
    // the gate here, goetia is. A whitespace-only name is either trimmed to
    // empty downstream (the same reset-to-root `reject_empty` exists to
    // close) or names an account that cannot exist, so `reject_empty`
    // alone (a byte-length check) must not be the only gate.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nuser: \"  \"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("user.name"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_a_whitespace_only_user_id() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nuser:\n  id: \"  \"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("user.id"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_command_executable() {
    // Left unresolved, an empty `command[0]` would join against `base_dir`
    // and resolve to the manifest directory itself — silently, and with no
    // trace that the executable name was ever missing.
    let err = resolve_yaml("id: frpc\ncommand: [\"\"]\n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("command"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_cwd() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\ncwd: \"\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("cwd"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn accepts_a_whitespace_only_cwd() {
    // The boundary that stops a later reader from "completing" the
    // asymmetry by widening `reject_blank` (or its whitespace check) to
    // paths: a file named `" "` is legal on Unix, and an all-whitespace
    // path has no dangerous default the way an all-whitespace account
    // does — it simply fails to resolve like any other bad path, so it
    // must not be rejected here.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\ncwd: \"  \"\n";
    // No backend named: systemd refuses a path that ends in whitespace.
    let (spec, _warnings) = resolve_as(parse_manifest(yaml), &base_dir(), None)
        .expect("a whitespace-only path is not empty and has no unsafe default");
    assert_eq!(spec.cwd, Some(base_dir().join("  ")));
}

#[skuld::test]
fn rejects_an_empty_logs() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nlogs: \"\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("logs"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_env_key() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nenv:\n  \"\": \"value\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("env"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn accepts_an_empty_env_value() {
    // `FOO:` with nothing after it is a normal, meaningful assignment
    // (`FOO=`) — the boundary of the new rule, pinned in the other
    // direction from `rejects_an_empty_env_key`.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nenv:\n  FOO: \"\"\n";
    let (spec, _warnings) = resolve_yaml(yaml).expect("an empty env value is an ordinary assignment");
    assert_eq!(spec.env.get("FOO"), Some(&String::new()));
}

#[skuld::test]
fn accepts_an_empty_name() {
    // An empty systemd `Description=` is harmless — the boundary of the
    // new rule, pinned in the other direction from `rejects_an_empty_cwd`
    // and friends.
    let yaml = "id: frpc\nname: \"\"\ncommand: [/bin/frpc]\n";
    let (spec, _warnings) = resolve_yaml(yaml).expect("an empty name is a harmless empty Description=");
    assert_eq!(spec.name, "");
}

// Drive-relative paths ================================================================================================

// `C:bin` is neither absolute nor joinable: `PathBuf::push` truncates
// whenever the pushed path carries a prefix, so joining it against an
// absolute base silently discards the base and leaves a relative path. It is
// nonetheless a *valid* Windows path — drive-relative syntax, resolved
// against the current directory on that drive — so goetia must support it
// rather than reject it to protect its own join-based resolution strategy.

#[cfg(windows)]
#[skuld::test]
fn drive_relative_path_resolves_against_the_drives_current_directory_and_warns() {
    // `base_dir()` is the literal `C:\base`, and the test process never runs
    // there, so a result that differs from `base_dir().join("bin/frpc.exe")`
    // is safe to read as "anchored to the process's cwd, not the manifest".
    let yaml = "id: frpc\ncommand: [\"C:bin/frpc.exe\"]\n";
    let (spec, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("drive-relative command resolves");

    assert!(Path::new(&spec.command[0]).is_absolute());
    assert_ne!(
        spec.command[0],
        base_dir().join("bin").join("frpc.exe").to_string_lossy(),
        "a drive-relative path must anchor to the process's cwd on that drive, not to base_dir"
    );

    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].message.contains("command"),
        "warning should name the field: {}",
        warnings[0].message
    );
    assert!(
        warnings[0].message.contains("C:bin/frpc.exe"),
        "warning should quote the raw path: {}",
        warnings[0].message
    );
}

#[cfg(not(windows))]
#[skuld::test]
fn a_colon_bearing_name_is_an_ordinary_relative_filename() {
    // A colon is a legal Unix filename character, so on Linux/macOS
    // `C:bin/frpc.exe` is nothing but a relative path with an odd name —
    // joined against `base_dir` exactly like any other, with no warning.
    let yaml = "id: frpc\ncommand: [\"C:bin/frpc.exe\"]\n";
    let (spec, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves as an ordinary filename");
    assert_eq!(spec.command[0], base_dir().join("C:bin/frpc.exe").to_string_lossy());
    assert!(warnings.is_empty());
}

#[cfg(windows)]
#[skuld::test]
fn a_manifest_relative_path_beside_a_drive_relative_one_still_anchors_to_the_manifest() {
    // Pins that the fallback fires per path and does not capture the
    // ordinary relative `cwd` sitting next to a drive-relative `command`.
    let yaml = "id: frpc\ncommand: [\"C:bin/frpc.exe\"]\ncwd: data\n";
    let (spec, _warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves");
    assert_eq!(spec.cwd, Some(base_dir().join("data")));
}

#[cfg(windows)]
#[skuld::test]
fn drive_relative_resolution_collapses_dot_dot_the_windows_way() {
    // The one exception to `normalize_preserves_parent_dir_components`:
    // `GetFullPathNameW` collapses `..` lexically before the filesystem ever
    // sees the path, so a drive-relative path goetia resolved differently
    // from every other Windows tool would be the bug, not the other way
    // around.
    let yaml = r"id: frpc
command: ['C:a\..\b']
";
    let (spec, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("drive-relative path resolves");
    let resolved = Path::new(&spec.command[0]);

    assert!(
        !resolved
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "`..` must be collapsed on the drive-relative fallback path, got {resolved:?}"
    );
    assert_eq!(resolved.file_name(), Some(std::ffi::OsStr::new("b")));
    assert_eq!(warnings.len(), 1);
}

#[cfg(windows)]
#[skuld::test]
fn a_drive_relative_manifest_directory_resolves_and_warns() {
    let yaml = "id: frpc\ncommand: [bin/frpc.exe]\n";
    let (_spec, warnings) = resolve(parse_manifest(yaml), Path::new("C:proj")).expect("-f C:proj resolves");
    assert_eq!(warnings.len(), 1);
    // A manifest-level advisory belongs to no daemon.
    assert_eq!(warnings[0].id, None);
}

#[cfg(windows)]
#[skuld::test]
fn a_canonicalised_working_directory_carries_no_verbatim_prefix() {
    let mut warnings = Vec::new();
    let resolved = absolutize(Path::new("."), &mut warnings).expect("resolves");
    assert!(
        !resolved.to_string_lossy().starts_with(r"\\?\"),
        "resolved base_dir must not carry the verbatim prefix: {resolved:?}"
    );
}

#[cfg(windows)]
#[skuld::test]
fn verbatim_unc_strips_to_the_double_backslash_form() {
    // Not `UNC\server\share\repo` — the trap this helper exists to avoid.
    let verbatim = PathBuf::from(r"\\?\UNC\server\share\repo");
    assert_eq!(strip_verbatim_prefix(verbatim), PathBuf::from(r"\\server\share\repo"));
}

#[cfg(windows)]
#[skuld::test]
fn verbatim_disk_strips_to_the_drive_form() {
    let verbatim = PathBuf::from(r"\\?\C:\repo");
    assert_eq!(strip_verbatim_prefix(verbatim), PathBuf::from(r"C:\repo"));
}

#[cfg(windows)]
#[skuld::test]
fn a_verbatim_disk_prefix_resolves_unchanged_because_it_carries_an_implicit_root() {
    // `\\?\C:` parses as a `VerbatimDisk` prefix, not a bare `Disk` — and
    // `std`'s own `Prefix::has_implicit_root` is `true` for every prefix
    // except `Disk`. So, unlike the drive-relative `C:bin` case above,
    // `\\?\C:` is already absolute before `resolve_path_string`'s
    // `std::path::absolute` fallback ever runs: it passes through
    // unchanged, with no error and no drive-relative warning.
    let yaml = r"id: frpc
command: ['\\?\C:']
";
    let (spec, warnings) = resolve(parse_manifest(yaml), &base_dir())
        .expect("a VerbatimDisk prefix carries an implicit root and resolves");
    assert_eq!(
        spec.command[0], r"\\?\C:",
        "a verbatim prefix must pass through resolve unchanged"
    );
    assert!(
        warnings.is_empty(),
        "a VerbatimDisk prefix is not drive-relative and must not warn: {warnings:?}"
    );
}

// Interpolation =======================================================================================================

/// Write `goetia.yaml` into `dir`, and `.env` when `env` is given.
fn write_fixture(dir: &Path, yaml: &str, env: Option<&str>) {
    std::fs::write(dir.join("goetia.yaml"), yaml).expect("fixture manifest should be writable");
    if let Some(env) = env {
        std::fs::write(dir.join(".env"), env).expect("fixture .env should be writable");
    }
}

#[skuld::test]
fn a_dollar_inside_a_substituted_value_survives_resolve_unchanged(#[fixture(temp_dir)] dir: &Path) {
    // A `.env` value is fully literal, so a `$` in one is ordinary content —
    // passwords and connection strings carry them routinely. Substituted
    // text is never rescanned, so that `$` must reach the resolved spec
    // exactly as written. Pinning it here because the scanner rejects a
    // bare `$` in *manifest* text: anything that interpolated a second time
    // would turn a legal secret into
    // "`$` at byte N is not part of `${...}`" and fail the whole load.
    write_fixture(
        dir,
        "id: frpc\ncommand: [/bin/frpc]\nenv:\n  PASSWORD: ${SECRET}\n",
        Some("SECRET=abc$def\n"),
    );
    let manifest = dir.join("goetia.yaml");
    let (spec, _warnings) = load(&manifest).expect("a literal `$` in a .env value is legal");
    assert_eq!(spec.env.get("PASSWORD").map(String::as_str), Some("abc$def"));
}

#[skuld::test]
fn resolve_substitutes_rather_than_letting_a_reference_through() {
    // Interpolation lives inside `resolve`, not beside it in `load`: every
    // manifest's path to a `DaemonSpec` goes through it, and a literal `${MISSING}`
    // reaching a generated unit would be expanded by systemd itself,
    // silently, to an empty string.
    let yaml = "id: frpc\nname: ${ALSO_MISSING}\ncommand: [/bin/frpc, \"--flag=${MISSING}\"]\n";

    let err = resolve_yaml(yaml).expect_err("an undefined reference must not resolve");
    assert!(
        matches!(err, Error::Interpolate { .. }),
        "expected Interpolate, got: {err:?}"
    );
    assert!(
        err.to_string().contains("ALSO_MISSING"),
        "error should name the variable: {err}"
    );
}

#[skuld::test]
fn resolve_reads_the_env_file_beside_its_base_dir(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(
        dir,
        "id: frpc\nname: ${NAME}\ncommand: [/bin/frpc]\n",
        Some("NAME=Frpc Tunnel\n"),
    );
    let yaml = "id: frpc\nname: ${NAME}\ncommand: [/bin/frpc]\n";

    let (spec, _warnings) = resolve(parse_manifest(yaml), dir).expect("manifest should resolve");
    assert_eq!(spec.name, "Frpc Tunnel");
}

#[skuld::test]
fn resolve_does_not_read_the_env_file_when_the_manifest_has_no_dollar(#[fixture(temp_dir)] dir: &Path) {
    // The gate moved with the substitution step, so it is `resolve`'s to
    // hold now: a directory at the `.env` path would make `Vars::load` fail
    // with `Io`, and a dollarless manifest resolving anyway proves it was
    // never read.
    write_fixture(dir, "id: frpc\ncommand: [/bin/frpc]\n", None);
    std::fs::create_dir(dir.join(".env")).expect("fixture directory should be creatable");
    let yaml = "id: frpc\ncommand: [/bin/frpc]\n";

    let (spec, _warnings) = resolve(parse_manifest(yaml), dir).expect("a dollarless manifest should resolve");
    assert_eq!(spec.id.as_str(), "frpc");
}

#[skuld::test]
fn load_substitutes_from_the_env_file_beside_the_manifest(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(
        dir,
        "id: frpc\nname: ${NAME}\ncommand: [/bin/frpc]\n",
        Some("NAME=Frpc Tunnel\n"),
    );

    let (spec, _warnings) = load(&dir.join("goetia.yaml")).expect("manifest should load");
    assert_eq!(spec.name, "Frpc Tunnel");
}

#[skuld::test]
fn load_substitutes_when_the_path_names_a_directory(#[fixture(temp_dir)] dir: &Path) {
    // The `.env` is found beside the manifest, not beside the *argument* —
    // the two differ for a directory path.
    write_fixture(
        dir,
        "id: frpc\nname: ${NAME}\ncommand: [/bin/frpc]\n",
        Some("NAME=Frpc Tunnel\n"),
    );

    let (spec, _warnings) = load(dir).expect("directory should load");
    assert_eq!(spec.name, "Frpc Tunnel");
}

#[skuld::test]
fn load_reports_an_unset_variable_naming_it(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(
        dir,
        "id: frpc\nname: ${NAME}\ncommand: [/bin/frpc]\n",
        Some("OTHER=1\n"),
    );

    let err = load(dir).unwrap_err();
    assert!(
        matches!(err, Error::Interpolate { .. }),
        "expected Interpolate, got: {err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("NAME"), "error should name the variable: {text}");
    assert!(text.starts_with("name: "), "error should name the field: {text}");
}

#[skuld::test]
fn load_does_not_read_the_env_file_when_the_manifest_has_no_dollar(#[fixture(temp_dir)] dir: &Path) {
    // A directory at the `.env` path makes `Vars::load` fail with `Io`, so a
    // manifest that loads anyway demonstrably never read it: an unrelated or
    // root-only-readable `.env` cannot break a manifest that references no
    // variable.
    write_fixture(dir, "id: frpc\ncommand: [/bin/frpc]\n", None);
    std::fs::create_dir(dir.join(".env")).expect("fixture directory should be creatable");

    let (spec, _warnings) = load(dir).expect("a dollarless manifest should not read .env");
    assert_eq!(spec.id.as_str(), "frpc");
}

#[skuld::test]
fn an_unreadable_env_file_fails_only_a_manifest_whose_native_spec_needs_it(#[fixture(temp_dir)] dir: &Path) {
    // An unreadable `.env` fails a manifest only when the native spec needs it.
    write_fixture(dir, "", None);
    std::fs::create_dir(dir.join(".env")).expect("fixture directory should be creatable");
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let overridden = format!(
        "id: frpc\ncommand: [/bin/frpc]\nrestart-delay: ${{DELAY}}\n\
         backend-specific:\n  {native}:\n    restart-delay: 1s\n"
    );
    let (spec, warnings) = resolve(parse_manifest(&overridden), dir)
        .expect("an unreadable `.env` must not fail a manifest whose native spec has no `$`");
    assert_eq!(spec.restart_delay, Some(Duration::from_secs(1)));
    assert!(warnings.is_empty(), "{warnings:?}");

    let bare = "id: frpc\ncommand: [/bin/frpc]\nrestart-delay: ${DELAY}\n";
    let err = resolve(parse_manifest(bare), dir).unwrap_err();
    assert!(matches!(err, Error::Os { .. }), "expected Os, got: {err:?}");
}

#[skuld::test]
fn load_reads_the_env_file_for_an_escaped_dollar_only(#[fixture(temp_dir)] dir: &Path) {
    // The same fixture whose only `$` is a `$$` escape *does* read `.env`,
    // pinning `would_substitution_change`'s over-approximation rather than
    // leaving it to drift: one predicate serves this decision and the
    // shape-phase carve-out, and it must stay the safe, wide one.
    write_fixture(dir, "id: frpc\ncommand: [/bin/frpc, $$ARGS]\n", None);
    std::fs::create_dir(dir.join(".env")).expect("fixture directory should be creatable");

    let err = load(dir).unwrap_err();
    assert!(matches!(err, Error::Os { .. }), "expected Os, got: {err:?}");
}

#[skuld::test]
fn a_dollar_in_a_comment_is_ignored(#[fixture(temp_dir)] dir: &Path) {
    // The walk runs over the parsed manifest, not the file's text, so a `$`
    // the YAML parser discards never reaches it.
    write_fixture(
        dir,
        "id: frpc\n# ${NOPE} is a comment, not a reference\ncommand: [/bin/frpc]\n",
        None,
    );
    std::fs::create_dir(dir.join(".env")).expect("fixture directory should be creatable");

    let (spec, _warnings) = load(dir).expect("a commented-out reference should not read .env");
    assert_eq!(spec.id.as_str(), "frpc");
}

#[skuld::test]
fn load_preserves_authored_scalar_text_alongside_interpolation(#[fixture(temp_dir)] dir: &Path) {
    // Every leaf stays the text the user wrote, all the way through
    // interpolation: nothing round-trips through a YAML value, so no scalar
    // is renormalised by a number/bool parse it never asked for.
    let yaml = "id: frpc\nname: +5\ncommand: [/bin/frpc]\nenv:\n  A: 0.10\n  B: 1e3\n  \
                C: 0x10\n  D: 0o22\n  E: True\n  F: 1.\n  G: 18446744073709551616\n  1.50: plain\n  \
                H: ${TOKEN}\n";
    write_fixture(dir, yaml, Some("TOKEN=s3cret\n"));

    // No backend named: systemd refuses the key `1.50`, which no systemd unit can carry.
    let (spec, _warnings) = resolve_as(parse_manifest(yaml), dir, None).expect("manifest should load");
    let env = &spec.env;
    assert_eq!(spec.name, "+5");
    assert_eq!(env["A"], "0.10");
    assert_eq!(env["B"], "1e3");
    assert_eq!(env["C"], "0x10");
    assert_eq!(env["D"], "0o22");
    assert_eq!(env["E"], "True");
    assert_eq!(env["F"], "1.");
    assert_eq!(env["G"], "18446744073709551616");
    assert_eq!(env["1.50"], "plain");
    assert_eq!(env["H"], "s3cret");
}

#[skuld::test]
fn load_reports_original_positions(#[fixture(temp_dir)] dir: &Path) {
    // Interpolation runs on the parsed manifest, after the one and only
    // parse, so a syntax or shape diagnostic still points at the file as
    // written — with or without a `${VAR}` in it.
    let plain = dir.join("plain");
    let interpolated = dir.join("interpolated");
    for sub in [&plain, &interpolated] {
        std::fs::create_dir(sub).expect("fixture directory should be creatable");
    }
    write_fixture(
        &plain,
        "id: frpc\nname: literal\ncommand: [/bin/frpc]\nbogus: 1\n",
        None,
    );
    write_fixture(
        &interpolated,
        "id: frpc\nname: ${NAME}\ncommand: [/bin/frpc]\nbogus: 1\n",
        Some("NAME=x\n"),
    );

    let plain_err = load(&plain).unwrap_err().to_string();
    let interpolated_err = load(&interpolated).unwrap_err().to_string();

    assert!(plain_err.contains("bogus"), "should name the field: {plain_err}");
    assert!(plain_err.contains("line 4"), "should name the line: {plain_err}");
    assert_eq!(plain_err, interpolated_err);
}

#[skuld::test]
fn interpolated_values_pass_through_the_injection_gate(#[fixture(temp_dir)] dir: &Path) {
    // Substitution happens before `resolve`, so a `.env` value carrying a
    // control character is rejected by the same gate a literal one hits —
    // on every leaf that reaches a generated artifact, not just `name`.
    // `.env` cannot express a newline at all (`vars.rs` rejects both the
    // `\n` escape and a multi-line value), so this uses the worst control
    // character it *can* carry.
    // Each row names the field the error must blame. Asserting only
    // "control character" would let a row pass on a *different* leaf's
    // rejection than the one it claims to exercise.
    let leaves = [
        ("name", "name: ${EVIL}\ncommand: [/bin/frpc]\n"),
        ("command", "command: [/bin/frpc, \"${EVIL}\"]\n"),
        ("cwd", "command: [/bin/frpc]\ncwd: ${EVIL}\n"),
        ("logs", "command: [/bin/frpc]\nlogs: ${EVIL}\n"),
        ("env[LOG]", "command: [/bin/frpc]\nenv:\n  LOG: ${EVIL}\n"),
        ("user.name", "command: [/bin/frpc]\nuser: ${EVIL}\n"),
        // The struct form too: it is the syntax the `RawUser` split
        // introduced, and it reaches the gate by a different arm.
        ("user.name", "command: [/bin/frpc]\nuser:\n  name: ${EVIL}\n"),
    ];

    for (leaf, body) in leaves {
        write_fixture(dir, &format!("id: frpc\n{body}"), Some("EVIL='evil\u{1b}[2Jclear'\n"));

        let err = load(dir).unwrap_err();
        assert!(
            matches!(err, Error::Invalid { .. }),
            "{leaf}: expected Invalid, got: {err:?}"
        );
        assert!(
            err.to_string().contains("control character"),
            "{leaf}: expected the control-character gate, got: {err}"
        );
        assert!(
            err.to_string().contains(&format!("field `{leaf}`")),
            "{leaf}: the gate should blame this leaf, got: {err}"
        );
    }
}

#[skuld::test]
fn an_interpolated_value_cannot_create_a_field() {
    // A substituted value is one scalar and is never re-parsed as YAML: it
    // cannot change the id, rewrite `command`, or introduce a field. Driven
    // through `Vars::from_pairs` rather than a `.env` file because `vars.rs`
    // refuses to produce a newline-bearing value in the first place — this
    // pins the second line of defence behind that refusal.
    let yaml = "id: frpc\nname: ${EVIL}\ncommand: [/bin/frpc]\n";
    let evil = "x\ncommand: [/bin/evil]\nbogus: 1";
    let mut raw = parse_manifest(yaml);
    interpolate::spec(&mut raw, &Vars::from_pairs(&[("EVIL", evil)])).expect("substitution should succeed");

    assert_eq!(raw.id, "frpc");
    assert_eq!(raw.command.as_deref(), Some(&["/bin/frpc".to_string()][..]));
    assert_eq!(raw.name.as_deref(), Some(evil));

    // `resolve` runs the injection gate once, on the merged native spec.
    // Its own substitution pass is a no-op here: `evil` holds no `$`, so it
    // is left untouched and the injection gate is what actually rejects it.
    let err = resolve(raw, &base_dir()).unwrap_err();
    assert!(
        err.to_string().contains("control character"),
        "expected the control-character gate, got: {err}"
    );
}

// backend-specific overrides ==========================================================================================

/// A backend that is not this host's native one. `BackendKind::ALL` always has
/// at least two such entries (`BackendKind::native()` names at most one), so the
/// first match is deterministic and always exists.
fn non_native_backend() -> BackendKind {
    BackendKind::ALL
        .into_iter()
        .find(|&b| Some(b) != BackendKind::native())
        .expect("ALL has 3 entries; native() names at most 1")
}

/// A `user:` value `BackendKind::error` rejects for `backend`, on any host — so
/// a test can aim a per-backend verdict at whichever backend it is naming
/// as native.
fn user_rejected_by(backend: BackendKind) -> &'static str {
    match backend {
        // A numeric uid is never a Windows account.
        BackendKind::Scm => "{id: 1000}",
        // A SID is never a POSIX account.
        BackendKind::Systemd | BackendKind::Launchd => "{id: \"S-1-5-18\"}",
    }
}

/// `resolve`, with `native` named rather than detected. Only a backend in [`HOST_BACKENDS`], whose
/// path style is this host's, may be named.
fn resolve_yaml_as(native: BackendKind, yaml: &str) -> Result<(DaemonSpec, Vec<Warning>), Error> {
    resolve_as(parse_manifest(yaml), &base_dir(), Some(native))
}

#[skuld::test]
fn a_value_that_only_substitution_made_invalid_says_so(#[fixture(temp_dir)] dir: &Path) {
    // The other half of step 5's annotation, and the half whose premise
    // does hold: step 4 shape-checked this same merged spec and passed, so
    // substitution is provably what changed. Pinned here because nothing else
    // would notice if the verdict path stopped carrying this prefix.
    write_fixture(
        dir,
        "id: frpc\ncommand: [/bin/frpc]\nname: ${N}\n",
        Some("N=\"a\tb\"\n"),
    );
    let err = load(&dir.join("goetia.yaml")).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("after substituting from .env"), "{msg}");
    assert!(msg.contains("control character"), "{msg}");
}

#[skuld::test]
fn a_shape_error_on_a_field_the_native_override_supplied_carries_the_override_then_env(
    #[fixture(temp_dir)] dir: &Path,
) {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    name: ${{N}}\n");
    write_fixture(dir, &yaml, Some("N=\"a\tb\"\n"));
    let msg = resolve(parse_manifest(&yaml), dir).unwrap_err().to_string();
    let prefix = format!("daemon `frpc`: backend-specific.{native}: after substituting from .env: ");
    let rest = msg
        .strip_prefix(prefix.as_str())
        .unwrap_or_else(|| panic!("no `{prefix}` at the start of {msg}"));
    assert!(rest.contains("control character"), "{msg}");
}

#[skuld::test]
fn a_templated_user_is_judged_under_the_native_backend_and_nowhere_else(#[fixture(temp_dir)] dir: &Path) {
    // `spec::backend`'s module doc, made executable. Substitution runs for
    // the native backend only, so `${ACCT}` is unresolvable — and therefore
    // unchecked — under the other two, while the native one is substituted
    // in step 5 and judged there. Whichever backend is native, both halves
    // of the sentence are asserted on this host.
    //
    // `Scm` is never named in the overrides: `Scm.error` rejects a numeric uid, and `user.id` refuses
    // substitution outright (`interpolate::substitute_user`), so no templated value can reach it.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  systemd:\n    \
                user: ${ACCT}\n  launchd:\n    user: ${ACCT}\n";
    write_fixture(dir, yaml, Some("ACCT=NT AUTHORITY\\SYSTEM\n"));

    for &native in HOST_BACKENDS {
        let result = resolve_as(parse_manifest(yaml), dir, Some(native));
        if native == BackendKind::Scm {
            // Neither `${ACCT}` is substituted, so neither is judged, and `.env` is not read at all.
            result.expect("a template under a non-native backend is unresolvable, and so unchecked");
            continue;
        }
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Windows built-in"), "native `{native}`: {msg}");
        assert!(
            msg.contains(&format!("backend-specific.{native}")),
            "native `{native}`: {msg}"
        );
    }
}

#[skuld::test]
fn a_native_backend_verdict_names_the_override_that_caused_it() {
    // Not `.env`: step 4 skips the native backend, so step 5 is the first
    // pass to apply a per-backend rule to the native spec — there is no
    // earlier pass whose success would make substitution the only suspect,
    // and this manifest carries no `$` and sits beside no `.env` file.
    for &native in HOST_BACKENDS {
        let yaml = format!(
            "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    user: {}\n",
            user_rejected_by(native)
        );
        let err = resolve_yaml_as(native, &yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("backend-specific.{native}")),
            "native `{native}` must be attributed like its non-native twin: {msg}"
        );
        assert!(
            !msg.contains("after substituting"),
            "native `{native}`: nothing was substituted and no `.env` exists: {msg}"
        );
    }
}

#[skuld::test]
fn resolve_applies_the_native_backends_override() {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nuser: root\nbackend-specific:\n  {native}:\n    \
         user: bindreams\n"
    );
    let (spec, _warnings) = resolve_yaml(&yaml).expect("resolves");
    assert_eq!(spec.user, User::Name("bindreams".to_string()));
}

#[skuld::test]
fn resolve_ignores_a_non_native_backends_override() {
    let backend = non_native_backend();
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nuser: root\nbackend-specific:\n  {backend}:\n    \
         user: bindreams\n"
    );
    let (spec, _warnings) = resolve_yaml(&yaml).expect("resolves");
    assert_eq!(
        spec.user,
        User::Root,
        "a non-native override must not change the resolved spec"
    );
}

#[skuld::test]
fn an_invalid_non_native_override_fails_the_whole_manifest() {
    let backend = non_native_backend();
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    \
         user: \"evil\\nUser=0\"\n"
    );
    assert!(resolve_yaml(&yaml).is_err());
}

#[skuld::test]
fn an_invalid_native_override_fails_the_whole_manifest() {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    // The pass whose spec is actually installed, and the one whose failure
    // has a production consequence.
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    \
         user: \"evil\\nUser=0\"\n"
    );
    assert!(resolve_yaml(&yaml).is_err());
}

#[skuld::test]
fn an_override_error_names_the_backend_that_caused_it() {
    let non_native = non_native_backend();
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {non_native}:\n    \
         user: \"evil\\nUser=0\"\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains(&format!("backend-specific.{non_native}")),
        "non-native: {err}"
    );

    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    \
         user: \"evil\\nUser=0\"\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains(&format!("backend-specific.{native}")),
        "native: {err}"
    );
}

#[skuld::test]
fn a_base_spec_error_is_not_attributed_to_a_backend() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nname: \"evil\\nExecStart=/bin/evil\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        !err.to_string().contains("backend-specific"),
        "a base-spec error must not be attributed to a backend: {err}"
    );
}

#[skuld::test]
fn windows_style_absolute_paths_in_an_scm_override_are_accepted_on_any_host() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  scm:\n    \
                command: [\"C:\\\\Program Files\\\\frpc\\\\frpc.exe\"]\n";
    resolve_yaml(yaml).expect("a Windows-style absolute path under a non-native `scm:` override must resolve");
}

#[cfg(windows)]
#[skuld::test]
fn a_base_drive_relative_path_warns_once() {
    // The native pass is the only one that keeps warnings, so a base path
    // warns once however many backends the host sweeps. Windows-only: the
    // advisory fires only where `std::path::absolute` leaves a path
    // relative, a Windows prefix-without-root property.
    let yaml = "id: frpc\ncommand: [\"C:bin/frpc.exe\"]\n";
    let (_spec, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[cfg(windows)]
#[skuld::test]
fn windows_divergence_warning_follows_the_scm_merged_spec() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\ntype: managed\ncwd: .\nbackend-specific:\n  \
                scm:\n    type: simple\n";
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("resolves");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[cfg(windows)]
#[skuld::test]
fn windows_divergence_warning_fires_for_an_scm_only_managed_type() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\ntype: simple\ncwd: .\nbackend-specific:\n  \
                scm:\n    type: managed\n";
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("resolves");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[cfg(unix)]
#[skuld::test]
fn sub_second_delay_warning_follows_the_launchd_merged_spec() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nrestart-delay: 500ms\nbackend-specific:\n  \
                launchd:\n    restart-delay: 2s\n";
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Launchd, yaml).expect("resolves");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[skuld::test]
fn native_pass_alone_emits_every_native_advisory() {
    // One row per advisory `BackendKind::warn` can produce. A host runs the rows of the backends
    // it may simulate; the Linux and Windows lanes together cover every row.
    let cases = [
        // `warn_on_sub_second_restart_delay`
        (BackendKind::Launchd, "restart-delay: 500ms", "ThrottleInterval"),
        // `warn_on_windows_divergences`
        (BackendKind::Scm, "type: managed\ncwd: .", "silently unavailable"),
        // `warn_on_over_long_restart_delay`: the clamp, then the rounding.
        (
            BackendKind::Scm,
            "restart: on-failure\nrestart-delay: 100d",
            "exceeds the ~49.71 days",
        ),
        (
            BackendKind::Scm,
            "restart: on-failure\nrestart-delay: 1500us",
            "not a whole number of milliseconds",
        ),
    ];
    for (native, fields, expected) in cases.into_iter().filter(|(native, ..)| HOST_BACKENDS.contains(native)) {
        let yaml = format!("id: frpc\ncommand: [/bin/frpc]\n{fields}\n");
        let (_spec, warnings) = resolve_yaml_as(native, &yaml).expect("resolves");
        assert_eq!(warnings.len(), 1, "native `{native}`, `{fields}`: {warnings:?}");
        assert!(
            warnings[0].message.contains(expected),
            "native `{native}`, `{fields}`: {warnings:?}"
        );
    }
}

#[skuld::test]
fn a_templated_base_field_keeps_the_native_advisories_it_had_as_a_literal(#[fixture(temp_dir)] dir: &Path) {
    // The native advisories read the substituted native spec, so moving a
    // base `type`/`restart`/`restart-delay` into `.env` must not delete a
    // warning about the value that is installed either way. The two
    // manifests resolve to the same spec, so they owe the same advisories.
    write_fixture(dir, "", Some("KIND=managed\nRESTART=on-failure\nDELAY=500500us\n"));
    let fields = |kind: &str, restart: &str, delay: &str| {
        format!(
            "id: frpc\ncommand: [/bin/frpc]\ncwd: .\ntype: {kind}\n\
             restart: {restart}\nrestart-delay: {delay}\n"
        )
    };
    let owed = [(BackendKind::Launchd, 1), (BackendKind::Scm, 2)];
    for (native, expected) in owed.into_iter().filter(|(native, _)| HOST_BACKENDS.contains(native)) {
        let (_spec, from_literal) = resolve_as(
            parse_manifest(&fields("managed", "on-failure", "500500us")),
            dir,
            Some(native),
        )
        .expect("resolves, with advisories");
        let (_spec, from_env) = resolve_as(
            parse_manifest(&fields("${KIND}", "${RESTART}", "${DELAY}")),
            dir,
            Some(native),
        )
        .expect("resolves, with advisories");
        assert_eq!(
            from_literal.len(),
            expected,
            "native `{native}`: the literal manifest owes {expected}: {from_literal:?}"
        );
        assert_eq!(
            from_env, from_literal,
            "native `{native}`: a base field moved into `.env` must keep every advisory it had as a literal"
        );
    }
}

#[skuld::test]
fn a_native_backends_verdict_does_not_pre_empt_the_sweep() {
    // The native skip in step 4, asserted through its one observable
    // consequence: with two invalid overrides, the sweep's verdict is
    // reported and the native backend's is not, because the native one is
    // deferred to step 5 — where it judges the values that will actually be
    // installed, substituted, rather than the text they were written as.
    //
    // The native backend must come *before* the swept one in `BackendKind::ALL` for this to
    // distinguish anything: were it swept, its verdict would be raised before the other's. The
    // first host backend and its successor, rather than two names, so the pairing cannot rot if
    // that order changes.
    let first = BackendKind::ALL
        .iter()
        .position(|b| HOST_BACKENDS.contains(b))
        .expect("a host backend is in ALL");
    let native = BackendKind::ALL[first];
    let swept = BackendKind::ALL[first + 1];
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    user: {}\n  \
         {swept}:\n    user: {}\n",
        user_rejected_by(native),
        user_rejected_by(swept)
    );
    let err = resolve_yaml_as(native, &yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(&format!("backend-specific.{swept}")), "{msg}");
    assert!(!msg.contains(&format!("backend-specific.{native}")), "{msg}");
}

#[skuld::test]
fn a_host_with_no_native_backend_resolves_the_base_spec_alone() {
    // `resolve_as`'s fourth legal `native`, and the one no platform this
    // crate builds for produces — `BackendKind::native()` is `Some` on all
    // three, which is exactly why the seam has to be driven here. With no
    // backend to merge for, `merged_native` falls back to
    // `without_overrides` and every override is ignored.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nname: base\nbackend-specific:\n  \
                systemd:\n    name: from-systemd\n  launchd:\n    name: from-launchd\n  \
                scm:\n    name: from-scm\n";
    let (spec, _warnings) = resolve_as(parse_manifest(yaml), &base_dir(), None).expect("resolves");
    assert_eq!(spec.name, "base");

    // No backend is native, so no backend's advisories are this host's to give.
    let advisory = "id: frpc\ncommand: [/bin/frpc]\ncwd: .\ntype: managed\n\
                    restart: on-failure\nrestart-delay: 60d\n";
    let (_spec, warnings) = resolve_as(parse_manifest(advisory), &base_dir(), None).expect("resolves");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[skuld::test]
fn a_missing_command_with_no_native_backend_names_no_backend() {
    // `missing_command_error`'s `None` arm. There is no backend to install
    // to, so `backend-specific.<backend>.command` is not a remedy and the
    // message must not offer it — including when a `command` was written
    // under an override, which is the same "not this host's" value.
    let manifests = [
        "id: frpc\nname: frpc\n".to_string(),
        "id: frpc\nbackend-specific:\n  systemd:\n    command: [/bin/frpc]\n".to_string(),
    ];
    for yaml in manifests {
        let err = resolve_as(parse_manifest(&yaml), &base_dir(), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("command must not be empty"), "{msg}");
        assert!(
            !msg.contains("backend-specific"),
            "no backend to offer as a remedy: {msg}"
        );
        for backend in BackendKind::ALL {
            assert!(!msg.contains(&backend.to_string()), "no backend to name: {msg}");
        }
    }
}

#[skuld::test]
fn an_override_on_every_backend_resolves() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  launchd:\n    \
                user: bindreams\n  scm:\n    user: bindreams\n  systemd:\n    user: bindreams\n";
    resolve_yaml(yaml).expect("resolves");
}

#[skuld::test]
fn a_command_only_under_the_native_backend_installs() {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = format!("id: frpc\nbackend-specific:\n  {native}:\n    command: [/bin/frpc]\n");
    let (spec, _warnings) = resolve_yaml(&yaml).expect("resolves");
    assert!(!spec.command.is_empty());
}

#[skuld::test]
fn a_command_missing_for_the_native_backend_is_an_error() {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = "id: frpc\nname: frpc\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(&native.to_string()), "should name the backend: {msg}");
    assert!(msg.contains("command"), "should name the remedy: {msg}");
    assert!(
        msg.contains("backend-specific"),
        "should name the override remedy: {msg}"
    );
}

#[skuld::test]
fn a_command_missing_for_a_non_native_backend_resolves() {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    // A command present only for the native backend: completeness is
    // per-install, not per-backend, and a Windows-less manifest is usable
    // on Linux.
    let yaml = format!("id: frpc\nbackend-specific:\n  {native}:\n    command: [/bin/frpc]\n");
    resolve_yaml(&yaml).expect("resolves");
}

#[skuld::test]
fn an_empty_command_is_rejected_like_a_missing_one() {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = format!("id: frpc\nbackend-specific:\n  {native}:\n    command: []\n");
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(err.to_string().contains("command"));
}

#[skuld::test]
fn an_empty_user_name_is_rejected_on_every_backend() {
    // Guards the seam: `resolve_shape` must run `reject_blank` on the user. See `reject_blank`'s
    // doc comment (`resolve.rs`) for why an empty account name is dangerous rather than merely odd.
    assert!(resolve_yaml("id: frpc\ncommand: [/bin/frpc]\nuser: \"\"\n").is_err());
    assert!(resolve_yaml("id: frpc\ncommand: [/bin/frpc]\nuser:\n  id: \"\"\n").is_err());
}

#[skuld::test]
fn a_numeric_uid_under_scm_is_rejected_on_every_host() {
    // The finding that showed the architecture claim was false as first
    // written: a per-backend rejection applied to `merged_for` unmasked by
    // `Supplied` would reject far more than this.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  scm:\n    \
                user:\n      id: 1000\n";
    let err = resolve_yaml(yaml).unwrap_err();
    // Attributed on every host, from either pass: where `scm` is non-native
    // the rejection runs in the sweep (step 4), and where it is native
    // (Windows) the same rule runs in the native pass (step 5) — see
    // `a_native_backend_verdict_names_the_override_that_caused_it`.
    assert!(err.to_string().contains("backend-specific.scm"), "{err}");
}

#[skuld::test]
fn a_windows_builtin_account_under_a_posix_backend_is_rejected_on_every_host() {
    // `NT AUTHORITY\LocalSystem` alongside the bare name: it is the row
    // `windows_builtin` was missing, and the one that made a backend accept
    // an account under `systemd` while rejecting its own sibling
    // `NT AUTHORITY\SYSTEM`.
    for user in ["LocalService", r"NT AUTHORITY\LocalSystem", r"NT AUTHORITY\SYSTEM"] {
        for backend in ["systemd", "launchd"] {
            let yaml = format!(
                "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    \
                 user: \"{}\"\n",
                user.replace('\\', "\\\\")
            );
            assert!(resolve_yaml(&yaml).is_err(), "`{user}` under `{backend}`");
        }
    }
}

#[skuld::test]
fn a_sid_under_a_posix_backend_is_rejected_at_resolve_time() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  systemd:\n    \
                user:\n      id: \"S-1-5-19\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("unquoted"),
        "message should tell a user who meant a uid to write it unquoted: {err}"
    );
}

#[skuld::test]
fn a_base_uid_resolves_on_every_host() {
    // The regression guard on `Supplied`. `merged_for(Scm)` on this manifest
    // *is* the base spec (there is no `backend-specific` block at all), so
    // an unmasked `Scm.error` would reject a plain Linux manifest —
    // published interface, `README.md` — on Linux and macOS. No `cfg`: "on
    // every host" is the claim.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nuser:\n  id: 1000\n";
    let (spec, _warnings) = resolve_yaml(yaml).expect("a base numeric uid must resolve on every host");
    assert_eq!(spec.user, User::Id(AccountId::Uid(1000)));
}

#[skuld::test]
fn a_base_windows_account_resolves_on_every_host() {
    // The symmetric direction: without `Supplied`, these fail on Windows,
    // where `Systemd.error`/`Launchd.error` sweep the base spec. Neither
    // half is reachable from a Linux runner, which is exactly why it is
    // written here rather than left to be discovered on CI.
    let yaml_sid = "id: frpc\ncommand: [/bin/frpc]\nuser:\n  id: \"S-1-5-19\"\n";
    resolve_yaml(yaml_sid).expect("a base SID must resolve on every host");

    let yaml_name = "id: frpc\ncommand: [/bin/frpc]\nuser: LocalService\n";
    resolve_yaml(yaml_name).expect("a base Windows built-in name must resolve on every host");
}

#[skuld::test]
fn a_base_named_system_account_resolves_under_every_backend() {
    // `system` is a legal POSIX username, so `windows_only_account` must not
    // recognise it as Windows-only.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nuser: system\n";
    resolve_yaml(yaml).expect("a base `system` account must resolve under every backend");

    let yaml_override = "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  systemd:\n    \
                         user: system\n";
    resolve_yaml(yaml_override).expect("`system` under a systemd override must resolve: it is a legal POSIX username");
}

#[cfg(windows)]
#[skuld::test]
fn an_over_long_restart_delay_warns_when_scm_is_native() {
    // `restart: on-failure` is load-bearing: the advisory is unreachable for `Restart::Never`.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\ntype: managed\nrestart: on-failure\nrestart-delay: 100d\n";
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("resolves, with a warning");
    assert!(
        warnings.iter().any(|w| w.message.contains("restart-delay")),
        "expected the SCM clamp advisory: {warnings:?}"
    );
}

#[cfg(windows)]
#[skuld::test]
fn over_long_restart_delay_warns_for_simple_too() {
    let yaml = "id: frpc
command: [/bin/frpc]
type: simple
restart: on-failure
restart-delay: 100d
";
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("resolves, with a warning");
    assert!(
        warnings.iter().any(|w| w.message.contains("SC_ACTION")),
        "a simple daemon's restart-delay is an SC_ACTION.Delay now and is clamped like a managed one: {warnings:?}"
    );
}

#[cfg(windows)]
#[skuld::test]
fn an_over_long_restart_delay_with_no_type_does_not_warn() {
    // The unit-level twin of `huge_restart_delay_saturates_instead_of_overflowing`,
    // which asserts exactly one warning (the sub-second one) for a
    // near-`Duration::MAX` delay written the same way.
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nrestart-delay: {}s 1ns\n", u64::MAX);
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, &yaml).expect("resolves");
    assert!(
        !warnings.iter().any(|w| w.message.contains("SC_ACTION")),
        "the SCM clamp advisory is unreachable with no `restart:`: {warnings:?}"
    );
}

#[cfg(windows)]
#[skuld::test]
fn a_templated_drive_relative_path_warns_once_quoting_the_substituted_path(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\ncommand: [\"C:${SUB}\"]\n", Some("SUB=bin/frpc.exe\n"));
    let (_spec, warnings) = load(&dir.join("goetia.yaml")).expect("resolves");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].message.contains("C:bin/frpc.exe"),
        "warning should quote the substituted spelling: {}",
        warnings[0].message
    );
    assert!(
        !warnings[0].message.contains("${SUB}"),
        "warning must not quote the unresolved template: {}",
        warnings[0].message
    );
}

#[skuld::test]
fn an_interpolation_grammar_error_in_a_non_native_override_is_rejected_on_this_host() {
    let backend = non_native_backend();
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    name: \"$HOME\"\n");
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(err.to_string().contains("not part of"), "{err}");
}

#[skuld::test]
fn a_base_grammar_error_is_rejected_even_when_every_backend_overrides_the_field() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nname: \"a$b\"\nbackend-specific:\n  \
                launchd:\n    name: ok1\n  scm:\n    name: ok2\n  systemd:\n    name: ok3\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(err.to_string().contains("not part of"), "{err}");
}

#[skuld::test]
fn a_bare_dollar_in_an_env_value_survives_a_substituted_pass(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(
        dir,
        "id: frpc\ncommand: [/bin/frpc]\nenv:\n  X: ${S}\n",
        Some("S=ab$cd\n"),
    );
    let (spec, _warnings) = load(&dir.join("goetia.yaml")).expect("resolves");
    assert_eq!(spec.env["X"], "ab$cd");
}

#[skuld::test]
fn a_base_value_must_pass_the_shape_gate_even_when_every_backend_overrides_it() {
    // Deliberate: the base pass shape-checks every authored value, even one
    // no backend ends up using, so a dead value cannot sit in the manifest
    // as a trap for whoever later deletes an override.
    let yaml = "id: frpc\ncommand: [/bin/frpc]\ncwd: \"/opt/a\\tb\"\n\
                backend-specific:\n  launchd:\n    cwd: /var/frpc\n  scm:\n    cwd: /var/frpc\n  \
                systemd:\n    cwd: /var/frpc\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(err.to_string().contains("control character"), "{err}");
}

// The `$`-skip rule for `restart`/`type`/`restart-delay` ==============================================================

#[skuld::test]
fn a_templated_restart_resolves(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(
        dir,
        "id: frpc\ncommand: [/bin/frpc]\nrestart: ${R}\n",
        Some("R=on-failure\n"),
    );
    let (spec, _warnings) = load(&dir.join("goetia.yaml")).expect("resolves");
    assert_eq!(spec.restart, Restart::OnFailure);
}

#[skuld::test]
fn a_templated_type_resolves(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(dir, "id: frpc\ncommand: [/bin/frpc]\ntype: ${T}\n", Some("T=managed\n"));
    let (spec, _warnings) = load(&dir.join("goetia.yaml")).expect("resolves");
    assert_eq!(spec.kind, Kind::Managed);
}

#[skuld::test]
fn a_templated_restart_delay_resolves(#[fixture(temp_dir)] dir: &Path) {
    write_fixture(
        dir,
        "id: frpc\ncommand: [/bin/frpc]\nrestart-delay: ${D}\n",
        Some("D=2s\n"),
    );
    let (spec, _warnings) = load(&dir.join("goetia.yaml")).expect("resolves");
    assert_eq!(spec.restart_delay, Some(Duration::from_secs(2)));
}

#[skuld::test]
fn a_templated_restart_in_the_native_override_resolves(#[fixture(temp_dir)] dir: &Path) {
    let native = BackendKind::native().expect("native() is Some on every CI platform");
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {native}:\n    restart: ${{R}}\n");
    write_fixture(dir, &yaml, Some("R=on-failure\n"));
    let (spec, _warnings) = load(&dir.join("goetia.yaml")).expect("resolves");
    assert_eq!(spec.restart, Restart::OnFailure);
}

#[skuld::test]
fn a_literal_bad_restart_in_a_non_native_override_is_still_rejected() {
    let backend = non_native_backend();
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    restart: bogus\n");
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains(&format!("backend-specific.{backend}")),
        "{err}"
    );
}

#[skuld::test]
fn a_templated_restart_in_a_non_native_override_is_not_parsed_here() {
    let backend = non_native_backend();
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    restart: ${{R}}\n");
    resolve_yaml(&yaml)
        .expect("an unresolvable templated value under a non-native backend must be unchecked, not guessed at");
}

#[skuld::test]
fn an_escaped_dollar_in_an_enum_field_is_deferred_and_then_rejected() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nrestart: $$never\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        err.to_string().contains("field `restart` is `$never`"),
        "the value must be deferred (it carries a `$`) and then rejected once substitution changes nothing: {err}"
    );
}

#[skuld::test]
fn a_variable_used_only_in_a_non_native_override_does_not_read_the_env_file(#[fixture(temp_dir)] dir: &Path) {
    let backend = non_native_backend();
    let yaml = format!("id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    restart: ${{R}}\n");
    write_fixture(dir, &yaml, None);
    std::fs::create_dir(dir.join(".env")).expect("fixture directory should be creatable");
    let (spec, _warnings) =
        load(&dir.join("goetia.yaml")).expect("must not read .env for a value it will never substitute");
    assert_eq!(spec.id.as_str(), "frpc");
}

// The rules that reject `$` outright ==================================================================================

#[skuld::test]
fn an_interpolated_id_is_refused() {
    let yaml = "id: ${ID}\ncommand: [/bin/frpc]\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        matches!(&err, Error::Interpolate { path, .. } if path == "id"),
        "expected an interpolation error at `id`: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "id: a daemon id cannot be interpolated: it names the installed service, so an interpolated one could \
         change which service this manifest installs; write the id literally"
    );
    assert!(
        !err.to_string().contains("does not match the required pattern"),
        "the no-interpolation rejection must run first: {err}"
    );
}

#[skuld::test]
fn a_dollar_in_an_env_name_is_rejected_under_every_backend() {
    let backend = non_native_backend();
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    \
         env:\n      ${{K}}: v\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains("an `env` name cannot be interpolated"),
        "{err}"
    );
}

#[skuld::test]
fn a_dollar_in_a_user_id_is_rejected_under_every_backend() {
    let backend = non_native_backend();
    let yaml = format!(
        "id: frpc\ncommand: [/bin/frpc]\nbackend-specific:\n  {backend}:\n    \
         user:\n      id: \"${{S}}\"\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(err.to_string().contains(interpolate::USER_ID_MESSAGE), "{err}");
}

// A host warns only about its native backend ==========================================================================

#[skuld::test]
fn base_restart_always_resolves_on_each_host_backend() {
    // Whether a backend can honour `always` is its generator's verdict, at install and preview: the
    // base spec is nobody's override, so resolution accepts it.
    let yaml = "id: svc
command: [/bin/svc]
restart: always
";
    for &native in HOST_BACKENDS {
        let (spec, _warnings) = resolve_yaml_as(native, yaml).unwrap_or_else(|e| panic!("native `{native}`: {e}"));
        assert_eq!(spec.restart, Restart::Always, "native `{native}`");
    }
}

#[skuld::test]
fn a_host_warns_only_about_its_native_backend() {
    // One manifest that trips every advisory: `type: managed` with a `cwd` (SCM divergence), `on-failure` with
    // a delay that is neither whole seconds (launchd) nor whole milliseconds (SCM).
    let yaml = "id: frpc
command: [/bin/frpc]
type: managed
cwd: .
restart: on-failure
restart-delay: 500500us
";

    for &native in HOST_BACKENDS {
        let (_spec, warnings) = resolve_yaml_as(native, yaml).expect("resolves");
        match native {
            BackendKind::Systemd => assert!(warnings.is_empty(), "systemd has no advisories: {warnings:?}"),
            BackendKind::Launchd => {
                let [launchd] = warnings.as_slice() else {
                    panic!("launchd owes exactly its own advisory: {warnings:?}");
                };
                assert!(launchd.message.contains("not a whole number of seconds"), "{launchd:?}");
            }
            BackendKind::Scm => {
                let [divergence, rounding] = warnings.as_slice() else {
                    panic!("scm owes exactly its own two advisories: {warnings:?}");
                };
                assert!(divergence.message.contains("silently unavailable"), "{divergence:?}");
                assert!(
                    rounding.message.contains("not a whole number of milliseconds"),
                    "{rounding:?}"
                );
            }
        }
    }
}

#[cfg(unix)]
#[skuld::test]
fn a_supplied_non_native_value_is_still_refused() {
    let yaml = "id: frpc
command: [/bin/frpc]
backend-specific:
  scm:
    user: { id: 1000 }
";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemon `frpc`: backend-specific.scm: `1000` is a numeric uid, which is never a Windows account"
    );
}

#[cfg(windows)]
#[skuld::test]
fn a_non_native_overrides_drive_relative_path_does_not_warn() {
    // The advisory is about a path the installed spec would carry; the systemd override is not it.
    let yaml = "id: frpc
command: [/bin/frpc]
backend-specific:
  systemd:
    command: [\"C:bin/frpc.exe\"]
";
    let (_spec, warnings) = resolve_yaml_as(BackendKind::Scm, yaml).expect("resolves");
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// Each input the `systemd` module's tests show a systemd host refusing, which an SCM host carries.
#[cfg(windows)]
#[skuld::test]
fn an_scm_host_accepts_every_value_a_systemd_host_refuses() {
    let opt = abs("opt/x").display().to_string();
    let base = format!("id: frpc\ncommand: ['{}']\n", program("frpc"));
    let cases = [
        ("env: {MY-VAR: x}".to_string(), base_dir()),
        ("name: ' a'".to_string(), base_dir()),
        ("name: 'a '".to_string(), base_dir()),
        (format!("cwd: '{opt} '"), base_dir()),
        ("cwd: .".to_string(), abs("a ")),
        (format!("cwd: '{opt}\\..\\y'"), base_dir()),
        (format!("logs: '{opt}\\..\\y'"), base_dir()),
        ("cwd: sub".to_string(), base_dir().join("..").join("x")),
        ("logs: sub".to_string(), base_dir().join("..").join("x")),
    ];
    for (extra, dir) in cases {
        let yaml = format!("{base}{extra}\n");
        resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Scm)).unwrap_or_else(|e| panic!("{extra}: {e}"));
    }
}

/// The message quotes the resolved path as it is; the sink that prints it escapes it.
#[skuld::test]
fn a_refused_resolved_path_is_escaped_where_the_error_is_printed() {
    let id = Id::try_from("frpc").unwrap();
    let e =
        Error::from(check_resolved(&id, PathField::Cwd, "C:", Path::new("/a\nb\u{202e}c"), DirJoin::Not).unwrap_err());
    assert_eq!(
        e.report().to_string(),
        r"daemon `frpc`: field `cwd` is `C:`, which resolves to `/a\u{a}b\u{202e}c`, which contains a control character, which is forbidden"
    );
}

// systemd's `Environment=` names ======================================================================================

#[skuld::test]
fn an_env_key_systemd_accepts_draws_no_complaint_on_any_host() {
    let yaml = "id: frpc\ncommand: [/bin/frpc]\nenv:\n  MY_VAR: x\n";
    for &native in HOST_BACKENDS {
        let (_spec, warnings) = resolve_yaml_as(native, yaml).unwrap_or_else(|e| panic!("{native}: {e}"));
        assert!(warnings.is_empty(), "{native}: {warnings:?}");
    }
}

// What systemd refuses ================================================================================================
//
// The rules of a host whose native backend is systemd, which never runs on Windows.

#[cfg(unix)]
mod systemd {
    use super::*;
    use crate::spec::test_support::assert_no_run_of_spaces;

    #[skuld::test]
    fn only_a_systemd_host_refuses_an_env_key_systemd_would_drop() {
        let yaml = "id: frpc
command: [/bin/frpc]
env:
  MY-VAR: x
";

        let msg = resolve_yaml_as(BackendKind::Systemd, yaml).unwrap_err().to_string();
        assert!(msg.contains("\"MY-VAR\"") && msg.contains("Environment="), "{msg}");

        // Another host's native backend carries the key; a systemd advisory is not its to give.
        let (_spec, warnings) = resolve_yaml_as(BackendKind::Launchd, yaml).unwrap_or_else(|e| panic!("launchd: {e}"));
        assert!(warnings.is_empty(), "launchd: {warnings:?}");
    }

    #[skuld::test]
    fn a_systemd_refusal_names_the_override_only_when_the_override_wrote_the_value() {
        let yaml = "id: frpc
command: [/bin/frpc]
backend-specific:
  systemd:
    env: {MY-VAR: x}
";
        let msg = resolve_yaml_as(BackendKind::Systemd, yaml).unwrap_err().to_string();
        assert!(msg.contains("backend-specific.systemd: "), "{msg}");

        // The same value in the base, and an override that wrote something else: not the override's.
        let yaml = "id: frpc
command: [/bin/frpc]
env: {MY-VAR: x}
backend-specific:
  systemd:
    name: other
";
        let msg = resolve_yaml_as(BackendKind::Systemd, yaml).unwrap_err().to_string();
        assert!(!msg.contains("backend-specific"), "{msg}");
    }

    #[skuld::test]
    fn an_override_that_wrote_a_different_env_key_does_not_take_the_blame_for_a_base_key() {
        let yaml =
            "id: frpc\ncommand: [/bin/frpc]\nenv: {MY-VAR: x}\nbackend-specific:\n  systemd:\n    env: {OTHER: y}\n";
        let msg = resolve_yaml_as(BackendKind::Systemd, yaml).unwrap_err().to_string();
        assert!(msg.contains("\"MY-VAR\"") && !msg.contains("backend-specific"), "{msg}");

        let yaml =
            "id: frpc\ncommand: [/bin/frpc]\nenv: {MY-VAR: x}\nbackend-specific:\n  systemd:\n    env: {MY-VAR: y}\n";
        let msg = resolve_yaml_as(BackendKind::Systemd, yaml).unwrap_err().to_string();
        assert!(msg.contains("backend-specific.systemd: "), "{msg}");
    }

    // What systemd cannot carry, refused at resolve ===================================================================

    /// A manifest of an absolute program with `extra` lines (and `command` unless `extra` sets it).
    fn systemd_manifest(extra: &str) -> String {
        let command = if extra.contains("command:") {
            String::new()
        } else {
            format!("command: ['{}']\n", program("frpc"))
        };
        format!("id: frpc\n{command}{extra}\n")
    }

    #[skuld::test]
    fn a_systemd_host_refuses_at_resolve_every_value_its_generator_refuses() {
        let opt = abs("opt/x").display().to_string();
        let frpc = program("frpc");
        let cases = [
            (format!("command: ['{frpc}\\y']"), "`command[0]`"),
            (format!("command: ['{frpc}\"y']"), "`command[0]`"),
            (format!("command: ['{frpc}''y']"), "`command[0]`"),
            (format!("command: ['{frpc} ']"), "`command[0]`"),
            ("name: ' a'".to_string(), "Description="),
            ("name: 'a '".to_string(), "Description="),
            (format!("cwd: '{opt} '"), "WorkingDirectory="),
            (format!("logs: '{opt} '"), "StandardOutput="),
            (format!("cwd: '{opt}/../y'"), "`cwd`"),
            (format!("logs: '{opt}/../y'"), "`logs`"),
        ];
        for (extra, needle) in cases {
            let yaml = systemd_manifest(&extra);
            let msg = resolve_yaml_as(BackendKind::Systemd, &yaml)
                .expect_err(&format!("systemd: {extra}"))
                .to_string();
            assert!(msg.contains(needle), "{extra}: {msg}");
            resolve_yaml_as(BackendKind::Launchd, &yaml).unwrap_or_else(|e| panic!("launchd: {extra}: {e}"));
        }
    }

    #[skuld::test]
    fn a_systemd_refusal_at_resolve_of_a_path_value_follows_the_same_attribution_as_an_env_key() {
        let opt = abs("opt/x").display().to_string();
        // The override wrote the `..`.
        let yaml = systemd_manifest(&format!("backend-specific:\n  systemd:\n    cwd: '{opt}/../y'"));
        let msg = resolve_yaml_as(BackendKind::Systemd, &yaml).unwrap_err().to_string();
        assert!(
            msg.contains("backend-specific.systemd: ") && msg.contains("`cwd`"),
            "{msg}"
        );

        // The base wrote it, and the override wrote something else.
        let yaml = systemd_manifest(&format!(
            "cwd: '{opt}/../y'\nbackend-specific:\n  systemd:\n    name: other"
        ));
        let msg = resolve_yaml_as(BackendKind::Systemd, &yaml).unwrap_err().to_string();
        assert!(msg.contains("`cwd`") && !msg.contains("backend-specific"), "{msg}");
    }

    #[skuld::test]
    fn a_parent_dir_the_manifest_directory_adds_is_attributed_to_dash_f() {
        let dir = base_dir().join("..").join("x");
        for field in ["cwd", "logs"] {
            let yaml = systemd_manifest(&format!("{field}: sub"));
            let msg = resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Systemd))
                .expect_err(field)
                .to_string();
            assert!(
                msg.contains("`-f`") && msg.contains("`..`") && msg.contains(&format!("`{field}`")),
                "{field}: {msg}"
            );
            resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Launchd))
                .unwrap_or_else(|e| panic!("{field}: {e}"));
        }

        // A `..` the field wrote is the field's, even under a clean `-f`, and so is one beside the
        // manifest directory's.
        for dir in [base_dir(), dir] {
            let yaml = systemd_manifest("cwd: ../y");
            let msg = resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Systemd))
                .unwrap_err()
                .to_string();
            assert!(msg.contains("`cwd`") && !msg.contains("`-f`"), "{msg}");
        }
    }

    #[skuld::test]
    fn a_parent_dir_in_command_zero_is_accepted_by_a_systemd_host() {
        // systemd runs `/bin/../bin/frpc`.
        let yaml = systemd_manifest(&format!("command: ['{}/../bin/frpc']", abs("opt").display()));
        resolve_yaml_as(BackendKind::Systemd, &yaml).expect("resolves");
    }

    // A manifest directory's own defects, on systemd ==================================================================

    #[skuld::test]
    fn a_leading_parent_dir_of_a_relative_manifest_directory_is_accepted_by_a_systemd_host() {
        let yaml = systemd_manifest("cwd: .");
        let (spec, _) = resolve_as(parse_manifest(&yaml), Path::new("../app"), Some(BackendKind::Systemd))
            .expect("`-f ../app` resolves without a `..`");
        let expected = canonical_cwd().parent().expect("parent").join("app");
        assert_eq!(spec.cwd, Some(expected));
    }

    #[skuld::test]
    fn a_parent_dir_after_a_named_component_of_dash_f_is_refused_by_a_systemd_host_naming_dash_f() {
        let yaml = systemd_manifest("cwd: .");
        let msg = resolve_as(parse_manifest(&yaml), Path::new("a/../b"), Some(BackendKind::Systemd))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("`-f") && msg.contains("`cwd`") && msg.contains("`..`"),
            "{msg}"
        );
        resolve_as(parse_manifest(&yaml), Path::new("a/../b"), Some(BackendKind::Launchd))
            .expect("only systemd refuses");
    }

    #[skuld::test]
    fn a_backslash_in_dash_f_is_blamed_on_dash_f_when_command_zero_is_relative() {
        let dir = abs(r"a\b");
        for yaml in [
            "id: frpc
command: [bin/frpc]
",
            "id: frpc
backend-specific:
  systemd:
    command: [bin/frpc]
",
        ] {
            let msg = resolve_as(parse_manifest(yaml), &dir, Some(BackendKind::Systemd))
                .expect_err(yaml)
                .to_string();
            assert!(
                msg.contains("`command[0]`") && msg.contains("`-f ") && msg.contains("backslash"),
                "{msg}"
            );
            assert!(
                !msg.contains("backend-specific"),
                "the override did not write it: {msg}"
            );
        }
        let yaml = "id: frpc
command: [bin/frpc]
";
        resolve_as(parse_manifest(yaml), &dir, Some(BackendKind::Launchd)).expect("only systemd refuses");
    }

    #[skuld::test]
    fn a_backslash_the_field_wrote_in_a_relative_command_zero_is_the_fields() {
        let yaml = systemd_manifest(r"command: ['bin\frpc']");
        let msg = resolve_as(parse_manifest(&yaml), &abs("base"), Some(BackendKind::Systemd))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`command[0]`") && !msg.contains("`-f "), "{msg}");
    }

    #[skuld::test]
    fn edge_whitespace_in_dash_f_is_blamed_on_dash_f_for_cwd_and_logs() {
        let dir = abs("a ");
        for field in ["cwd", "logs"] {
            let yaml = systemd_manifest(&format!("{field}: ."));
            let msg = resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Systemd))
                .expect_err(field)
                .to_string();
            assert!(
                msg.contains("`-f ") && msg.contains(&format!("`{field}`")) && msg.contains("whitespace"),
                "{field}: {msg}"
            );
        }
    }

    #[skuld::test]
    fn an_absolute_field_that_spells_the_manifest_directorys_prefix_is_the_fields_to_blame() {
        // `-f` holds a `..`, and so does the absolute `cwd`, which merely starts with the same text.
        let dir = base_dir().join("..").join("x");
        let cwd = abs("base/../x/y").display().to_string();
        let yaml = systemd_manifest(&format!("cwd: '{cwd}'"));
        let msg = resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Systemd))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`cwd`") && !msg.contains("`-f "), "{msg}");

        // Absolute with only the directory's prefix, whose whitespace is the directory's text too.
        let dir = abs("a ");
        let yaml = systemd_manifest(&format!("cwd: '{}'", abs("a ").display()));
        let msg = resolve_as(parse_manifest(&yaml), &dir, Some(BackendKind::Systemd))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("WorkingDirectory=") && !msg.contains("`-f "), "{msg}");
    }

    #[skuld::test]
    fn only_the_defect_the_field_did_not_write_is_dash_fs() {
        // `-f` holds a backslash; the field wrote only a trailing space.
        let yaml = systemd_manifest("command: ['bin/frpc ']");
        let msg = resolve_as(parse_manifest(&yaml), &abs(r"a\b"), Some(BackendKind::Systemd))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("`command[0]`") && msg.contains("`-f ") && msg.contains("backslash"),
            "{msg}"
        );
        assert_no_run_of_spaces(&msg);

        // `-f` holds a `..`; the field wrote only a leading space.
        let yaml = systemd_manifest("cwd: ' x'");
        let msg = resolve_as(parse_manifest(&yaml), Path::new("a/../b"), Some(BackendKind::Systemd))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("`-f ") && msg.contains("`..`") && msg.contains("`cwd`"),
            "{msg}"
        );
        assert_no_run_of_spaces(&msg);
    }

    #[skuld::test]
    fn a_parent_dir_the_field_wrote_says_how_to_fix_it() {
        let yaml = systemd_manifest(&format!("cwd: '{}/../y'", abs("opt/x").display()));
        let msg = resolve_yaml_as(BackendKind::Systemd, &yaml).unwrap_err().to_string();
        assert!(msg.contains("write the path without `..`"), "{msg}");
    }
}
