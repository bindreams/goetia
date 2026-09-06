use super::{scalar, would_substitution_change};
use crate::error::Error;
use crate::spec::vars::Vars;

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
    // Adjacent.
    assert_eq!(sub("${A}${B}", &vars).unwrap(), "12");
    // Separated by literal text.
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
