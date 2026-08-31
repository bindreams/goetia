use super::*;
use crate::spec::Id;

// Fixtures ============================================================================================================

#[cfg(windows)]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\{p}"))
}
#[cfg(not(windows))]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!("/{p}"))
}

fn managed_spec() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "FRP tunnel client".to_string(),
        command: vec!["frpc".to_string(), "-c".to_string(), "frpc.toml".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: Some(Duration::from_secs(5)),
        logs: None,
        kind: Kind::Managed,
    }
}

fn simple_spec() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "FRP tunnel client".to_string(),
        // Deliberately not named `frpc` (the id) — `simple_kind_executable_is_shim`
        // asserts this string is absent from `arguments`, which would be
        // vacuously true if it happened to equal the id (also `frpc`, and
        // always present as the shim's sole argument).
        command: vec!["frpc-agent".to_string(), "-c".to_string(), "frpc.toml".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Always,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

fn identity() -> Identity {
    Identity {
        user: "svc-frpc".to_string(),
    }
}

fn shim_path() -> PathBuf {
    abs("Program Files/Goetia/goetia-shim.exe")
}

// executable/arguments per Kind =======================================================================================

#[skuld::test]
fn simple_kind_executable_is_shim() {
    let spec = simple_spec();
    let reg = registration(&spec, &identity(), &shim_path());

    assert_eq!(reg.executable, shim_path());
    assert_eq!(reg.arguments, vec![spec.id.as_str().to_string()]);
    assert!(
        !reg.arguments.contains(&spec.command[0]),
        "the spec's own command must not appear in arguments: {:?}",
        reg.arguments
    );
}

#[skuld::test]
fn managed_kind_executable_is_own_command() {
    let spec = managed_spec();
    let reg = registration(&spec, &identity(), &shim_path());

    assert_eq!(reg.executable, PathBuf::from(&spec.command[0]));
    assert_eq!(reg.arguments, spec.command[1..].to_vec());
}

// Account =============================================================================================================

#[skuld::test]
fn root_user_maps_to_local_system() {
    let mut spec = managed_spec();
    spec.user = User::Root;
    let reg = registration(&spec, &identity(), &shim_path());

    assert_eq!(reg.account, None, "User::Root must map to None (LocalSystem)");
}

#[skuld::test]
fn named_user_account_uses_resolved_identity() {
    let mut spec = managed_spec();
    spec.user = User::Name("bindreams".to_string());
    let id = identity();
    let reg = registration(&spec, &id, &shim_path());

    assert_eq!(
        reg.account,
        Some(id.user.clone()),
        "a non-root user must use the pre-resolved Identity, not re-derive one"
    );
}

// argv escaping and round trip ========================================================================================
//
// `registration` stores `executable`/`arguments` unquoted; `windows-service`
// escapes them itself when building `lpBinaryPathName`, and on readback SCM
// gives back one opaque command-line string that has to be split with
// `CommandLineToArgvW` rules to recover an argv. This is validated with a
// hand-written encoder/decoder pair rather than a dependency on the
// `windows-service`/`windows-sys` crates (both Windows-only, so a real
// dependency would stop this module's tests running on Linux/macOS) — see
// the module doc comment and Task 8's brief for why that's the deliberate
// choice here.

/// Mirrors `windows-service::shell_escape::escape` (private to that crate,
/// so reimplemented rather than depended on): quote-wrap an argument if
/// it's empty or contains a quote/space/tab/newline/vtab, doubling any
/// backslash run that immediately precedes a quote character (including
/// one this function itself appends at the end) so the quote can't be
/// swallowed by the backslashes ahead of it.
fn windows_style_escape(s: &str) -> String {
    const ESCAPE_CHARS: [char; 5] = ['"', ' ', '\n', '\t', '\u{000B}'];
    if !s.is_empty() && !s.chars().any(|c| ESCAPE_CHARS.contains(&c)) {
        return s.to_string();
    }

    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');

    let mut i = 0;
    loop {
        let start = i;
        while i < chars.len() && chars[i] == '\\' {
            i += 1;
        }
        let num_slashes = i - start;
        if i < chars.len() && chars[i] == '"' {
            out.push_str(&"\\".repeat(num_slashes * 2 + 1));
            out.push('"');
            i += 1;
        } else if i < chars.len() {
            out.push_str(&"\\".repeat(num_slashes));
            out.push(chars[i]);
            i += 1;
        } else {
            out.push_str(&"\\".repeat(num_slashes * 2));
            break;
        }
    }

    out.push('"');
    out
}

/// Build a command line from `executable` + `arguments` exactly the way
/// `windows-service`'s `RawServiceInfo::new` does: escape each element with
/// the same algorithm, then join with single spaces.
fn windows_style_command_line(executable: &str, arguments: &[&str]) -> String {
    let mut parts = vec![windows_style_escape(executable)];
    parts.extend(arguments.iter().map(|a| windows_style_escape(a)));
    parts.join(" ")
}

/// A `CommandLineToArgvW`-equivalent splitter, implementing the parsing
/// rules Microsoft documents for it: outside a quoted region, whitespace
/// delimits arguments; a run of `n` backslashes followed by a `"` yields
/// `n / 2` literal backslashes, and then either toggles "in quotes" mode
/// (`n` even — the quote is a delimiter, not part of the argument) or
/// contributes one literal `"` to the argument (`n` odd); a backslash run
/// not followed by a `"` is always literal.
fn windows_style_split(cmdline: &str) -> Vec<String> {
    let chars: Vec<char> = cmdline.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut args = Vec::new();

    loop {
        while i < n && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        if i >= n {
            break;
        }

        let mut arg = String::new();
        let mut in_quotes = false;
        while i < n {
            if !in_quotes && matches!(chars[i], ' ' | '\t') {
                break;
            }
            match chars[i] {
                '\\' => {
                    let start = i;
                    while i < n && chars[i] == '\\' {
                        i += 1;
                    }
                    let num_slashes = i - start;
                    if i < n && chars[i] == '"' {
                        arg.push_str(&"\\".repeat(num_slashes / 2));
                        if num_slashes % 2 == 1 {
                            arg.push('"');
                        } else {
                            in_quotes = !in_quotes;
                        }
                        i += 1;
                    } else {
                        arg.push_str(&"\\".repeat(num_slashes));
                    }
                }
                '"' => {
                    in_quotes = !in_quotes;
                    i += 1;
                }
                c => {
                    arg.push(c);
                    i += 1;
                }
            }
        }
        args.push(arg);
    }

    args
}

#[skuld::test]
fn argv_round_trips_through_command_line_argv_w() {
    let cases: Vec<(&str, &str, &[&str])> = vec![
        ("no special characters", "frpc", &["-c", "frpc.toml"]),
        (
            "space in an argument",
            r"C:\Program Files\frp\frpc.exe",
            &["-c", r"C:\Program Files\frp\frpc.toml"],
        ),
        ("embedded quote, no space", "frpc", &[r#"--features="default""#]),
        ("embedded quote and space", "frpc", &[r#"--name="quoted value""#]),
        // The classic CommandLineToArgvW trap: a trailing backslash on an
        // argument that also needs quoting (because it contains a space)
        // must be doubled ahead of the closing quote, or the backslash
        // would escape that quote instead of terminating the argument.
        (
            "trailing backslash on the executable",
            r"C:\some\directory with\spaces\",
            &["argument2"],
        ),
        ("trailing backslash on an argument", "frpc", &[r"C:\output dir\"]),
        // A trailing backslash with no adjacent quote (no space in the
        // argument) needs no escaping at all — included to show the
        // doubling above is conditional on quoting, not unconditional.
        ("unquoted trailing backslash", "frpc", &[r"trailing\\"]),
        ("empty argument", "frpc", &[""]),
    ];

    for (label, executable, arguments) in cases {
        let cmdline = windows_style_command_line(executable, arguments);
        let mut expected = vec![executable.to_string()];
        expected.extend(arguments.iter().map(|a| a.to_string()));

        let recovered = windows_style_split(&cmdline);
        assert_eq!(
            recovered, expected,
            "case `{label}`: round trip through {cmdline:?} did not recover the original argv"
        );
    }
}

// FailureActions ======================================================================================================

#[skuld::test]
fn simple_kind_has_no_failure_actions() {
    let reg = registration(&simple_spec(), &identity(), &shim_path());
    assert_eq!(
        reg.failure_actions, None,
        "the shim supervises the child itself; SCM recovery must stay empty (matching `sc qfailure` for the \
         nssm-based deployment this project is modeled on)"
    );
}

#[skuld::test]
fn managed_kind_sets_on_non_crash_failures() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let fa = reg
        .failure_actions
        .expect("restart: on-failure must configure recovery actions");
    assert!(
        fa.on_non_crash_failures,
        "without SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, a clean non-zero exit never restarts"
    );
}

#[skuld::test]
fn managed_kind_maps_restart_delay_to_action_delay() {
    let mut spec = managed_spec();
    spec.restart_delay = Some(Duration::from_secs(42));
    let reg = registration(&spec, &identity(), &shim_path());

    let fa = reg
        .failure_actions
        .expect("restart: on-failure must configure recovery actions");
    assert_eq!(fa.delay, Duration::from_secs(42));
}

#[skuld::test]
fn managed_kind_default_restart_delay_when_unset() {
    let mut spec = managed_spec();
    spec.restart_delay = None;
    let reg = registration(&spec, &identity(), &shim_path());

    let fa = reg
        .failure_actions
        .expect("restart: on-failure must configure recovery actions");
    assert_eq!(fa.delay, DEFAULT_RESTART_DELAY);
}

#[skuld::test]
fn managed_kind_with_restart_never_has_no_failure_actions() {
    let mut spec = managed_spec();
    spec.restart = Restart::Never;
    let reg = registration(&spec, &identity(), &shim_path());

    assert_eq!(
        reg.failure_actions, None,
        "restart: never must not silently become auto-restart-on-failure on this one platform"
    );
}

#[skuld::test]
fn managed_kind_restart_always_also_sets_failure_actions() {
    let mut spec = managed_spec();
    spec.restart = Restart::Always;
    let reg = registration(&spec, &identity(), &shim_path());

    assert!(
        reg.failure_actions.is_some(),
        "restart: always is approximated by the same recovery action as on-failure on SCM \
         (accepted divergence: SCM recovery never fires after a clean exit)"
    );
}

// extract =============================================================================================================

#[skuld::test]
fn parameters_round_trip_through_extract() {
    let spec = managed_spec();
    let reg = registration(&spec, &identity(), &shim_path());

    let blob = extract(&reg.parameters)
        .expect("a freshly generated registration's parameters must decode")
        .expect("a freshly generated registration's parameters must carry the marker");
    assert_eq!(blob.spec, spec);
    assert_eq!(blob.schema, blob::SCHEMA);
    assert_eq!(blob.version, crate::version());
}

#[skuld::test]
fn render_is_the_generation_invariant() {
    let spec = managed_spec();
    let id = identity();
    let shim = shim_path();

    let original = registration(&spec, &id, &shim);
    let recovered_spec = extract(&original.parameters).unwrap().unwrap().spec;
    let regenerated = registration(&recovered_spec, &id, &shim);

    assert_eq!(
        render(&original),
        render(&regenerated),
        "regenerating from the extracted spec must render identically to the original"
    );
}

#[skuld::test]
fn extract_returns_none_for_foreign_parameters() {
    let parameters = BTreeMap::from([("AppDirectory".to_string(), r"C:\nssm\app".to_string())]);
    assert_eq!(extract(&parameters).unwrap(), None);
}

#[skuld::test]
fn extract_errors_on_corrupt_blob() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_SPEC.to_string(), "not valid base64 !!!".to_string());

    extract(&parameters).expect_err("a corrupt Spec value must not decode");
}

#[skuld::test]
fn extract_errors_on_marker_value_mismatch() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_MARKER.to_string(), "not-goetia".to_string());

    let err = extract(&parameters).expect_err("a Marker present with the wrong value must not be treated as ours");
    assert!(err.to_string().contains(FIELD_MARKER));
}

#[skuld::test]
fn extract_errors_on_missing_schema_field() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let mut parameters = reg.parameters;
    parameters.remove(FIELD_SCHEMA);

    let err = extract(&parameters).expect_err("Marker present without Schema must not silently decode");
    assert!(err.to_string().contains(FIELD_SCHEMA));
}

#[skuld::test]
fn extract_errors_on_non_numeric_schema_field() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_SCHEMA.to_string(), "not-a-number".to_string());

    extract(&parameters).expect_err("a non-numeric Schema must be rejected, not silently mis-decoded");
}

#[skuld::test]
fn extract_errors_on_missing_version_field() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let mut parameters = reg.parameters;
    parameters.remove(FIELD_VERSION);

    let err = extract(&parameters).expect_err("Marker present without Version must not silently decode");
    assert!(err.to_string().contains(FIELD_VERSION));
}

#[skuld::test]
fn extract_errors_on_missing_spec_field() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let mut parameters = reg.parameters;
    parameters.remove(FIELD_SPEC);

    let err = extract(&parameters).expect_err("Marker present without Spec must not silently decode");
    assert!(err.to_string().contains(FIELD_SPEC));
}

// render ==============================================================================================================

#[skuld::test]
fn render_excludes_boot_enablement() {
    let reg = registration(&managed_spec(), &identity(), &shim_path());
    let text = render(&reg);
    let lower = text.to_lowercase();

    for forbidden in ["boot", "auto_start", "demand_start", "startup", "enable"] {
        assert!(
            !lower.contains(forbidden),
            "render() must not mention boot enablement (found `{forbidden}` in:\n{text}\n) — it is a property \
             of the installation, not the service, and excluded from every drift comparison"
        );
    }
}
