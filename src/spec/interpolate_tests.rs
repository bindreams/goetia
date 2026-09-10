use super::{check_spec_grammar, reject_interpolated_id, scalar, spec, spec_would_change, would_substitution_change};
use crate::error::Error;
use crate::spec::resolve::resolve_user;
use crate::spec::vars::Vars;
use crate::spec::{AccountId, RawManifest, RawSpec, RawUser, User};

/// Substitute `input` against `vars` at a fixed test path.
fn sub(input: &str, vars: &Vars) -> Result<String, Error> {
    scalar(input, "daemons.frpc.env[\"LOG\"]", vars)
}

// Basic substitution ==================================================================================================

#[skuld::test]
fn substitutes_a_braced_reference() {
    let vars = Vars::from_pairs(&[("NAME", "value")]);
    assert_eq!(sub("${NAME}", &vars).unwrap(), "value");
}

#[skuld::test]
fn substitutes_several_references_in_one_scalar() {
    let vars = Vars::from_pairs(&[("A", "1"), ("B", "2")]);
    assert_eq!(sub("${A}${B}", &vars).unwrap(), "12");
    assert_eq!(sub("x-${A}-y-${B}-z", &vars).unwrap(), "x-1-y-2-z");
}

#[skuld::test]
fn a_defined_but_empty_variable_without_a_default_is_the_empty_string() {
    // Distinct from `an_unset_variable_without_a_default_names_the_variable`:
    // an empty value is still a value, so no default is needed and no error
    // is raised — only a truly unset variable errors.
    let vars = Vars::from_pairs(&[("NAME", "")]);
    assert_eq!(sub("${NAME}", &vars).unwrap(), "");
}

#[skuld::test]
fn text_without_a_dollar_is_returned_unchanged() {
    let vars = Vars::empty();
    assert_eq!(
        sub("plain text, no dollars here", &vars).unwrap(),
        "plain text, no dollars here"
    );
}

// Defaults ============================================================================================================

#[skuld::test]
fn a_default_applies_when_the_variable_is_unset() {
    let vars = Vars::empty();
    assert_eq!(sub("${NAME:-fallback}", &vars).unwrap(), "fallback");
}

#[skuld::test]
fn a_default_applies_when_the_variable_is_empty() {
    let vars = Vars::from_pairs(&[("NAME", "")]);
    assert_eq!(sub("${NAME:-fallback}", &vars).unwrap(), "fallback");
}

#[skuld::test]
fn a_default_is_ignored_when_the_variable_is_set() {
    let vars = Vars::from_pairs(&[("NAME", "value")]);
    assert_eq!(sub("${NAME:-fallback}", &vars).unwrap(), "value");
}

#[skuld::test]
fn an_empty_default_yields_an_empty_string() {
    let vars = Vars::empty();
    assert_eq!(sub("${NAME:-}", &vars).unwrap(), "");
}

#[skuld::test]
fn a_closing_brace_inside_a_default_truncates_it() {
    // Deliberate, documented truncation, not a bug: a default has no escape
    // for a literal `}`, so it ends at the first one. `A` is set (its
    // default is discarded), leaving `default = "{x"` unused and the
    // second `}` copied through as ordinary trailing text.
    let vars = Vars::from_pairs(&[("A", "a")]);
    assert_eq!(sub("${A:-{x}}", &vars).unwrap(), "a}");
}

#[skuld::test]
fn a_dollar_in_a_default_is_an_error() {
    let vars = Vars::from_pairs(&[("X", "x")]);
    let err = sub("${NAME:-$X}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: `$` is not allowed inside a `:-` default"
    );
}

#[skuld::test]
fn a_dollar_in_a_discarded_default_is_still_an_error() {
    // The default is never used here (`SET` is defined and non-empty), but
    // its grammar is checked unconditionally: a future short-circuit that
    // skips scanning an unused default must not make this pass.
    let vars = Vars::from_pairs(&[("SET", "value"), ("X", "x")]);
    let err = sub("${SET:-$X}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: `$` is not allowed inside a `:-` default"
    );
}

// Literal-dollar escape ===============================================================================================

#[skuld::test]
fn double_dollar_is_a_literal_dollar() {
    let vars = Vars::empty();
    assert_eq!(sub("$$", &vars).unwrap(), "$");
    assert_eq!(sub("$$$$", &vars).unwrap(), "$$");
}

// Unresolved / bare-dollar errors =====================================================================================

#[skuld::test]
fn an_unset_variable_without_a_default_names_the_variable() {
    let vars = Vars::empty();
    let err = sub("${MISSING}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: no value for `MISSING`; define it in the `.env` file beside the manifest, or write `${MISSING:-default}`"
    );
}

#[skuld::test]
fn a_bare_dollar_variable_is_an_error() {
    let vars = Vars::from_pairs(&[("HOME", "/root")]);
    let err = sub("$HOME", &vars).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("$$"),
        "message should offer `$$` as the escape: {message}"
    );
    assert_eq!(
        message,
        "daemons.frpc.env[\"LOG\"]: `$` at byte 0 is not part of `${...}`; write `$$` for a literal `$`"
    );
}

#[skuld::test]
fn a_trailing_lone_dollar_is_an_error() {
    let vars = Vars::empty();
    let err = sub("abc$", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: `$` at byte 3 is not part of `${...}`; write `$$` for a literal `$`"
    );
}

// Malformed `${...}` ==================================================================================================

#[skuld::test]
fn an_unterminated_brace_is_an_error() {
    let vars = Vars::empty();
    let err = sub("${NAME", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: unterminated `${`: no closing `}`"
    );
}

#[skuld::test]
fn an_empty_variable_name_is_an_error() {
    let vars = Vars::empty();

    let err = sub("${}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: empty variable name in `${}`"
    );

    // `:` here opens a `:-` default, not an invalid character — the real
    // defect is the empty name preceding it, so this must report the same
    // message as `${}`, not `invalid character `:``.
    let err = sub("${:-fallback}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: empty variable name in `${}`"
    );
}

#[skuld::test]
fn an_invalid_character_in_a_variable_name_is_an_error() {
    let vars = Vars::empty();

    let err = sub("${1A}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: invalid character `1` in a variable name; names match [A-Za-z_][A-Za-z0-9_]*"
    );

    let err = sub("${A-B}", &vars).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.env[\"LOG\"]: invalid character `-` in a variable name; names match [A-Za-z_][A-Za-z0-9_]*"
    );
}

// No rescanning =======================================================================================================

#[skuld::test]
fn a_substituted_value_is_not_rescanned() {
    let vars = Vars::from_pairs(&[("NAME", "${OTHER}"), ("OTHER", "other-value")]);
    assert_eq!(sub("${NAME}", &vars).unwrap(), "${OTHER}");
}

// `would_substitution_change` =========================================================================================

#[skuld::test]
fn would_substitution_change_is_true_for_an_escaped_dollar() {
    assert!(would_substitution_change("${A}"));
    assert!(would_substitution_change("$$"));
    assert!(!would_substitution_change("no dollars here"));
}

// Error path attribution ==============================================================================================

#[skuld::test]
fn errors_carry_the_supplied_path() {
    let vars = Vars::empty();
    let err = scalar("${MISSING}", "daemons.example.name", &vars).unwrap_err();
    match err {
        Error::Interpolate { path, .. } => assert_eq!(path, "daemons.example.name"),
        other => panic!("expected Error::Interpolate, got: {other:?}"),
    }
}

// `check_grammar` =====================================================================================================

#[skuld::test]
fn check_grammar_accepts_a_well_formed_reference_without_resolving_it() {
    // Grammar only: no variable is resolved, so an undefined name is not an
    // error here — the whole reason `check_grammar` exists rather than
    // reusing `scalar` with an empty `Vars`.
    assert!(super::check_grammar("${UNDEFINED}", "daemons.frpc.name").is_ok());
}

#[skuld::test]
fn check_grammar_rejects_a_bare_dollar() {
    let err = super::check_grammar("$HOME", "daemons.frpc.name").unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.name: `$` at byte 0 is not part of `${...}`; write `$$` for a literal `$`"
    );
}

// The `spec` walk =====================================================================================================

/// Parse a manifest fixture, substitute its one `frpc` daemon, and hand back
/// the mutated `RawSpec` — the same walk `resolve` now does per daemon.
fn interpolate_yaml(yaml: &str, pairs: &[(&str, &str)]) -> Result<RawSpec, Error> {
    let mut raw: RawManifest = yaml_serde::from_str(yaml).expect("fixture yaml should parse");
    let mut entry = raw.daemons.remove("frpc").expect("fixture must declare `frpc`");
    spec(&mut entry, "daemons.frpc", &Vars::from_pairs(pairs))?;
    Ok(entry)
}

#[skuld::test]
fn substitutes_every_string_leaf() {
    let yaml = r#"
daemons:
  frpc:
    name: ${NAME}
    command: ["${BIN}", "--flag=${FLAG}"]
    cwd: ${CWD}
    logs: ${LOGS}
    env:
      LOG: ${LEVEL}
    user: ${USER_NAME}
    restart: ${RESTART}
    restart-delay: ${DELAY}
    type: ${TYPE}
"#;
    let spec = interpolate_yaml(
        yaml,
        &[
            ("NAME", "Frpc Tunnel"),
            ("BIN", "/usr/bin/frpc"),
            ("FLAG", "on"),
            ("CWD", "/opt/frpc"),
            ("LOGS", "/var/log/frpc.log"),
            ("LEVEL", "info"),
            ("USER_NAME", "svc-frpc"),
            ("RESTART", "always"),
            ("DELAY", "2s"),
            ("TYPE", "managed"),
        ],
    )
    .unwrap();

    assert_eq!(spec.name.as_deref(), Some("Frpc Tunnel"));
    assert_eq!(
        spec.command.as_deref(),
        Some(&["/usr/bin/frpc".to_string(), "--flag=on".to_string()][..])
    );
    assert_eq!(spec.cwd.as_deref(), Some("/opt/frpc"));
    assert_eq!(spec.logs.as_deref(), Some("/var/log/frpc.log"));
    assert_eq!(spec.env["LOG"], "info");
    assert_eq!(spec.user, Some(RawUser::Scalar("svc-frpc".to_string())));
    assert_eq!(spec.restart.as_deref(), Some("always"));
    assert_eq!(spec.restart_delay.as_deref(), Some("2s"));
    assert_eq!(spec.kind.as_deref(), Some("managed"));
}

#[skuld::test]
fn a_dollar_in_a_daemon_id_is_an_error() {
    // `spec`/`interpolate_yaml` no longer see the daemon id at all — the
    // rejection is `reject_interpolated_id`'s alone now. The end-to-end
    // half (through `resolve`) lives in `resolve_tests.rs`.
    let err = reject_interpolated_id("${ID}").unwrap_err();
    assert!(
        err.to_string().contains("a daemon id cannot be interpolated"),
        "expected an id-is-a-key rejection, got: {err}"
    );
}

#[skuld::test]
fn a_dollar_in_an_env_name_is_an_error() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      ${KEY}: value\n";
    let err = interpolate_yaml(yaml, &[("KEY", "LOG")]).unwrap_err();
    assert!(
        err.to_string().contains("an `env` name cannot be interpolated"),
        "expected an env-name-is-a-key rejection, got: {err}"
    );
}

#[skuld::test]
fn an_env_name_containing_a_dot_is_not_mistaken_for_user_id() {
    // The walk dispatches on the struct, never on a rendered field path, so
    // an `env` key that happens to spell one cannot pick up that field's
    // rule.
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    env:\n      MY_user.id: ${TOKEN}\n";
    let spec = interpolate_yaml(yaml, &[("TOKEN", "s3cret")]).unwrap();
    assert_eq!(spec.env["MY_user.id"], "s3cret");
}

#[skuld::test]
fn a_dollar_under_user_id_is_an_error() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: \"${SID}\"\n";
    let err = interpolate_yaml(yaml, &[("SID", "S-1-5-21-1")]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "daemons.frpc.user.id: `user.id` cannot be interpolated: a substituted value is always a string, \
         so it would be read as a Windows SID; write the uid literally, or use `user: <name>`"
    );
}

#[skuld::test]
fn a_numeric_user_id_is_untouched() {
    let yaml = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      id: 1001\n";
    let spec = interpolate_yaml(yaml, &[]).unwrap();
    assert_eq!(spec.user, Some(RawUser::Id(AccountId::Uid(1001))));
}

/// The `user:` of `yaml`'s single `frpc` daemon, substituted and then
/// resolved — the two halves `load` runs back to back.
fn interpolated_user(yaml: &str, pairs: &[(&str, &str)]) -> User {
    let spec = interpolate_yaml(yaml, pairs).unwrap();
    resolve_user(spec.user)
}

#[skuld::test]
fn an_interpolated_user_of_root_normalises_to_the_root_variant() {
    // The bare string form is the one the `root` reserved word applies to,
    // and a substituted value must mean what the same text typed by hand
    // means: `User::Name("root")` would make systemd emit `User=root`
    // rather than `User=0`, and Windows resolve a nonexistent account.
    let interpolated = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: ${U}\n";
    let literal = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user: root\n";

    assert_eq!(interpolated_user(interpolated, &[("U", "root")]), User::Root);
    assert_eq!(interpolated_user(literal, &[]), User::Root);
}

#[skuld::test]
fn an_interpolated_user_name_of_root_stays_a_literal_account() {
    // The mirror image, and the reason the reserved word is `resolve`'s to
    // apply rather than something re-applied to a substituted `User::Name`:
    // `{name: ...}` is the escape hatch for an account genuinely called
    // `root`, so `{name: ${U}}` with `U=root` must mean exactly what
    // `{name: root}` means. Collapsing it to `Root` would silently install
    // as LocalSystem on Windows where the authored form fails closed.
    let interpolated = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      name: ${U}\n";
    let literal = "daemons:\n  frpc:\n    command: [/bin/frpc]\n    user:\n      name: root\n";
    let expected = User::Name("root".to_string());

    assert_eq!(interpolated_user(interpolated, &[("U", "root")]), expected);
    assert_eq!(interpolated_user(literal, &[]), expected);
}

#[skuld::test]
fn spec_would_change_is_true_for_an_escaped_dollar_only() {
    // The over-approximation, pinned at spec level: a `$$` escape has no
    // reference to resolve, yet still reports `true`. See the module doc
    // comment — narrowing this is a defect, not an optimisation.
    let escaped: RawManifest = yaml_serde::from_str("daemons:\n  frpc:\n    command: [/bin/frpc, $$ARGS]\n").unwrap();
    assert!(spec_would_change(&escaped.daemons["frpc"]));

    let dollarless: RawManifest =
        yaml_serde::from_str("daemons:\n  frpc:\n    command: [/bin/frpc, --flag]\n").unwrap();
    assert!(!spec_would_change(&dollarless.daemons["frpc"]));
}

// `check_spec_grammar` ================================================================================================

#[skuld::test]
fn check_spec_grammar_accepts_a_well_formed_manifest() {
    let raw: RawManifest =
        yaml_serde::from_str("daemons:\n  frpc:\n    name: ${NAME}\n    command: [\"${BIN}\"]\n    restart: ${R}\n")
            .unwrap();
    assert!(check_spec_grammar(&raw.daemons["frpc"], "daemons.frpc").is_ok());
}

#[skuld::test]
fn check_spec_grammar_rejects_a_bare_dollar_in_name() {
    let raw: RawManifest =
        yaml_serde::from_str("daemons:\n  frpc:\n    name: a$b\n    command: [/bin/frpc]\n").unwrap();
    let err = check_spec_grammar(&raw.daemons["frpc"], "daemons.frpc").unwrap_err();
    assert!(
        err.to_string().contains("not part of"),
        "expected the bare-`$` grammar rejection, got: {err}"
    );
}

#[skuld::test]
fn check_spec_grammar_does_not_require_a_defined_variable() {
    // Grammar only, never resolution — an undefined `${UNDEFINED}` must not
    // fail here; that is `interpolate::spec`'s job, once `.env` is actually
    // consulted.
    let raw: RawManifest =
        yaml_serde::from_str("daemons:\n  frpc:\n    name: ${UNDEFINED}\n    command: [/bin/frpc]\n").unwrap();
    assert!(check_spec_grammar(&raw.daemons["frpc"], "daemons.frpc").is_ok());
}
