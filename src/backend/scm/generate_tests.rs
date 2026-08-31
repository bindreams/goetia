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
        command: vec!["frpc-agent".to_string(), "-c".to_string(), "frpc.toml".to_string()],
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

/// `registration` returns `(ScmRegistration, Vec<Warning>)`; most tests
/// only care about the registration, so this drops the warnings for them.
fn build(spec: &DaemonSpec) -> ScmRegistration {
    registration(spec, &identity(), &shim_path()).0
}

// executable/arguments per Kind =======================================================================================

#[skuld::test]
fn simple_kind_executable_is_shim() {
    let spec = simple_spec();
    let reg = build(&spec);

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
    let reg = build(&spec);

    assert_eq!(reg.executable, PathBuf::from(&spec.command[0]));
    assert_eq!(reg.arguments, spec.command[1..].to_vec());
}

// Account =============================================================================================================

#[skuld::test]
fn root_user_maps_to_local_system() {
    let mut spec = managed_spec();
    spec.user = User::Root;
    let reg = build(&spec);

    assert_eq!(reg.account, None, "User::Root must map to None (LocalSystem)");
}

#[skuld::test]
fn named_user_account_uses_resolved_identity() {
    let mut spec = managed_spec();
    spec.user = User::Name("bindreams".to_string());
    let id = identity();
    let reg = registration(&spec, &id, &shim_path()).0;

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
// the module doc comment for why that's the deliberate choice here. The
// decoder is additionally checked against the real Win32 API on Windows
// (`windows_style_split_matches_real_command_line_to_argv_w`), because a
// hand-rolled model that is only ever compared against itself certifies
// nothing about the real API it claims to describe.

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
/// the same algorithm — argv[0] included — then join with single spaces.
fn windows_style_command_line(executable: &str, arguments: &[&str]) -> String {
    let mut parts = vec![windows_style_escape(executable)];
    parts.extend(arguments.iter().map(|a| windows_style_escape(a)));
    parts.join(" ")
}

/// A `CommandLineToArgvW`-equivalent splitter. Real `CommandLineToArgvW`
/// parses argv[0] (the program name) under different rules than every
/// later argument — see `parse_argv0` — which is the source of the
/// argv[0]-corruption finding this module's doc comment documents; this
/// split is only faithful to the real API *because* it special-cases the
/// first token, confirmed against the real API by
/// `windows_style_split_matches_real_command_line_to_argv_w`.
fn windows_style_split(cmdline: &str) -> Vec<String> {
    let chars: Vec<char> = cmdline.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut args = Vec::new();

    while i < n && matches!(chars[i], ' ' | '\t') {
        i += 1;
    }
    if i >= n {
        return args;
    }
    args.push(parse_argv0(&chars, &mut i));

    loop {
        while i < n && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        if i >= n {
            break;
        }
        args.push(parse_arg(&chars, &mut i));
    }

    args
}

/// Parses the program-name token (argv[0]) under the special rule real
/// `CommandLineToArgvW` applies only to it: a backslash is always literal
/// here — never counted or doubled the way `parse_arg` treats one — and a
/// `"` is a pure, uncounted toggle of "in quotes" mode. This is not
/// documented on the current MSDN page for `CommandLineToArgvW` (which
/// describes only the `parse_arg` rule, as if it applied uniformly);
/// confirmed empirically against the real API by
/// `windows_style_split_matches_real_command_line_to_argv_w`. Getting this
/// wrong — applying `parse_arg`'s rule to argv[0] too — is exactly the bug
/// this function exists to not have: it silently "corrects" a trailing
/// backslash that the real API does not correct, certifying a round trip
/// that does not hold.
fn parse_argv0(chars: &[char], i: &mut usize) -> String {
    let n = chars.len();
    let mut arg = String::new();
    let mut in_quotes = false;
    while *i < n {
        if !in_quotes && matches!(chars[*i], ' ' | '\t') {
            break;
        }
        match chars[*i] {
            '"' => {
                in_quotes = !in_quotes;
                *i += 1;
            }
            c => {
                arg.push(c);
                *i += 1;
            }
        }
    }
    arg
}

/// Parses one of argv[1..] under the rule Microsoft documents for
/// `CommandLineToArgvW`: outside a quoted region, whitespace delimits
/// arguments; a run of `n` backslashes followed by a `"` yields `n / 2`
/// literal backslashes, then either toggles "in quotes" mode (`n` even —
/// the quote is a delimiter, not part of the argument) or contributes one
/// literal `"` to the argument (`n` odd); a backslash run not followed by
/// a `"` is always literal.
fn parse_arg(chars: &[char], i: &mut usize) -> String {
    let n = chars.len();
    let mut arg = String::new();
    let mut in_quotes = false;
    while *i < n {
        if !in_quotes && matches!(chars[*i], ' ' | '\t') {
            break;
        }
        match chars[*i] {
            '\\' => {
                let start = *i;
                while *i < n && chars[*i] == '\\' {
                    *i += 1;
                }
                let num_slashes = *i - start;
                if *i < n && chars[*i] == '"' {
                    arg.push_str(&"\\".repeat(num_slashes / 2));
                    if num_slashes % 2 == 1 {
                        arg.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    *i += 1;
                } else {
                    arg.push_str(&"\\".repeat(num_slashes));
                }
            }
            '"' => {
                in_quotes = !in_quotes;
                *i += 1;
            }
            c => {
                arg.push(c);
                *i += 1;
            }
        }
    }
    arg
}

#[skuld::test]
fn argv_round_trips_through_command_line_argv_w() {
    let cases: Vec<(&str, &str, &[&str])> = vec![
        ("no special characters", "frpc", &["-c", "frpc.toml"]),
        (
            "space in an argument, executable also has a space but no trailing backslash",
            r"C:\Program Files\frp\frpc.exe",
            &["-c", r"C:\Program Files\frp\frpc.toml"],
        ),
        ("embedded quote, no space", "frpc", &[r#"--features="default""#]),
        ("embedded quote and space", "frpc", &[r#"--name="quoted value""#]),
        // The classic CommandLineToArgvW trap: a trailing backslash on an
        // argument that also needs quoting (because it contains a space)
        // must be doubled ahead of the closing quote, or the backslash
        // would escape that quote instead of terminating the argument.
        // This round-trips correctly because arguments (unlike argv[0] —
        // see `executable_trailing_backslash_does_not_round_trip_through_argv0`)
        // use the counted backslash rule on both the write and read side.
        ("trailing backslash on an argument", "frpc", &[r"C:\output dir\"]),
        // A trailing backslash with no adjacent quote (no space in the
        // string) needs no escaping at all, so it round-trips whether it's
        // the executable or an argument — included to show the doubling
        // above is conditional on quoting, not unconditional.
        ("unquoted trailing backslash argument", "frpc", &[r"trailing\\"]),
        (
            "unquoted trailing backslash executable",
            r"C:\no\spaces\here\",
            &["arg"],
        ),
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

#[skuld::test]
fn executable_trailing_backslash_does_not_round_trip_through_argv0() {
    // A documented, real, residual limitation — not silently dropped
    // coverage. Unlike every later argument, real CommandLineToArgvW
    // parses argv[0] with backslashes always literal, never halved back
    // down the way `parse_arg`'s counted rule (which `windows-service`'s
    // escaping assumes will apply symmetrically on read) requires. So an
    // `executable` that needs quoting (a space) *and* ends in a backslash
    // is not recoverable from `lpBinaryPathName` as written: the doubled
    // trailing backslash `windows-service` writes to protect its closing
    // quote is read back as two literal backslashes, not undone to one.
    // See the module doc comment.
    let executable = r"C:\some\directory with\spaces\";
    let cmdline = windows_style_command_line(executable, &["argument2"]);
    let recovered = windows_style_split(&cmdline);

    assert_ne!(
        recovered[0], executable,
        "if this now passes, either windows-service's escaping or the real argv[0] rule changed — \
         re-verify against the real API (see `windows_style_split_matches_real_command_line_to_argv_w`) \
         before deleting this test"
    );
    assert_eq!(
        recovered[0],
        format!("{executable}\\"),
        "the doubled trailing backslash written to protect the closing quote is read back literally, \
         not halved"
    );
}

#[cfg(windows)]
mod real_win32 {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CommandLineToArgvW(lp_cmd_line: *const u16, p_num_args: *mut i32) -> *mut *mut u16;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    /// Splits `cmdline` with the real Win32 `CommandLineToArgvW` — the
    /// authority `windows_style_split` models. Used only by
    /// `windows_style_split_matches_real_command_line_to_argv_w`, so the
    /// hand-rolled splitter is checked against ground truth on the one
    /// platform where that is possible, rather than exclusively against
    /// itself.
    pub fn split(cmdline: &str) -> Vec<String> {
        let wide: Vec<u16> = std::ffi::OsStr::new(cmdline)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut argc: i32 = 0;
        // SAFETY: `wide` is a valid null-terminated UTF-16 string that
        // outlives this call; `argc` is a valid, aligned `i32` out-param.
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
        assert!(!argv.is_null(), "CommandLineToArgvW failed for {cmdline:?}");

        let mut result = Vec::with_capacity(argc.max(0) as usize);
        for idx in 0..argc as isize {
            // SAFETY: `argv` points to `argc` valid pointers to
            // null-terminated UTF-16 strings, per the documented
            // CommandLineToArgvW contract.
            let ptr = unsafe { *argv.offset(idx) };
            let mut len = 0isize;
            // SAFETY: `ptr` is a valid null-terminated UTF-16 string.
            while unsafe { *ptr.offset(len) } != 0 {
                len += 1;
            }
            // SAFETY: `ptr[0..len)` are exactly the UTF-16 code units just counted.
            let slice = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            result.push(OsString::from_wide(slice).to_string_lossy().into_owned());
        }

        // SAFETY: `argv` was allocated by `CommandLineToArgvW` and is
        // freed exactly once, per its documented contract, after every
        // element has already been copied out above.
        unsafe {
            LocalFree(argv as *mut core::ffi::c_void);
        }
        result
    }
}

#[cfg(windows)]
#[skuld::test]
fn windows_style_split_matches_real_command_line_to_argv_w() {
    let cmdlines = [
        windows_style_command_line("frpc", &["-c", "frpc.toml"]),
        windows_style_command_line(
            r"C:\Program Files\frp\frpc.exe",
            &["-c", r"C:\Program Files\frp\frpc.toml"],
        ),
        windows_style_command_line("frpc", &[r#"--features="default""#]),
        windows_style_command_line("frpc", &[r#"--name="quoted value""#]),
        windows_style_command_line("frpc", &[r"C:\output dir\"]),
        windows_style_command_line("frpc", &[r"trailing\\"]),
        windows_style_command_line(r"C:\no\spaces\here\", &["arg"]),
        windows_style_command_line("frpc", &[""]),
        // The residual limitation itself: pinning that the real API and
        // this splitter still *agree* on the corrupted result confirms
        // `windows_style_split` models reality here, even though reality
        // does not round-trip.
        windows_style_command_line(r"C:\some\directory with\spaces\", &["argument2"]),
    ];

    for cmdline in cmdlines {
        assert_eq!(
            windows_style_split(&cmdline),
            real_win32::split(&cmdline),
            "windows_style_split disagrees with the real CommandLineToArgvW for {cmdline:?}"
        );
    }
}

// FailureActions ======================================================================================================

#[skuld::test]
fn simple_kind_has_no_failure_actions() {
    let reg = build(&simple_spec());
    assert_eq!(
        reg.failure_actions, None,
        "the shim supervises the child itself; SCM recovery must stay empty (matching `sc qfailure` for the \
         nssm-based deployment this project is modeled on)"
    );
}

#[skuld::test]
fn managed_kind_sets_on_non_crash_failures() {
    let reg = build(&managed_spec());
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
    let reg = build(&spec);

    let fa = reg
        .failure_actions
        .expect("restart: on-failure must configure recovery actions");
    assert_eq!(fa.delay, Duration::from_secs(42));
}

#[skuld::test]
fn managed_kind_default_restart_delay_when_unset() {
    let mut spec = managed_spec();
    spec.restart_delay = None;
    let reg = build(&spec);

    let fa = reg
        .failure_actions
        .expect("restart: on-failure must configure recovery actions");
    assert_eq!(fa.delay, DEFAULT_RESTART_DELAY);
}

#[skuld::test]
fn managed_kind_with_restart_never_has_no_failure_actions() {
    let mut spec = managed_spec();
    spec.restart = Restart::Never;
    let reg = build(&spec);

    assert_eq!(
        reg.failure_actions, None,
        "restart: never must not silently become auto-restart-on-failure on this one platform"
    );
}

#[skuld::test]
fn managed_kind_restart_always_also_sets_failure_actions() {
    let mut spec = managed_spec();
    spec.restart = Restart::Always;
    let reg = build(&spec);

    assert!(
        reg.failure_actions.is_some(),
        "restart: always is approximated by the same recovery action as on-failure on SCM \
         (accepted divergence: SCM recovery never fires after a clean exit)"
    );
}

#[skuld::test]
fn managed_kind_clamps_absurdly_long_restart_delay_and_warns() {
    let mut spec = managed_spec();
    // One millisecond past what a DWORD of milliseconds (SC_ACTION.Delay)
    // can express.
    spec.restart_delay = Some(MAX_SC_ACTION_DELAY + Duration::from_millis(1));
    let (reg, warnings) = registration(&spec, &identity(), &shim_path());

    let fa = reg
        .failure_actions
        .expect("restart: on-failure must configure recovery actions");
    assert_eq!(
        fa.delay, MAX_SC_ACTION_DELAY,
        "an out-of-range delay must be clamped, not passed through to a value that panics \
         inside windows-service's ServiceAction::to_raw"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.id == spec.id && w.message.contains("restart-delay")),
        "clamping must be reported as a Warning, not done silently: {warnings:?}"
    );
}

#[skuld::test]
fn managed_kind_restart_delay_within_bound_does_not_warn() {
    let mut spec = managed_spec();
    spec.restart_delay = Some(Duration::from_secs(42));
    let (_, warnings) = registration(&spec, &identity(), &shim_path());

    assert!(
        warnings.is_empty(),
        "a restart-delay well within SC_ACTION.Delay's range must not warn: {warnings:?}"
    );
}

// extract =============================================================================================================

#[skuld::test]
fn parameters_round_trip_through_extract() {
    let spec = managed_spec();
    let reg = build(&spec);

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

    let original = registration(&spec, &id, &shim).0;
    let recovered_spec = extract(&original.parameters).unwrap().unwrap().spec;
    let regenerated = registration(&recovered_spec, &id, &shim).0;

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
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_SPEC.to_string(), "not valid base64 !!!".to_string());

    extract(&parameters).expect_err("a corrupt Spec value must not decode");
}

#[skuld::test]
fn extract_errors_on_marker_value_mismatch() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_MARKER.to_string(), "not-goetia".to_string());

    let err = extract(&parameters).expect_err("a Marker present with the wrong value must not be treated as ours");
    assert!(err.to_string().contains(FIELD_MARKER));
}

#[skuld::test]
fn extract_errors_on_missing_schema_field() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.remove(FIELD_SCHEMA);

    let err = extract(&parameters).expect_err("Marker present without Schema must not silently decode");
    assert!(err.to_string().contains(FIELD_SCHEMA));
}

#[skuld::test]
fn extract_errors_on_non_numeric_schema_field() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_SCHEMA.to_string(), "not-a-number".to_string());

    extract(&parameters).expect_err("a non-numeric Schema must be rejected, not silently mis-decoded");
}

#[skuld::test]
fn extract_errors_on_missing_version_field() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.remove(FIELD_VERSION);

    let err = extract(&parameters).expect_err("Marker present without Version must not silently decode");
    assert!(err.to_string().contains(FIELD_VERSION));
}

#[skuld::test]
fn extract_errors_on_missing_spec_field() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.remove(FIELD_SPEC);

    let err = extract(&parameters).expect_err("Marker present without Spec must not silently decode");
    assert!(err.to_string().contains(FIELD_SPEC));
}

#[skuld::test]
fn extract_errors_on_schema_mismatch_with_decoded_blob() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    // A syntactically valid integer that disagrees with what `Spec` itself
    // decodes to — `extract_errors_on_non_numeric_schema_field` only
    // covers the "not even a number" shape, which is a weaker check than
    // the doc comment's "must check out completely" promise.
    parameters.insert(FIELD_SCHEMA.to_string(), "999".to_string());

    let err = extract(&parameters).expect_err("a Schema that disagrees with the decoded blob must be rejected");
    assert!(err.to_string().contains(FIELD_SCHEMA));
}

#[skuld::test]
fn extract_errors_on_version_mismatch_with_decoded_blob() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    parameters.insert(FIELD_VERSION.to_string(), "0.0.0-not-the-real-version".to_string());

    let err = extract(&parameters).expect_err("a Version that disagrees with the decoded blob must be rejected");
    assert!(err.to_string().contains(FIELD_VERSION));
}

#[skuld::test]
fn extract_is_case_insensitive_for_field_names() {
    let reg = build(&managed_spec());
    // Simulate a `Parameters` set written or hand-repaired with different
    // casing than goetia's own writer uses — Windows registry value names
    // are case-insensitive, so this must still be recognized as ours.
    let differently_cased: BTreeMap<String, String> =
        reg.parameters.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();

    let blob = extract(&differently_cased)
        .expect("differently-cased field names must still decode")
        .expect("differently-cased field names must still be recognized as ours");
    assert_eq!(blob.spec, managed_spec());
}

#[skuld::test]
fn extract_errors_on_case_insensitive_field_collision() {
    let reg = build(&managed_spec());
    let mut parameters = reg.parameters;
    // Two keys that name the same registry value case-insensitively, with
    // different content — a plain `BTreeMap<String, String>` can represent
    // this even though a real `Services\<name>\Parameters` key never
    // could, so `extract` must not silently pick one.
    parameters.insert("marker".to_string(), "also-goetia".to_string());

    let err = extract(&parameters).expect_err("two case-differing spellings of the same field must be rejected");
    assert!(err.to_string().contains(FIELD_MARKER));
}

// render ==============================================================================================================

#[skuld::test]
fn render_excludes_boot_enablement() {
    let reg = build(&managed_spec());
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

#[skuld::test]
fn render_escapes_display_name_and_parameter_values_for_injectivity() {
    let mut reg = build(&managed_spec());
    reg.display_name = "line one\nline two".to_string();
    reg.parameters.insert("Extra".to_string(), "a\nb".to_string());

    let text = render(&reg);

    // Readback data (`ServiceConfig::display_name`, an arbitrary
    // `Parameters` value) is not covered by the injection gate that keeps
    // goetia-authored strings newline-free, so `render` must escape it
    // itself rather than let it forge extra lines indistinguishable from
    // real fields.
    assert!(
        text.contains("DisplayName: \"line one\\nline two\""),
        "display_name's newline must be escaped, not literal:\n{text}"
    );
    assert!(
        text.contains("  Extra: \"a\\nb\""),
        "a Parameters value's newline must be escaped, not literal:\n{text}"
    );
    assert!(
        !text.contains("line one\nline two"),
        "the literal newline must never appear unescaped in the rendered text:\n{text}"
    );
}

#[skuld::test]
fn render_output_format_is_pinned() {
    let reg = ScmRegistration {
        name: "frpc".to_string(),
        display_name: "FRP tunnel client".to_string(),
        executable: PathBuf::from(r"C:\opt\frp\frpc.exe"),
        arguments: vec!["-c".to_string(), "frpc.toml".to_string()],
        account: None,
        failure_actions: Some(FailureActions {
            delay: Duration::from_secs(1),
            reset_period: Duration::from_secs(86_400),
            on_non_crash_failures: true,
        }),
        parameters: BTreeMap::from([
            ("Marker".to_string(), "goetia".to_string()),
            ("Schema".to_string(), "1".to_string()),
        ]),
    };

    // Every line render() itself controls (labels, order, indentation,
    // `{:?}` quoting) is pinned literally; the two duration substrings
    // are computed through the same `humantime::format_duration` call
    // render() uses, since that formatting is humantime's contract, not
    // render()'s.
    let delay_text = humantime::format_duration(Duration::from_secs(1));
    let reset_period_text = humantime::format_duration(Duration::from_secs(86_400));
    let expected = [
        "Name: frpc".to_string(),
        "DisplayName: \"FRP tunnel client\"".to_string(),
        "Account: \"LocalSystem\"".to_string(),
        format!("Executable: {:?}", PathBuf::from(r"C:\opt\frp\frpc.exe")),
        "Arguments:".to_string(),
        "  - \"-c\"".to_string(),
        "  - \"frpc.toml\"".to_string(),
        "FailureActions:".to_string(),
        format!("  Delay: {delay_text}"),
        format!("  ResetPeriod: {reset_period_text}"),
        "  OnNonCrashFailures: true".to_string(),
        "Parameters:".to_string(),
        "  Marker: \"goetia\"".to_string(),
        "  Schema: \"1\"".to_string(),
    ]
    .into_iter()
    .map(|line| format!("{line}\n"))
    .collect::<String>();

    assert_eq!(render(&reg), expected);
}
