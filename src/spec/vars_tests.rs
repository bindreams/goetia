use super::Vars;
use crate::error::Error;

/// Write `contents` to `<dir>/.env` and load it.
fn load_str(contents: &str) -> Result<Vars, Error> {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::write(dir.path().join(".env"), contents).expect("fixture .env should be writable");
    Vars::load(dir.path())
}

/// Write raw `bytes` to `<dir>/.env` and load it, for BOM/encoding fixtures
/// that are not valid UTF-8 text.
fn load_bytes(bytes: &[u8]) -> Result<Vars, Error> {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::write(dir.path().join(".env"), bytes).expect("fixture .env should be writable");
    Vars::load(dir.path())
}

// Plain assignments ===================================================================================================

#[skuld::test]
fn reads_plain_assignments() {
    let vars = load_str("A=1\nB=two\n").unwrap();
    assert_eq!(vars.get("A"), Some("1"));
    assert_eq!(vars.get("B"), Some("two"));
}

#[skuld::test]
fn an_empty_value_is_the_empty_string() {
    let vars = load_str("A=\n").unwrap();
    assert_eq!(vars.get("A"), Some(""));
}

// Missing / unreadable file ===========================================================================================

#[skuld::test]
fn missing_env_file_yields_no_variables() {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    let vars = Vars::load(dir.path()).expect("a missing .env should not be an error");
    assert_eq!(vars.get("ANYTHING"), None);
}

#[skuld::test]
fn a_directory_at_the_env_path_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");
    let err = Vars::load(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "expected Io, got: {err:?}");
}

// Blank lines, comments, whitespace ===================================================================================

#[skuld::test]
fn skips_blank_lines_and_comments() {
    let vars = load_str("\n   \n# a comment\n   # indented comment\nA=1\n").unwrap();
    assert_eq!(vars.get("A"), Some("1"));
}

#[skuld::test]
fn leading_whitespace_before_a_name_is_allowed() {
    let vars = load_str("   A=1\n\tB=2\n").unwrap();
    assert_eq!(vars.get("A"), Some("1"));
    assert_eq!(vars.get("B"), Some("2"));
}

#[skuld::test]
fn an_export_prefix_is_stripped_only_when_followed_by_whitespace() {
    let vars = load_str("export A=1\nexport=2\n").unwrap();
    assert_eq!(vars.get("A"), Some("1"));
    assert_eq!(vars.get("export"), Some("2"));
}

#[skuld::test]
fn whitespace_around_the_equals_sign_is_an_error() {
    let err = load_str("A =1\n").unwrap_err();
    assert!(matches!(err, Error::EnvFile { .. }));
    assert!(err.to_string().contains("A=1"), "message should show `A=1`: {err}");

    let err = load_str("A= 1\n").unwrap_err();
    assert!(matches!(err, Error::EnvFile { .. }));
    assert!(err.to_string().contains("A=1"), "message should show `A=1`: {err}");
}

#[skuld::test]
fn trailing_whitespace_at_the_end_of_the_line_yields_an_empty_value() {
    // `A= ` is not `A =` or `A= 1`: its only whitespace is at the very end
    // of the line, where an unquoted value's trailing whitespace is
    // trimmed regardless — so it means `A=`, not an error.
    let vars = load_str("A= \n").unwrap();
    assert_eq!(vars.get("A"), Some(""));
}

#[skuld::test]
fn trailing_whitespace_after_a_closing_quote_does_not_leak_into_the_value() {
    // Pins the line-end trim to running on the *line*, not the quoted
    // value: the trailing space outside the quotes is stripped, but the
    // one inside them survives.
    let vars = load_str("A=\"x \" \n").unwrap();
    assert_eq!(vars.get("A"), Some("x "));
}

// Encoding ============================================================================================================

#[skuld::test]
fn strips_a_utf8_bom_from_the_first_key() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"A=1\n");
    let vars = load_bytes(&bytes).unwrap();
    assert_eq!(vars.get("A"), Some("1"));
}

#[skuld::test]
fn a_utf16_env_file_is_rejected_by_name() {
    // "A=1\n" encoded as UTF-16LE with its BOM.
    let mut bytes = vec![0xFF, 0xFE];
    for c in "A=1\n".encode_utf16() {
        bytes.extend_from_slice(&c.to_le_bytes());
    }
    let err = load_bytes(&bytes).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("UTF-16"), "message should name the encoding: {msg}");
    assert!(
        !msg.contains("invalid UTF-8"),
        "message should not be the generic decode error: {msg}"
    );
    assert!(msg.contains(".env"), "message should name the file: {msg}");
}

#[skuld::test]
fn a_utf32_bom_is_not_misreported_as_utf16() {
    // The UTF-32LE BOM (`FF FE 00 00`) shares its first two bytes with the
    // UTF-16LE BOM (`FF FE`); the 4-byte pattern must be checked first.
    let mut bytes = vec![0xFF, 0xFE, 0x00, 0x00];
    for c in "A=1\n".chars() {
        bytes.extend_from_slice(&(c as u32).to_le_bytes());
    }
    let err = load_bytes(&bytes).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("UTF-32"), "message should name the encoding: {msg}");
    assert!(
        !msg.contains("UTF-16"),
        "message should not be misreported as UTF-16: {msg}"
    );
}

// Quoting =============================================================================================================

#[skuld::test]
fn single_quoted_values_are_literal() {
    let vars = load_str(r#"A='hello world'"#).unwrap();
    assert_eq!(vars.get("A"), Some("hello world"));

    let vars = load_str(r#"A='$NOT_EXPANDED "still literal"'"#).unwrap();
    assert_eq!(vars.get("A"), Some(r#"$NOT_EXPANDED "still literal""#));
}

#[skuld::test]
fn double_quoted_values_decode_backslash_and_quote() {
    let vars = load_str(r#"A="a\\b\"c""#).unwrap();
    assert_eq!(vars.get("A"), Some(r#"a\b"c"#));
}

#[skuld::test]
fn a_backslash_n_escape_is_an_error() {
    let err = load_str(r#"A="a\nb""#).unwrap_err();
    match &err {
        Error::EnvFile { line, message, .. } => {
            assert_eq!(*line, 1);
            assert!(message.contains("\\n"), "message should name the escape: {message}");
        }
        other => panic!("expected EnvFile, got: {other:?}"),
    }
}

#[skuld::test]
fn an_unterminated_quote_is_an_error() {
    let err = load_str("A='hello\n").unwrap_err();
    assert!(matches!(err, Error::EnvFile { .. }));

    let err = load_str(r#"A="hello"#).unwrap_err();
    assert!(matches!(err, Error::EnvFile { .. }));
}

#[skuld::test]
fn trailing_text_after_a_closing_quote_is_an_error() {
    let err = load_str("A='hello'world\n").unwrap_err();
    assert!(matches!(err, Error::EnvFile { .. }));
}

// Unquoted values =====================================================================================================

#[skuld::test]
fn an_unquoted_value_drops_a_trailing_comment_and_whitespace() {
    let vars = load_str("A=1   # a comment\n").unwrap();
    assert_eq!(vars.get("A"), Some("1"));
}

#[skuld::test]
fn a_hash_not_preceded_by_whitespace_stays_in_an_unquoted_value() {
    let vars = load_str("A=1#nothash\n").unwrap();
    assert_eq!(vars.get("A"), Some("1#nothash"));
}

#[skuld::test]
fn dollar_signs_in_values_are_literal() {
    let vars = load_str("LOG=$MISSING/app.log\nB=${NOPE}\nA=$$\n").unwrap();
    assert_eq!(vars.get("LOG"), Some("$MISSING/app.log"));
    assert_eq!(vars.get("B"), Some("${NOPE}"));
    assert_eq!(vars.get("A"), Some("$$"));
}

// Malformed lines =====================================================================================================

#[skuld::test]
fn a_line_without_an_equals_sign_is_an_error() {
    let err = load_str("NOEQUALS\n").unwrap_err();
    assert!(matches!(err, Error::EnvFile { .. }));
}

#[skuld::test]
fn an_invalid_variable_name_is_an_error() {
    let err = load_str("9NAME=1\n").unwrap_err();
    assert!(
        matches!(err, Error::EnvFile { .. }),
        "leading digit should be rejected: {err:?}"
    );

    let err = load_str("NA-ME=1\n").unwrap_err();
    assert!(
        matches!(err, Error::EnvFile { .. }),
        "a dash should be rejected: {err:?}"
    );
}

#[skuld::test]
fn a_duplicate_name_is_an_error() {
    let err = load_str("A=1\nB=2\nA=3\n").unwrap_err();
    match &err {
        Error::EnvFile { line, message, .. } => {
            assert_eq!(*line, 3);
            assert!(message.contains('A'), "message should name the variable: {message}");
            assert!(
                message.contains("line 1"),
                "message should name the first line (1): {message}"
            );
        }
        other => panic!("expected EnvFile, got: {other:?}"),
    }
}

#[skuld::test]
fn parse_errors_name_the_file_and_the_line() {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    std::fs::write(dir.path().join(".env"), "A=1\nNOEQUALS\n").unwrap();
    let err = Vars::load(dir.path()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(".env"), "message should name the file: {msg}");
    assert!(
        msg.contains(".env:2:"),
        "message should name line 2 as `.env:2:`: {msg}"
    );
}

// Isolation from the process environment ==============================================================================

#[skuld::test]
fn get_does_not_fall_back_to_an_ambient_variable() {
    // Pick a name the *ambient* process environment already defines,
    // without this test setting one itself (setting a process variable
    // from a test is a data race across the suite and is forbidden here).
    // A `.env` that never mentions this name must not leak it into
    // `Vars`, proving `get` never falls back to `std::env`. (The
    // no-`$`-expansion property is a separate one, pinned by
    // `dollar_signs_in_values_are_literal`.)
    let ambient_name = std::env::vars()
        .map(|(name, _)| name)
        .find(|name| name != "GOETIA_TEST_OTHER")
        .expect(
            "test fixture could not be built: the process environment defines no variable to \
             use as the ambient name this test checks `Vars::get` does not fall back to",
        );

    let vars = load_str("GOETIA_TEST_OTHER=1\n").unwrap();
    assert_eq!(vars.get(&ambient_name), None);
}
