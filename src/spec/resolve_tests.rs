use std::path::PathBuf;
use std::time::Duration;

use super::*;

// `Path::new("/opt/rt").is_absolute()` is `false` on Windows, and
// `PathBuf::from(r"C:\base").join("/opt/rt")` yields `C:/opt/rt` — so every
// test below that exercises absolute-path resolution needs a base dir the
// host platform actually considers absolute.
#[cfg(windows)]
fn base_dir() -> PathBuf {
    PathBuf::from(r"C:\base")
}
#[cfg(not(windows))]
fn base_dir() -> PathBuf {
    PathBuf::from("/base")
}

fn parse_manifest(yaml: &str) -> RawManifest {
    serde_yaml_ng::from_str(yaml).expect("fixture yaml should parse")
}

fn resolve_yaml(yaml: &str) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
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
daemons:
  frpc:
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
daemons:
  frpc:
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
daemons:
  frpc:
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
daemons:
  frpc:
    command: ["bin/frpc"]
    env:
      "FOO=BAR": "1"
"#;
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frpc"), "error should name the daemon: {msg}");
    assert!(msg.contains('='), "error should call out the `=`: {msg}");
}

/// Beyond the four gate tests the plan names by name: the same directive
/// injection is possible through `user`'s bare-string form (it lands in
/// `User=`/`UserName` unescaped, exactly like `name`), so it goes through
/// the same gate.
#[skuld::test]
fn rejects_control_characters_in_user_name() {
    let yaml = r#"
daemons:
  frpc:
    command: ["bin/frpc"]
    user: "evil\nUser=0"
"#;
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frpc"), "error should name the daemon: {msg}");
    assert!(msg.contains("user"), "error should name the field: {msg}");
}

#[skuld::test]
fn rejects_control_characters_in_cwd_and_logs() {
    let cwd_err = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n    cwd: \"evil\\nX=1\"\n").unwrap_err();
    assert!(cwd_err.to_string().contains("cwd"));

    let logs_err = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n    logs: \"evil\\nX=1\"\n").unwrap_err();
    assert!(logs_err.to_string().contains("logs"));
}

// Resolution ==========================================================================================================

#[skuld::test]
fn resolve_makes_paths_absolute_against_base_dir() {
    let yaml = r#"
daemons:
  frpc:
    command: ["bin/frpc.exe", "-c", "frpc.toml"]
    cwd: "."
    logs: "logs/frpc.log"
"#;
    let (specs, _warnings) = resolve_yaml(yaml).expect("valid manifest should resolve");
    let spec = &specs[0];

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
    let yaml = format!("daemons:\n  frpc:\n    command: ['{absolute}']\n");
    let (specs, _warnings) = resolve_yaml(&yaml).expect("valid manifest should resolve");
    assert_eq!(specs[0].command[0], absolute);
}

#[skuld::test]
fn resolve_defaults_name_to_id() {
    let (specs, _) = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n").unwrap();
    assert_eq!(specs[0].name, "frpc");
}

#[skuld::test]
fn resolve_defaults_user_to_root() {
    let (specs, _) = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n").unwrap();
    assert_eq!(specs[0].user, User::Root);
}

#[skuld::test]
fn resolve_defaults_kind_to_simple() {
    let (specs, _) = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n").unwrap();
    assert_eq!(specs[0].kind, Kind::Simple);
}

#[skuld::test]
fn resolve_defaults_restart_to_never() {
    let (specs, _) = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n").unwrap();
    assert_eq!(specs[0].restart, Restart::Never);
}

#[skuld::test]
fn resolve_rejects_empty_command() {
    let err = resolve_yaml("daemons:\n  frpc:\n    command: []\n").unwrap_err();
    assert!(err.to_string().contains("command"));
}

// Warnings ============================================================================================================

#[skuld::test]
fn managed_on_windows_warns_for_cwd_and_logs() {
    let yaml = r#"
daemons:
  svc:
    command: ["bin/svc.exe"]
    cwd: "."
    logs: "logs/svc.log"
    type: managed
"#;
    let (specs, warnings) = resolve_yaml(yaml).expect("accepted with a warning, not rejected");
    assert_eq!(specs[0].kind, Kind::Managed);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].id.as_str(), "svc");
    assert!(warnings[0].message.contains("cwd") || warnings[0].message.contains("logs"));
}

#[skuld::test]
fn simple_on_windows_does_not_warn_for_cwd_and_logs() {
    let yaml = r#"
daemons:
  svc:
    command: ["bin/svc.exe"]
    cwd: "."
    logs: "logs/svc.log"
    type: simple
"#;
    let (_specs, warnings) = resolve_yaml(yaml).unwrap();
    assert!(warnings.is_empty());
}

#[skuld::test]
fn managed_always_restart_warns() {
    let yaml = r#"
daemons:
  svc:
    command: ["bin/svc.exe"]
    type: managed
    restart: always
"#;
    let (specs, warnings) = resolve_yaml(yaml).expect("accepted with a warning, not rejected");
    assert_eq!(specs[0].restart, Restart::Always);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("always"));
}

#[skuld::test]
fn simple_always_restart_does_not_warn() {
    let yaml = r#"
daemons:
  svc:
    command: ["bin/svc.exe"]
    type: simple
    restart: always
"#;
    let (_specs, warnings) = resolve_yaml(yaml).unwrap();
    assert!(warnings.is_empty());
}

#[skuld::test]
fn sub_second_restart_delay_rounds_up_and_warns() {
    let yaml = r#"
daemons:
  frpc:
    command: ["bin/frpc"]
    restart-delay: "1.5s"
"#;
    let (specs, warnings) = resolve_yaml(yaml).expect("accepted with a warning, not rejected");

    // The blob keeps the authored value unrounded, so generation stays
    // deterministic; only the launchd generator (Task 7) rounds up.
    assert_eq!(specs[0].restart_delay, Some(Duration::from_millis(1500)));
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].message.contains('2'),
        "warning should name the rounded value: {}",
        warnings[0].message
    );
}

#[skuld::test]
fn whole_second_restart_delay_does_not_warn() {
    let yaml = r#"
daemons:
  frpc:
    command: ["bin/frpc"]
    restart-delay: "2s"
"#;
    let (specs, warnings) = resolve_yaml(yaml).unwrap();
    assert_eq!(specs[0].restart_delay, Some(Duration::from_secs(2)));
    assert!(warnings.is_empty());
}

// resolve() / load() over multiple daemons and all-or-nothing failure =================================================

#[skuld::test]
fn resolve_returns_every_daemon() {
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
  websocat:
    command: [bin/websocat]
";
    let (specs, _warnings) = resolve_yaml(yaml).unwrap();
    let mut ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["frpc", "websocat"]);
}

#[skuld::test]
fn one_invalid_daemon_fails_the_whole_manifest() {
    let yaml = "
daemons:
  frpc:
    command: [bin/frpc]
  websocat:
    command: []
";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(err.to_string().contains("websocat"));
}

// load() ==============================================================================================================

#[skuld::test]
fn load_reads_a_file_path() {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    let manifest_path = dir.path().join("goetia.yaml");
    std::fs::write(&manifest_path, "daemons:\n  frpc:\n    command: [bin/frpc]\n").unwrap();

    let (specs, _warnings) = load(&manifest_path).expect("file path should load");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id.as_str(), "frpc");
    // Relative paths resolve against the manifest's own directory, not the
    // process's current directory.
    let expected = dir.path().join("bin").join("frpc").to_string_lossy().into_owned();
    assert_eq!(specs[0].command[0], expected);
}

#[skuld::test]
fn load_reads_a_directory_containing_goetia_yaml() {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::write(
        dir.path().join("goetia.yaml"),
        "daemons:\n  frpc:\n    command: [bin/frpc]\n",
    )
    .unwrap();

    let (specs, _warnings) = load(dir.path()).expect("directory should load");
    assert_eq!(specs.len(), 1);
}

#[skuld::test]
fn load_reports_io_error_for_missing_path() {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    let missing = dir.path().join("does-not-exist.yaml");

    let err = load(&missing).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "expected an Io error, got: {err:?}");
}

// Emission hardening ==================================================================================================

/// A manifest whose `name` is `value`, resolved. Drives the validation gate
/// from one field without repeating the YAML in every test.
fn resolve_with_name(value: &str) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    let yaml = format!("daemons:\n  frpc:\n    name: {value:?}\n    command: [/bin/frpc]\n");
    resolve_yaml(&yaml)
}

/// The same for an env value, which reaches `Environment=` on systemd and
/// `EnvironmentVariables` on launchd.
fn resolve_with_env_value(value: &str) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    let yaml = format!("daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      FOO: {value:?}\n");
    resolve_yaml(&yaml)
}

#[skuld::test]
fn rejects_trailing_backslash_in_name() {
    // systemd reads a backslash at end of line as a line continuation, so a
    // `name` ending in one merges `Description=` with whatever follows — which
    // can be the `[Service]` section header, or the `User=` directive. A
    // swallowed `User=` silently runs the daemon as root instead of the
    // requested account, so this is a privilege boundary, not formatting.
    let err = resolve_with_name("FRP client\\").expect_err("trailing backslash must be rejected");
    assert!(
        err.to_string().contains("backslash"),
        "message should name the cause: {err}"
    );
}

#[skuld::test]
fn rejects_trailing_backslash_in_env_value() {
    let err = resolve_with_env_value("C:\\opt\\rt\\").expect_err("trailing backslash must be rejected");
    assert!(
        err.to_string().contains("backslash"),
        "message should name the cause: {err}"
    );
}

#[skuld::test]
fn accepts_interior_backslashes() {
    // Only a *trailing* backslash continues a line. Rejecting interior ones
    // would make ordinary Windows paths unexpressible.
    resolve_with_env_value("C:\\opt\\rt").expect("interior backslashes are fine");
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
        let err = reject_unemittable(&id, "name", &value).expect_err("noncharacter must be rejected");
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
    reject_unemittable(&id, "name", "FRP — клиент 日本語").expect("ordinary text is fine");
}

#[skuld::test]
fn huge_restart_delay_does_not_overflow() {
    // `as_secs() + 1` on a near-`Duration::MAX` value panics in debug and
    // wraps to a false "rounds to 0s" warning in release.
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart-delay: {}s 1ns\n",
        u64::MAX
    );
    let _ = resolve_yaml(&yaml);
}

#[skuld::test]
fn relative_base_dir_still_yields_absolute_paths() {
    // `DaemonSpec` documents every path as absolute, and `blob::decode`
    // enforces it with `reject_relative_path`. So a relative `base_dir`
    // silently producing relative paths does not merely look untidy — it
    // yields an artifact whose own embedded blob cannot be decoded, which
    // breaks the drift invariant for the common case of `goetia daemon
    // install -f .`.
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    cwd: .\n    logs: logs/frpc.log\n";
    let (specs, _) = resolve(parse_manifest(yaml), Path::new(".")).expect("resolves");
    let spec = &specs[0];

    assert!(
        Path::new(&spec.command[0]).is_absolute(),
        "command[0] must be absolute, got {:?}",
        spec.command[0]
    );
    assert!(
        spec.cwd.as_deref().is_some_and(Path::is_absolute),
        "cwd must be absolute, got {:?}",
        spec.cwd
    );
    assert!(
        spec.logs.as_deref().is_some_and(Path::is_absolute),
        "logs must be absolute, got {:?}",
        spec.logs
    );
}
