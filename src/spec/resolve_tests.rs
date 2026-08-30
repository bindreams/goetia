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

    let expected_command0 = base_dir().join("bin/frpc.exe").to_string_lossy().into_owned();
    assert_eq!(spec.command[0], expected_command0);
    assert_eq!(spec.command[1], "-c", "later argv entries are untouched");
    assert_eq!(spec.cwd, Some(base_dir().join(".")));
    assert_eq!(spec.logs, Some(base_dir().join("logs/frpc.log")));
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
    let expected = dir.path().join("bin/frpc").to_string_lossy().into_owned();
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
