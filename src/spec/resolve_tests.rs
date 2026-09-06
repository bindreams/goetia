use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::spec::interpolate;
use crate::spec::vars::Vars;

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
fn resolve_defaults_restart_delay_to_none() {
    let (specs, _) = resolve_yaml("daemons:\n  frpc:\n    command: [bin/frpc]\n").unwrap();
    assert_eq!(specs[0].restart_delay, None);
}

#[skuld::test]
fn resolve_rejects_empty_command() {
    let err = resolve_yaml("daemons:\n  frpc:\n    command: []\n").unwrap_err();
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
        let yaml = format!("daemons:\n  frpc:\n    command: [bin/frpc]\n    restart: {raw}\n");
        let (specs, _) = resolve_yaml(&yaml).unwrap_or_else(|e| panic!("`restart: {raw}` should resolve: {e}"));
        assert_eq!(specs[0].restart, expected, "restart: {raw}");
    }
}

#[skuld::test]
fn resolve_parses_each_type() {
    for (raw, expected) in [("simple", Kind::Simple), ("managed", Kind::Managed)] {
        let yaml = format!("daemons:\n  frpc:\n    command: [bin/frpc]\n    type: {raw}\n");
        let (specs, _) = resolve_yaml(&yaml).unwrap_or_else(|e| panic!("`type: {raw}` should resolve: {e}"));
        assert_eq!(specs[0].kind, expected, "type: {raw}");
    }
}

#[skuld::test]
fn an_unknown_restart_policy_names_the_daemon_and_the_valid_values() {
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    restart: sometimes\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemon `frpc`: field `restart` is `sometimes`; expected one of `never`, `on-failure`, `always`"
    );
}

#[skuld::test]
fn an_unknown_type_names_the_daemon_and_the_valid_values() {
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    type: complicated\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemon `frpc`: field `type` is `complicated`; expected one of `simple`, `managed`"
    );
}

#[skuld::test]
fn a_malformed_restart_delay_names_the_daemon_and_shows_an_example() {
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    restart-delay: not-a-duration\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemon `frpc`: field `restart-delay` is `not-a-duration`, which is not a duration \
         (e.g. `30s`, `1m 30s`): expected number at 0"
    );
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

/// The warning above now runs on a `Duration` parsed by `parse_restart_delay`
/// rather than one `humantime_serde` produced during deserialization — pin
/// that moving the parse did not move the threshold too.
#[skuld::test]
fn a_sub_second_restart_delay_still_warns() {
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    restart-delay: 500ms\n";
    let (specs, warnings) = resolve_yaml(yaml).expect("accepted with a warning, not rejected");
    assert_eq!(specs[0].restart_delay, Some(Duration::from_millis(500)));
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

#[skuld::test]
fn a_whole_second_restart_delay_still_does_not_warn() {
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    restart-delay: 3s\n";
    let (specs, warnings) = resolve_yaml(yaml).unwrap();
    assert_eq!(specs[0].restart_delay, Some(Duration::from_secs(3)));
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
    // Same invariant as `resolve`'s absolutize step: every emitted path must
    // come out absolute even when `base_dir` (e.g. `-f .`) is not.
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    cwd: .\n    logs: logs/frpc.log\n";
    let (specs, _) = resolve(parse_manifest(yaml), Path::new(".")).expect("resolves");
    let spec = &specs[0];

    // Assert *which* directory was used, not merely that some absolute one
    // was: an `absolutize` that ignored its argument and returned a constant
    // would still satisfy `is_absolute()` on every field.
    let cwd = std::env::current_dir().expect("cwd");
    assert_eq!(spec.command[0], cwd.join("bin").join("frpc").to_string_lossy());
    assert_eq!(spec.cwd.as_deref(), Some(cwd.as_path()));
    assert_eq!(spec.logs, Some(cwd.join("logs").join("frpc.log")));
}

#[skuld::test]
fn normalize_preserves_parent_dir_components() {
    // `.` is dropped but `..` is deliberately kept: collapsing `..`
    // lexically is wrong when a component is a symlink, since `a/b/..` is
    // only `a` if `b` is a real directory. Pin the distinction so a future
    // "completion" of the match arm cannot quietly change it.
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    cwd: ../sibling\n";
    let (specs, _) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves");
    let cwd = specs[0].cwd.as_deref().expect("cwd is set");

    assert!(
        cwd.components().any(|c| matches!(c, std::path::Component::ParentDir)),
        "`..` must survive normalization, got {cwd:?}"
    );
}

// The emptiness gate ==================================================================================================

#[skuld::test]
fn rejects_an_empty_user_name() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: \"\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("user.name"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_user_id() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: \"\"\n";
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
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: \"  \"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("user.name"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_a_whitespace_only_user_id() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: \"  \"\n";
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
    let err = resolve_yaml("daemons:\n  frpc:\n    command: [\"\"]\n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("command"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_cwd() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    cwd: \"\"\n";
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
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    cwd: \"  \"\n";
    let (specs, _warnings) = resolve_yaml(yaml).expect("a whitespace-only path is not empty and has no unsafe default");
    assert_eq!(specs[0].cwd, Some(base_dir().join("  ")));
}

#[skuld::test]
fn rejects_an_empty_logs() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    logs: \"\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("logs"), "error should name the field: {msg}");
    assert!(msg.contains("empty"), "error should say why: {msg}");
}

#[skuld::test]
fn rejects_an_empty_env_key() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      \"\": \"value\"\n";
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
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      FOO: \"\"\n";
    let (specs, _warnings) = resolve_yaml(yaml).expect("an empty env value is an ordinary assignment");
    assert_eq!(specs[0].env.get("FOO"), Some(&String::new()));
}

#[skuld::test]
fn accepts_an_empty_name() {
    // An empty systemd `Description=` is harmless — the boundary of the
    // new rule, pinned in the other direction from `rejects_an_empty_cwd`
    // and friends.
    let yaml = "daemons:\n  frpc:\n    name: \"\"\n    command: [/bin/frpc]\n";
    let (specs, _warnings) = resolve_yaml(yaml).expect("an empty name is a harmless empty Description=");
    assert_eq!(specs[0].name, "");
}

#[cfg(windows)]
#[skuld::test]
fn rejects_drive_relative_paths() {
    // `C:bin` is neither absolute nor joinable: `PathBuf::push` truncates
    // whenever the pushed path carries a prefix, so joining it against an
    // absolute base silently discards the base and leaves a relative path.
    // That would emit an artifact whose own blob `blob::decode` rejects.
    let yaml = "daemons:\n  frpc:\n    command: [\"C:bin/frpc.exe\"]\n";
    let err = resolve(parse_manifest(yaml), &base_dir()).expect_err("drive-relative must be rejected");
    assert!(
        err.to_string().contains("drive-relative"),
        "message should name the cause: {err}"
    );
}

// Interpolation =======================================================================================================

/// A fresh directory holding `goetia.yaml`, and `.env` when `env` is given.
fn fixture_dir(yaml: &str, env: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::write(dir.path().join("goetia.yaml"), yaml).expect("fixture manifest should be writable");
    if let Some(env) = env {
        std::fs::write(dir.path().join(".env"), env).expect("fixture .env should be writable");
    }
    dir
}

#[skuld::test]
fn load_substitutes_from_the_env_file_beside_the_manifest() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n",
        Some("NAME=Frpc Tunnel\n"),
    );

    let (specs, _warnings) = load(&dir.path().join("goetia.yaml")).expect("manifest should load");
    assert_eq!(specs[0].name, "Frpc Tunnel");
}

#[skuld::test]
fn load_substitutes_when_the_path_names_a_directory() {
    // The `.env` is found beside the manifest, not beside the *argument* —
    // the two differ for a directory path.
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n",
        Some("NAME=Frpc Tunnel\n"),
    );

    let (specs, _warnings) = load(dir.path()).expect("directory should load");
    assert_eq!(specs[0].name, "Frpc Tunnel");
}

#[skuld::test]
fn load_reports_an_unset_variable_naming_it() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n",
        Some("OTHER=1\n"),
    );

    let err = load(dir.path()).unwrap_err();
    assert!(
        matches!(err, Error::Interpolate { .. }),
        "expected Interpolate, got: {err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("NAME"), "error should name the variable: {text}");
    assert!(
        text.contains("daemons.frpc.name"),
        "error should name the field: {text}"
    );
}

#[skuld::test]
fn load_does_not_read_the_env_file_when_the_manifest_has_no_dollar() {
    // A directory at the `.env` path makes `Vars::load` fail with `Io`, so a
    // manifest that loads anyway demonstrably never read it: an unrelated or
    // root-only-readable `.env` cannot break a manifest that references no
    // variable.
    let dir = fixture_dir("daemons:\n  frpc:\n    command: [/bin/frpc]\n", None);
    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");

    let (specs, _warnings) = load(dir.path()).expect("a dollarless manifest should not read .env");
    assert_eq!(specs.len(), 1);
}

#[skuld::test]
fn load_reads_the_env_file_for_an_escaped_dollar_only() {
    // The same fixture whose only `$` is a `$$` escape *does* read `.env`,
    // pinning `would_substitution_change`'s over-approximation rather than
    // leaving it to drift: one predicate serves this decision and the
    // shape-phase carve-out, and it must stay the safe, wide one.
    let dir = fixture_dir("daemons:\n  frpc:\n    command: [/bin/frpc, $$ARGS]\n", None);
    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");

    let err = load(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "expected Io, got: {err:?}");
}

#[skuld::test]
fn a_dollar_in_a_comment_is_ignored() {
    // The walk runs over the parsed manifest, not the file's text, so a `$`
    // the YAML parser discards never reaches it.
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    # ${NOPE} is a comment, not a reference\n    command: [/bin/frpc]\n",
        None,
    );
    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");

    let (specs, _warnings) = load(dir.path()).expect("a commented-out reference should not read .env");
    assert_eq!(specs.len(), 1);
}

#[skuld::test]
fn load_preserves_authored_scalar_text_alongside_interpolation() {
    // Every leaf stays the text the user wrote, all the way through
    // interpolation: nothing round-trips through a YAML value, so no scalar
    // is renormalised by a number/bool parse it never asked for.
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    name: +5\n    command: [/bin/frpc]\n    env:\n      A: 0.10\n      B: 1e3\n      \
         C: 0x10\n      D: 0o22\n      E: True\n      F: 1.\n      G: 18446744073709551616\n      1.50: plain\n      \
         H: ${TOKEN}\n",
        Some("TOKEN=s3cret\n"),
    );

    let (specs, _warnings) = load(dir.path()).expect("manifest should load");
    let env = &specs[0].env;
    assert_eq!(specs[0].name, "+5");
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
fn load_reports_original_positions() {
    // Interpolation runs on the parsed manifest, after the one and only
    // parse, so a syntax or shape diagnostic still points at the file as
    // written — with or without a `${VAR}` in it.
    let plain = fixture_dir(
        "daemons:\n  frpc:\n    name: literal\n    command: [/bin/frpc]\n    bogus: 1\n",
        None,
    );
    let interpolated = fixture_dir(
        "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n    bogus: 1\n",
        Some("NAME=x\n"),
    );

    let plain_err = load(plain.path()).unwrap_err().to_string();
    let interpolated_err = load(interpolated.path()).unwrap_err().to_string();

    assert!(plain_err.contains("bogus"), "should name the field: {plain_err}");
    assert!(plain_err.contains("line 5"), "should name the line: {plain_err}");
    assert_eq!(plain_err, interpolated_err);
}

#[skuld::test]
fn interpolated_values_pass_through_the_injection_gate() {
    // Substitution happens before `resolve`, so a `.env` value carrying a
    // control character is rejected by the same gate a literal one hits.
    // `.env` cannot express a newline at all (`vars.rs` rejects both the
    // `\n` escape and a multi-line value), so this uses the worst control
    // character it *can* carry.
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n",
        Some("NAME='evil\u{1b}[2Jclear'\n"),
    );

    let err = load(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Invalid { .. }), "expected Invalid, got: {err:?}");
    assert!(
        err.to_string().contains("control character"),
        "expected the control-character gate, got: {err}"
    );
}

#[skuld::test]
fn an_interpolated_value_cannot_create_a_daemon_or_a_field() {
    // A substituted value is one scalar and is never re-parsed as YAML: it
    // cannot add a daemon, rewrite `command`, or introduce a field. Driven
    // through `Vars::from_pairs` rather than a `.env` file because `vars.rs`
    // refuses to produce a newline-bearing value in the first place — this
    // pins the second line of defence behind that refusal.
    let yaml = "daemons:\n  frpc:\n    name: ${EVIL}\n    command: [/bin/frpc]\n";
    let evil = "x\ncommand: [/bin/evil]\nbogus: 1";
    let mut raw = parse_manifest(yaml);
    interpolate::manifest(&mut raw, &Vars::from_pairs(&[("EVIL", evil)])).expect("substitution should succeed");

    assert_eq!(raw.daemons.len(), 1);
    assert_eq!(raw.daemons["frpc"].command, vec!["/bin/frpc"]);
    assert_eq!(raw.daemons["frpc"].name.as_deref(), Some(evil));

    let err = resolve(raw, &base_dir()).unwrap_err();
    assert!(
        err.to_string().contains("control character"),
        "expected the control-character gate, got: {err}"
    );
}
