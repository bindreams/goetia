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
    assert_eq!(warnings[0].id.as_ref().map(Id::as_str), Some("svc"));
    assert!(warnings[0].message.contains("cwd") || warnings[0].message.contains("logs"));
}

#[skuld::test]
fn managed_on_windows_warning_names_the_argument_consequence() {
    let yaml = r#"
daemons:
  svc:
    command: ["bin/svc.exe"]
    cwd: "."
    logs: "logs/svc.log"
    type: managed
"#;
    let (_specs, warnings) = resolve_yaml(yaml).expect("accepted with a warning, not rejected");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("System32"));
    assert!(warnings[0].message.contains("argument"));
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
fn huge_restart_delay_saturates_instead_of_overflowing() {
    // `as_secs() + 1` on a near-`Duration::MAX` value panics in debug and
    // wraps to a false "rounds to 0s" warning in release. Assert the
    // saturated outcome, not merely the absence of a panic: a wrap is
    // silent in release, and a debug panic is the only half a
    // result-discarding test could ever catch.
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart-delay: {}s 1ns\n",
        u64::MAX
    );
    let (specs, warnings) = resolve_yaml(&yaml).expect("a near-`Duration::MAX` delay is accepted, with a warning");

    assert_eq!(specs[0].restart_delay, Some(Duration::new(u64::MAX, 1)));
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
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc]\n    cwd: .\n    logs: logs/frpc.log\n";
    let (specs, _) = resolve(parse_manifest(yaml), Path::new(".")).expect("resolves");
    let spec = &specs[0];

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
fn an_authored_base_dir_is_not_canonicalised() {
    // The guard on the deduced/authored line: an absolute `-f` pointing
    // through a symlinked directory must resolve to the path as written,
    // not to the link target. No fallback, no conditional assertion — if
    // the symlink cannot be created, this must fail loudly rather than
    // quietly assert something else.
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).expect("real dir should be creatable");
    let link = dir.path().join("link");

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
    let yaml = "daemons:\n  frpc:\n    command: [\"C:bin/frpc.exe\"]\n";
    let (specs, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("drive-relative command resolves");

    assert!(Path::new(&specs[0].command[0]).is_absolute());
    assert_ne!(
        specs[0].command[0],
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
    let yaml = "daemons:\n  frpc:\n    command: [\"C:bin/frpc.exe\"]\n";
    let (specs, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves as an ordinary filename");
    assert_eq!(specs[0].command[0], base_dir().join("C:bin/frpc.exe").to_string_lossy());
    assert!(warnings.is_empty());
}

#[cfg(windows)]
#[skuld::test]
fn a_manifest_relative_path_beside_a_drive_relative_one_still_anchors_to_the_manifest() {
    // Pins that the fallback fires per path and does not capture the
    // ordinary relative `cwd` sitting next to a drive-relative `command`.
    let yaml = "daemons:\n  frpc:\n    command: [\"C:bin/frpc.exe\"]\n    cwd: data\n";
    let (specs, _warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves");
    assert_eq!(specs[0].cwd, Some(base_dir().join("data")));
}

#[cfg(windows)]
#[skuld::test]
fn drive_relative_resolution_collapses_dot_dot_the_windows_way() {
    // The one exception to `normalize_preserves_parent_dir_components`:
    // `GetFullPathNameW` collapses `..` lexically before the filesystem ever
    // sees the path, so a drive-relative path goetia resolved differently
    // from every other Windows tool would be the bug, not the other way
    // around.
    let yaml = r"daemons:
  frpc:
    command: ['C:a\..\b']
";
    let (specs, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("drive-relative path resolves");
    let resolved = Path::new(&specs[0].command[0]);

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
    let yaml = "daemons:\n  frpc:\n    command: [bin/frpc.exe]\n";
    let (_specs, warnings) = resolve(parse_manifest(yaml), Path::new("C:proj")).expect("-f C:proj resolves");
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
    let yaml = r"daemons:
  frpc:
    command: ['\\?\C:']
";
    let (specs, warnings) = resolve(parse_manifest(yaml), &base_dir())
        .expect("a VerbatimDisk prefix carries an implicit root and resolves");
    assert_eq!(
        specs[0].command[0], r"\\?\C:",
        "a verbatim prefix must pass through resolve unchanged"
    );
    assert!(
        warnings.is_empty(),
        "a VerbatimDisk prefix is not drive-relative and must not warn: {warnings:?}"
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
fn a_dollar_inside_a_substituted_value_survives_resolve_unchanged() {
    // A `.env` value is fully literal, so a `$` in one is ordinary content —
    // passwords and connection strings carry them routinely. Substituted
    // text is never rescanned, so that `$` must reach the resolved spec
    // exactly as written. Pinning it here because the scanner rejects a
    // bare `$` in *manifest* text: anything that interpolated a second time
    // would turn a legal secret into
    // "`$` at byte N is not part of `${...}`" and fail the whole load.
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      PASSWORD: ${SECRET}\n",
        Some("SECRET=abc$def\n"),
    );
    let manifest = dir.path().join("goetia.yaml");
    let (specs, _warnings) = load(&manifest).expect("a literal `$` in a .env value is legal");
    assert_eq!(specs[0].env.get("PASSWORD").map(String::as_str), Some("abc$def"));
}

#[skuld::test]
fn resolve_substitutes_rather_than_letting_a_reference_through() {
    // `resolve` is public and takes a public `RawManifest`, so it is
    // reachable without `load`. Interpolation therefore lives inside it,
    // not beside it: a literal `${MISSING}` reaching a generated unit would
    // be expanded by systemd itself, silently, to an empty string.
    let yaml = "daemons:\n  frpc:\n    name: ${ALSO_MISSING}\n    command: [/bin/frpc, \"--flag=${MISSING}\"]\n";

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
fn resolve_reads_the_env_file_beside_its_base_dir() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n",
        Some("NAME=Frpc Tunnel\n"),
    );
    let yaml = "daemons:\n  frpc:\n    name: ${NAME}\n    command: [/bin/frpc]\n";

    let (specs, _warnings) = resolve(parse_manifest(yaml), dir.path()).expect("manifest should resolve");
    assert_eq!(specs[0].name, "Frpc Tunnel");
}

#[skuld::test]
fn resolve_does_not_read_the_env_file_when_the_manifest_has_no_dollar() {
    // The gate moved with the substitution step, so it is `resolve`'s to
    // hold now: a directory at the `.env` path would make `Vars::load` fail
    // with `Io`, and a dollarless manifest resolving anyway proves it was
    // never read.
    let dir = fixture_dir("daemons:\n  frpc:\n    command: [/bin/frpc]\n", None);
    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n";

    let (specs, _warnings) = resolve(parse_manifest(yaml), dir.path()).expect("a dollarless manifest should resolve");
    assert_eq!(specs.len(), 1);
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
    // control character is rejected by the same gate a literal one hits —
    // on every leaf that reaches a generated artifact, not just `name`.
    // `.env` cannot express a newline at all (`vars.rs` rejects both the
    // `\n` escape and a multi-line value), so this uses the worst control
    // character it *can* carry.
    // Each row names the field the error must blame. Asserting only
    // "control character" would let a row pass on a *different* leaf's
    // rejection than the one it claims to exercise.
    let leaves = [
        ("name", "    name: ${EVIL}\n    command: [/bin/frpc]\n"),
        ("command", "    command: [/bin/frpc, \"${EVIL}\"]\n"),
        ("cwd", "    command: [/bin/frpc]\n    cwd: ${EVIL}\n"),
        ("logs", "    command: [/bin/frpc]\n    logs: ${EVIL}\n"),
        ("env[LOG]", "    command: [/bin/frpc]\n    env:\n      LOG: ${EVIL}\n"),
        ("user.name", "    command: [/bin/frpc]\n    user: ${EVIL}\n"),
        // The struct form too: it is the syntax the `RawUser` split
        // introduced, and it reaches the gate by a different arm.
        (
            "user.name",
            "    command: [/bin/frpc]\n    user:\n      name: ${EVIL}\n",
        ),
    ];

    for (leaf, body) in leaves {
        let dir = fixture_dir(
            &format!("daemons:\n  frpc:\n{body}"),
            Some("EVIL='evil\u{1b}[2Jclear'\n"),
        );

        let err = load(dir.path()).unwrap_err();
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
fn an_interpolated_value_cannot_create_a_daemon_or_a_field() {
    // A substituted value is one scalar and is never re-parsed as YAML: it
    // cannot add a daemon, rewrite `command`, or introduce a field. Driven
    // through `Vars::from_pairs` rather than a `.env` file because `vars.rs`
    // refuses to produce a newline-bearing value in the first place — this
    // pins the second line of defence behind that refusal.
    let yaml = "daemons:\n  frpc:\n    name: ${EVIL}\n    command: [/bin/frpc]\n";
    let evil = "x\ncommand: [/bin/evil]\nbogus: 1";
    let mut raw = parse_manifest(yaml);
    let entry = raw.daemons.get_mut("frpc").expect("fixture declares frpc");
    interpolate::spec(entry, "daemons.frpc", &Vars::from_pairs(&[("EVIL", evil)]))
        .expect("substitution should succeed");

    assert_eq!(raw.daemons.len(), 1);
    assert_eq!(
        raw.daemons["frpc"].command.as_deref(),
        Some(&["/bin/frpc".to_string()][..])
    );
    assert_eq!(raw.daemons["frpc"].name.as_deref(), Some(evil));

    // `resolve` runs the injection gate on the substituted value once, not
    // that a second substitution is a no-op, inside `resolve`, per daemon,
    // on the merged native spec: `evil` holds no `$`, so `resolve`'s own
    // substitution pass leaves it untouched and the injection gate is what
    // actually rejects it.
    let err = resolve(raw, &base_dir()).unwrap_err();
    assert!(
        err.to_string().contains("control character"),
        "expected the control-character gate, got: {err}"
    );
}

// backend-specific overrides ==========================================================================================

/// A backend that is not this host's native one. `Backend::ALL` always has
/// at least two such entries (`Backend::native()` names at most one), so the
/// first match is deterministic and always exists.
fn non_native_backend() -> Backend {
    Backend::ALL
        .into_iter()
        .find(|&b| Some(b) != Backend::native())
        .expect("ALL has 3 entries; native() names at most 1")
}

/// A `user:` value `Backend::error` rejects for `backend`, on any host — so
/// a test can aim a per-backend verdict at whichever backend it is naming
/// as native.
fn user_rejected_by(backend: Backend) -> &'static str {
    match backend {
        // A numeric uid is never a Windows account.
        Backend::Scm => "{id: 1000}",
        // A SID is never a POSIX account.
        Backend::Systemd | Backend::Launchd => "{id: \"S-1-5-18\"}",
    }
}

/// `resolve`, with `native` named rather than detected — the seam that puts
/// every host's native arm within reach of every other host's test runner.
fn resolve_yaml_as(native: Backend, yaml: &str) -> Result<(Vec<DaemonSpec>, Vec<Warning>), Error> {
    resolve_as(parse_manifest(yaml), &base_dir(), Some(native))
}

#[skuld::test]
fn a_native_backend_verdict_names_the_override_that_caused_it() {
    // Not `.env`: step 4 skips the native backend, so step 5 is the first
    // pass to apply a per-backend rule to the native spec — there is no
    // earlier pass whose success would make substitution the only suspect,
    // and this manifest carries no `$` and sits beside no `.env` file.
    for native in Backend::ALL {
        let yaml = format!(
            "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {native}:\n        user: {}\n",
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
    let Some(native) = Backend::native() else {
        return; // No native backend on this host: nothing to assert.
    };
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: root\n    backend-specific:\n      {native}:\n        \
         user: bindreams\n"
    );
    let (specs, _warnings) = resolve_yaml(&yaml).expect("resolves");
    assert_eq!(specs[0].user, User::Name("bindreams".to_string()));
}

#[skuld::test]
fn resolve_ignores_a_non_native_backends_override() {
    let backend = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: root\n    backend-specific:\n      {backend}:\n        \
         user: bindreams\n"
    );
    let (specs, _warnings) = resolve_yaml(&yaml).expect("resolves");
    assert_eq!(
        specs[0].user,
        User::Root,
        "a non-native override must not change the resolved spec"
    );
}

#[skuld::test]
fn an_invalid_non_native_override_fails_the_whole_manifest() {
    let backend = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        \
         user: \"evil\\nUser=0\"\n"
    );
    assert!(resolve_yaml(&yaml).is_err());
}

#[skuld::test]
fn an_invalid_native_override_fails_the_whole_manifest() {
    let Some(native) = Backend::native() else {
        return;
    };
    // The pass whose spec is actually installed, and the one whose failure
    // has a production consequence.
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {native}:\n        \
         user: \"evil\\nUser=0\"\n"
    );
    assert!(resolve_yaml(&yaml).is_err());
}

#[skuld::test]
fn an_override_error_names_the_backend_that_caused_it() {
    let non_native = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {non_native}:\n        \
         user: \"evil\\nUser=0\"\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains(&format!("backend-specific.{non_native}")),
        "non-native: {err}"
    );

    if let Some(native) = Backend::native() {
        let yaml = format!(
            "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {native}:\n        \
             user: \"evil\\nUser=0\"\n"
        );
        let err = resolve_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains(&format!("backend-specific.{native}")),
            "native: {err}"
        );
    }
}

#[skuld::test]
fn a_base_spec_error_is_not_attributed_to_a_backend() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    name: \"evil\\nExecStart=/bin/evil\"\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        !err.to_string().contains("backend-specific"),
        "a base-spec error must not be attributed to a backend: {err}"
    );
}

#[skuld::test]
fn windows_style_absolute_paths_in_an_scm_override_are_accepted_on_any_host() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      scm:\n        \
                command: [\"C:\\\\Program Files\\\\frpc\\\\frpc.exe\"]\n";
    resolve_yaml(yaml).expect("a Windows-style absolute path under a non-native `scm:` override must resolve");
}

#[skuld::test]
fn dedup_keeps_first_occurrence_and_preserves_order() {
    let id = Id::try_from("frpc").unwrap();
    let a = Warning {
        id: Some(id.clone()),
        message: "a".to_string(),
    };
    let b = Warning {
        id: Some(id.clone()),
        message: "b".to_string(),
    };
    let warnings = vec![a.clone(), b.clone(), a.clone()];
    assert_eq!(dedup_warnings(warnings), vec![a, b]);
}

#[skuld::test]
fn dedup_does_not_merge_warnings_differing_only_by_id() {
    let a = Id::try_from("frpc").unwrap();
    let b = Id::try_from("websocat").unwrap();
    let w1 = Warning {
        id: Some(a),
        message: "same text".to_string(),
    };
    let w2 = Warning {
        id: Some(b),
        message: "same text".to_string(),
    };
    let warnings = vec![w1.clone(), w2.clone()];
    assert_eq!(dedup_warnings(warnings), vec![w1, w2]);
}

#[cfg(windows)]
#[skuld::test]
fn a_base_drive_relative_path_warns_once_not_once_per_backend_pass() {
    let yaml = "daemons:\n  frpc:\n    command: [\"C:bin/frpc.exe\"]\n";
    let (_specs, warnings) = resolve(parse_manifest(yaml), &base_dir()).expect("resolves");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[skuld::test]
fn windows_divergence_warning_follows_the_scm_merged_spec() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    type: managed\n    cwd: .\n    backend-specific:\n      \
                scm:\n        type: simple\n";
    let (_specs, warnings) = resolve_yaml(yaml).expect("resolves");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[skuld::test]
fn windows_divergence_warning_fires_for_an_scm_only_managed_type() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    type: simple\n    cwd: .\n    backend-specific:\n      \
                scm:\n        type: managed\n";
    let (_specs, warnings) = resolve_yaml(yaml).expect("resolves");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[skuld::test]
fn sub_second_delay_warning_follows_the_launchd_merged_spec() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart-delay: 500ms\n    backend-specific:\n      \
                launchd:\n        restart-delay: 2s\n";
    let (_specs, warnings) = resolve_yaml(yaml).expect("resolves");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[skuld::test]
fn each_warning_fires_exactly_once_across_every_backend_pass() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart-delay: 500ms\n";
    let (_specs, warnings) = resolve_yaml(yaml).expect("resolves");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[skuld::test]
fn an_override_on_every_backend_resolves() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      launchd:\n        \
                user: bindreams\n      scm:\n        user: bindreams\n      systemd:\n        user: bindreams\n";
    resolve_yaml(yaml).expect("resolves");
}

#[skuld::test]
fn a_command_only_under_the_native_backend_installs() {
    let Some(native) = Backend::native() else {
        return;
    };
    let yaml = format!("daemons:\n  frpc:\n    backend-specific:\n      {native}:\n        command: [/bin/frpc]\n");
    let (specs, _warnings) = resolve_yaml(&yaml).expect("resolves");
    assert!(!specs[0].command.is_empty());
}

#[skuld::test]
fn a_command_missing_for_the_native_backend_is_an_error() {
    let Some(native) = Backend::native() else {
        return;
    };
    let yaml = "daemons:\n  frpc:\n    name: frpc\n";
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
    let Some(native) = Backend::native() else {
        return;
    };
    // A command present only for the native backend: completeness is
    // per-install, not per-backend, and a Windows-less manifest is usable
    // on Linux.
    let yaml = format!("daemons:\n  frpc:\n    backend-specific:\n      {native}:\n        command: [/bin/frpc]\n");
    resolve_yaml(&yaml).expect("resolves");
}

#[skuld::test]
fn an_empty_command_is_rejected_like_a_missing_one() {
    let Some(native) = Backend::native() else {
        return;
    };
    let yaml = format!("daemons:\n  frpc:\n    backend-specific:\n      {native}:\n        command: []\n");
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(err.to_string().contains("command"));
}

#[skuld::test]
fn an_empty_user_name_is_rejected_on_every_backend() {
    // Guards the seam: `resolve_shape` must carry `reject_blank` across from
    // `resolve_one`. See `reject_blank`'s doc comment (`resolve.rs`) for why
    // an empty account name is dangerous rather than merely odd.
    assert!(resolve_yaml("daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: \"\"\n").is_err());
    assert!(resolve_yaml("daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: \"\"\n").is_err());
}

#[skuld::test]
fn a_numeric_uid_under_scm_is_rejected_on_every_host() {
    // The finding that showed the architecture claim was false as first
    // written: a per-backend rejection applied to `merged_for` unmasked by
    // `Supplied` would reject far more than this.
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      scm:\n        \
                user:\n          id: 1000\n";
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
                "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        \
                 user: \"{}\"\n",
                user.replace('\\', "\\\\")
            );
            assert!(resolve_yaml(&yaml).is_err(), "`{user}` under `{backend}`");
        }
    }
}

#[skuld::test]
fn a_sid_under_a_posix_backend_is_rejected_at_resolve_time() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      systemd:\n        \
                user:\n          id: \"S-1-5-19\"\n";
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
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: 1000\n";
    let (specs, _warnings) = resolve_yaml(yaml).expect("a base numeric uid must resolve on every host");
    assert_eq!(specs[0].user, User::Id(AccountId::Uid(1000)));
}

#[skuld::test]
fn a_base_windows_account_resolves_on_every_host() {
    // The symmetric direction: without `Supplied`, these fail on Windows,
    // where `Systemd.error`/`Launchd.error` sweep the base spec. Neither
    // half is reachable from a Linux runner, which is exactly why it is
    // written here rather than left to be discovered on CI.
    let yaml_sid = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: \"S-1-5-19\"\n";
    resolve_yaml(yaml_sid).expect("a base SID must resolve on every host");

    let yaml_name = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: LocalService\n";
    resolve_yaml(yaml_name).expect("a base Windows built-in name must resolve on every host");
}

#[skuld::test]
fn a_base_named_system_account_resolves_under_every_backend() {
    // `system` is a legal POSIX username, so `windows_only_account` must not
    // recognise it as Windows-only.
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: system\n";
    resolve_yaml(yaml).expect("a base `system` account must resolve under every backend");

    let yaml_override = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      systemd:\n        \
                         user: system\n";
    resolve_yaml(yaml_override).expect("`system` under a systemd override must resolve: it is a legal POSIX username");
}

#[skuld::test]
fn an_over_long_restart_delay_warns_on_every_host() {
    // All three keys are load-bearing: the advisory is unreachable for
    // `Kind::Simple` and for `Restart::Never`.
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    type: managed\n    restart: on-failure\n    restart-delay: 100d\n";
    let (_specs, warnings) = resolve_yaml(yaml).expect("resolves, with a warning");
    assert!(
        warnings.iter().any(|w| w.message.contains("restart-delay")),
        "expected the SCM clamp advisory on every host: {warnings:?}"
    );
}

#[skuld::test]
fn an_over_long_restart_delay_with_no_type_does_not_warn() {
    // The unit-level twin of `huge_restart_delay_saturates_instead_of_overflowing`,
    // which asserts exactly one warning (the sub-second one) for a
    // near-`Duration::MAX` delay written the same way.
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart-delay: {}s 1ns\n",
        u64::MAX
    );
    let (_specs, warnings) = resolve_yaml(&yaml).expect("resolves");
    assert!(
        !warnings.iter().any(|w| w.message.contains("SC_ACTION")),
        "the SCM clamp advisory is unreachable with no `type:`/`restart:`: {warnings:?}"
    );
}

#[cfg(windows)]
#[skuld::test]
fn a_templated_drive_relative_path_warns_once_quoting_the_substituted_path() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    command: [\"C:${SUB}\"]\n",
        Some("SUB=bin/frpc.exe\n"),
    );
    let (_specs, warnings) = load(&dir.path().join("goetia.yaml")).expect("resolves");
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
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        name: \"$HOME\"\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(err.to_string().contains("not part of"), "{err}");
}

#[skuld::test]
fn a_base_grammar_error_is_rejected_even_when_every_backend_overrides_the_field() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    name: \"a$b\"\n    backend-specific:\n      \
                launchd:\n        name: ok1\n      scm:\n        name: ok2\n      systemd:\n        name: ok3\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(err.to_string().contains("not part of"), "{err}");
}

#[skuld::test]
fn a_bare_dollar_in_an_env_value_survives_a_substituted_pass() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      X: ${S}\n",
        Some("S=ab$cd\n"),
    );
    let (specs, _warnings) = load(&dir.path().join("goetia.yaml")).expect("resolves");
    assert_eq!(specs[0].env["X"], "ab$cd");
}

#[skuld::test]
fn a_base_value_must_pass_the_shape_gate_even_when_every_backend_overrides_it() {
    // Deliberate: the base pass shape-checks every authored value, even one
    // no backend ends up using, so a dead value cannot sit in the manifest
    // as a trap for whoever later deletes an override. `C:\ProgramData\frpc`
    // names the same directory as `C:\ProgramData\frpc\`, so the remedy is
    // lossless: write the value under the backends that need it instead.
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    cwd: \"C:\\\\ProgramData\\\\frpc\\\\\"\n    \
                backend-specific:\n      launchd:\n        cwd: /var/frpc\n      scm:\n        cwd: /var/frpc\n      \
                systemd:\n        cwd: /var/frpc\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(err.to_string().contains("backslash"), "{err}");
}

// The `$`-skip rule for `restart`/`type`/`restart-delay` ==============================================================

#[skuld::test]
fn a_templated_restart_resolves() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart: ${R}\n",
        Some("R=always\n"),
    );
    let (specs, _warnings) = load(&dir.path().join("goetia.yaml")).expect("resolves");
    assert_eq!(specs[0].restart, Restart::Always);
}

#[skuld::test]
fn a_templated_type_resolves() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    type: ${T}\n",
        Some("T=managed\n"),
    );
    let (specs, _warnings) = load(&dir.path().join("goetia.yaml")).expect("resolves");
    assert_eq!(specs[0].kind, Kind::Managed);
}

#[skuld::test]
fn a_templated_restart_delay_resolves() {
    let dir = fixture_dir(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart-delay: ${D}\n",
        Some("D=2s\n"),
    );
    let (specs, _warnings) = load(&dir.path().join("goetia.yaml")).expect("resolves");
    assert_eq!(specs[0].restart_delay, Some(Duration::from_secs(2)));
}

#[skuld::test]
fn a_templated_restart_in_the_native_override_resolves() {
    let Some(native) = Backend::native() else {
        return;
    };
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {native}:\n        restart: ${{R}}\n"
    );
    let dir = fixture_dir(&yaml, Some("R=always\n"));
    let (specs, _warnings) = load(&dir.path().join("goetia.yaml")).expect("resolves");
    assert_eq!(specs[0].restart, Restart::Always);
}

#[skuld::test]
fn a_literal_bad_restart_in_a_non_native_override_is_still_rejected() {
    let backend = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        restart: bogus\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains(&format!("backend-specific.{backend}")),
        "{err}"
    );
}

#[skuld::test]
fn a_templated_restart_in_a_non_native_override_is_not_parsed_here() {
    let backend = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        restart: ${{R}}\n"
    );
    resolve_yaml(&yaml)
        .expect("an unresolvable templated value under a non-native backend must be unchecked, not guessed at");
}

#[skuld::test]
fn an_escaped_dollar_in_an_enum_field_is_deferred_and_then_rejected() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    restart: $$never\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        err.to_string().contains("field `restart` is `$never`"),
        "the value must be deferred (it carries a `$`) and then rejected once substitution changes nothing: {err}"
    );
}

#[skuld::test]
fn a_variable_used_only_in_a_non_native_override_does_not_read_the_env_file() {
    let backend = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        restart: ${{R}}\n"
    );
    let dir = fixture_dir(&yaml, None);
    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");
    let (specs, _warnings) =
        load(&dir.path().join("goetia.yaml")).expect("must not read .env for a value it will never substitute");
    assert_eq!(specs.len(), 1);
}

// The relocated key-position rules ====================================================================================

#[skuld::test]
fn a_dollar_in_a_daemon_id_is_rejected_by_resolve() {
    let yaml = "daemons:\n  ${ID}:\n    command: [/bin/frpc]\n";
    let err = resolve_yaml(yaml).unwrap_err();
    assert!(
        err.to_string().contains("a daemon id cannot be interpolated"),
        "expected the id-is-a-key rejection, not the id-pattern one: {err}"
    );
    assert!(
        !err.to_string().contains("does not match the required pattern"),
        "the id-is-a-key rejection must run first: {err}"
    );
}

#[skuld::test]
fn a_dollar_in_an_env_name_is_rejected_under_every_backend() {
    let backend = non_native_backend();
    let yaml = format!(
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        \
         env:\n          ${{K}}: v\n"
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
        "daemons:\n  frpc:\n    command: [/bin/frpc]\n    backend-specific:\n      {backend}:\n        \
         user:\n          id: \"${{S}}\"\n"
    );
    let err = resolve_yaml(&yaml).unwrap_err();
    assert!(err.to_string().contains(interpolate::USER_ID_MESSAGE), "{err}");
}
