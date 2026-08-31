use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::spec::{Id, Kind, User};

// Fixtures ============================================================================================================
//
// Constructed literally, never via `spec::resolve()`: `resolve()` joins
// relative paths against a base dir with `Path::join`, which inserts the
// platform's native separator, so a spec built that way renders `\` on
// Windows and `/` elsewhere and would break the snapshot assertions below on
// two of three CI platforms. A `PathBuf` built directly from a forward-slash
// literal (no `join`) keeps the exact bytes it was given on every platform.

fn full_spec() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "Frpc Tunnel".to_string(),
        command: vec!["frpc".to_string(), "-c".to_string(), "frpc.toml".to_string()],
        cwd: Some(PathBuf::from("/opt/frpc")),
        env: BTreeMap::from([
            ("RUST_LOG".to_string(), "info".to_string()),
            ("URL".to_string(), "http://x/a%20b".to_string()),
        ]),
        user: User::Name("svc-frpc".to_string()),
        restart: Restart::Always,
        restart_delay: Some(Duration::new(2, 500_000_000)),
        logs: Some(PathBuf::from("/var/log/frpc.log")),
        kind: Kind::Managed,
    }
}

fn full_identity() -> Identity {
    Identity {
        user: "svc-frpc".to_string(),
    }
}

// `blob::decode` (reached through `extract`) re-runs `spec::resolve`'s
// invariant checks, including "cwd/logs must be absolute" — and
// `Path::new("/opt/rt").is_absolute()` is `false` on Windows, so a spec
// used for a round trip needs a path that is genuinely absolute on the
// host platform, unlike the literal forward-slash paths the snapshot
// fixtures above use (which are never decoded, only rendered).
#[cfg(windows)]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!(r"C:\{p}"))
}
#[cfg(not(windows))]
fn abs(p: &str) -> PathBuf {
    PathBuf::from(format!("/{p}"))
}

fn roundtrip_spec() -> DaemonSpec {
    DaemonSpec {
        cwd: Some(abs("opt/frpc")),
        logs: Some(abs("var/log/frpc.log")),
        ..full_spec()
    }
}

fn minimal_spec() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "frpc".to_string(),
        command: vec!["frpc".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

fn minimal_identity() -> Identity {
    Identity {
        user: "root".to_string(),
    }
}

fn spec_with_restart(restart: Restart) -> DaemonSpec {
    DaemonSpec {
        restart,
        ..minimal_spec()
    }
}

// Snapshots ===========================================================================================================
//
// Reviewed by eye: a malformed plist means the daemon refuses to load. The
// metadata comment's `Version`/`Spec` fields are interpolated from the real
// `crate::version()` and `blob::encode` rather than hand-transcribed, so
// this snapshot does not need updating on every crate version bump;
// everything else is pinned literally.

#[skuld::test]
fn plist_snapshot_minimal() {
    let spec = minimal_spec();
    let text = plist(&spec, &minimal_identity());

    let expected = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- goetia:begin\n\
         Marker: goetia\n\
         Schema: 1\n\
         Version: {version}\n\
         Spec: {blob}\n\
         goetia:end -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>frpc</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>frpc</string>\n\
         \t</array>\n\
         \t<key>UserName</key>\n\
         \t<string>root</string>\n\
         </dict>\n\
         </plist>\n",
        version = crate::version(),
        blob = blob::encode(&spec),
    );

    assert_eq!(text, expected);
}

#[skuld::test]
fn plist_snapshot_full() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity());

    let expected = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- goetia:begin\n\
         Marker: goetia\n\
         Schema: 1\n\
         Version: {version}\n\
         Spec: {blob}\n\
         goetia:end -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>frpc</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>frpc</string>\n\
         \t\t<string>-c</string>\n\
         \t\t<string>frpc.toml</string>\n\
         \t</array>\n\
         \t<key>WorkingDirectory</key>\n\
         \t<string>/opt/frpc</string>\n\
         \t<key>EnvironmentVariables</key>\n\
         \t<dict>\n\
         \t\t<key>RUST_LOG</key>\n\
         \t\t<string>info</string>\n\
         \t\t<key>URL</key>\n\
         \t\t<string>http://x/a%20b</string>\n\
         \t</dict>\n\
         \t<key>UserName</key>\n\
         \t<string>svc-frpc</string>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>ThrottleInterval</key>\n\
         \t<integer>3</integer>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>/var/log/frpc.log</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>/var/log/frpc.log</string>\n\
         </dict>\n\
         </plist>\n",
        version = crate::version(),
        blob = blob::encode(&spec),
    );

    assert_eq!(text, expected);
}

// Round trip and the generation invariant ============================================================================

#[skuld::test]
fn plist_round_trips_through_extract() {
    let spec = roundtrip_spec();
    let text = plist(&spec, &full_identity());

    let decoded = extract(&text)
        .expect("extract should succeed on our own output")
        .expect("marker should be present");

    assert_eq!(decoded.spec, spec);
    assert_eq!(decoded.schema, blob::SCHEMA);
    assert_eq!(decoded.version, crate::version());
}

#[skuld::test]
fn plist_is_the_generation_invariant() {
    let spec = roundtrip_spec();
    let id = full_identity();
    let text = plist(&spec, &id);

    let decoded = extract(&text).unwrap().unwrap();
    let regenerated = plist(&decoded.spec, &id);

    assert_eq!(
        regenerated, text,
        "regenerating from the extracted spec must be byte-identical"
    );
}

// Extract dispositions ================================================================================================

const FOREIGN_PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
    <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
    <plist version=\"1.0\">\n\
    <dict>\n\
    \t<key>Label</key>\n\
    \t<string>com.example.other</string>\n\
    \t<key>ProgramArguments</key>\n\
    \t<array>\n\
    \t\t<string>/bin/other</string>\n\
    \t</array>\n\
    </dict>\n\
    </plist>\n";

#[skuld::test]
fn extract_returns_none_for_foreign_plist() {
    assert!(extract(FOREIGN_PLIST).unwrap().is_none());
}

#[skuld::test]
fn extract_returns_none_when_no_metadata_comment_matches_our_marker() {
    // A plist that does carry an XML comment, but not ours: proves the
    // sentinel — not "any comment exists" — is what identifies ownership.
    let text = FOREIGN_PLIST.replacen(
        "<plist",
        "<!-- just a note left by a human, nothing to do with goetia -->\n<plist",
        1,
    );
    assert!(extract(&text).unwrap().is_none());
}

#[skuld::test]
fn extract_errors_on_corrupt_blob() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity());

    // Corrupt the base64 payload embedded in the comment, without disturbing
    // the surrounding sentinel structure. The comment is still recognizably
    // "ours" (the marker is present) but the blob no longer decodes.
    let corrupted = text.replacen("Spec: ", "Spec: !!!not-base64!!!", 1);

    let err = extract(&corrupted).expect_err("a corrupt blob must error, not silently decode");
    assert!(matches!(err, Error::Blob(_)), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_on_duplicate_metadata_comment() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity());

    // Two well-formed metadata comments in one document.
    let comment_end = text.find("-->").expect("fixture must contain a comment") + "-->".len();
    let mut doubled = text[..comment_end].to_string();
    doubled.push('\n');
    doubled.push_str(&text[..comment_end]);
    doubled.push_str(&text[comment_end..]);

    let err = extract(&doubled).expect_err("more than one metadata comment must error");
    assert!(matches!(err, Error::Blob(_)), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_when_spec_value_is_split_across_lines() {
    // A hand-edit (or a bug) can insert a bare newline in the middle of the
    // `Spec` value. That must be rejected outright, not silently truncated
    // to whatever precedes the newline.
    let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <!-- goetia:begin\n\
        Marker: goetia\n\
        Schema: 1\n\
        Version: 0.1.0\n\
        Spec: abcd\n\
        efgh\n\
        goetia:end -->\n\
        <plist version=\"1.0\">\n\
        <dict>\n\
        \t<key>Label</key>\n\
        \t<string>frpc</string>\n\
        </dict>\n\
        </plist>\n";

    let err = extract(text).expect_err("a split Spec value must error, not silently mis-decode");
    assert!(matches!(err, Error::Blob(_)), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_on_metadata_comment_missing_a_field() {
    // A hand-edited or bit-rotted comment with `Version` deleted: still
    // recognizably ours (the sentinels are intact), still not decodable.
    let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <!-- goetia:begin\n\
        Marker: goetia\n\
        Schema: 1\n\
        Spec: abcd\n\
        goetia:end -->\n\
        <plist version=\"1.0\">\n\
        <dict>\n\
        \t<key>Label</key>\n\
        \t<string>frpc</string>\n\
        </dict>\n\
        </plist>\n";

    let err = extract(text).expect_err("a metadata comment missing a field must error");
    assert!(matches!(err, Error::Blob(_)), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_on_marker_mismatch() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity());
    let tampered = text.replacen("Marker: goetia", "Marker: not-goetia", 1);

    let err = extract(&tampered).expect_err("a metadata comment naming the wrong marker must error");
    assert!(matches!(err, Error::Blob(_)), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_on_unterminated_begin_sentinel() {
    let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <!-- goetia:begin\n\
        Marker: goetia\n\
        <plist version=\"1.0\">\n\
        <dict>\n\
        \t<key>Label</key>\n\
        \t<string>frpc</string>\n\
        </dict>\n\
        </plist>\n";

    let err = extract(text).expect_err("an unterminated goetia:begin must error, not be treated as absent");
    assert!(matches!(err, Error::Blob(_)), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_tolerates_crlf_line_endings_in_the_metadata_comment() {
    // A plist that has been saved with CRLF line endings (e.g. by a Windows
    // editor) still carries a fully intact metadata comment — the sentinel
    // match must not be tied to the exact `\n` byte.
    let spec = roundtrip_spec();
    let text = plist(&spec, &full_identity());
    let crlf = text.replace('\n', "\r\n");

    let decoded = extract(&crlf)
        .expect("extract should tolerate CRLF line endings")
        .expect("marker should still be recognized after CRLF conversion");

    assert_eq!(decoded.spec, spec);
}

// Escaping ============================================================================================================

#[skuld::test]
fn xml_special_characters_are_escaped() {
    let mut spec = minimal_spec();
    spec.command = vec!["frpc".to_string(), "a&b<c>d".to_string()];
    spec.env = BTreeMap::from([("K&L<M>N".to_string(), "x&y<z>w".to_string())]);
    spec.cwd = Some(PathBuf::from("/opt/a&b<c>d"));
    spec.logs = Some(PathBuf::from("/var/log/a&b<c>d"));

    let text = plist(
        &spec,
        &Identity {
            user: "svc&name<x>y".to_string(),
        },
    );

    assert!(
        text.contains("<string>a&amp;b&lt;c&gt;d</string>"),
        "argv value was not escaped:\n{text}"
    );
    assert!(
        text.contains("<key>K&amp;L&lt;M&gt;N</key>"),
        "env key was not escaped:\n{text}"
    );
    assert!(
        text.contains("<string>x&amp;y&lt;z&gt;w</string>"),
        "env value was not escaped:\n{text}"
    );
    assert!(
        text.contains("<key>UserName</key>\n\t<string>svc&amp;name&lt;x&gt;y</string>"),
        "UserName was not escaped:\n{text}"
    );
    assert!(
        text.contains("<key>WorkingDirectory</key>\n\t<string>/opt/a&amp;b&lt;c&gt;d</string>"),
        "cwd was not escaped:\n{text}"
    );
    assert!(
        text.contains("<key>StandardOutPath</key>\n\t<string>/var/log/a&amp;b&lt;c&gt;d</string>"),
        "logs was not escaped:\n{text}"
    );
    assert!(
        !text.contains("a&b<c>d"),
        "raw unescaped argv value must not appear in the output"
    );
    assert!(
        !text.contains("K&L<M>N"),
        "raw unescaped env key must not appear in the output"
    );
    assert!(
        !text.contains("svc&name<x>y"),
        "raw unescaped UserName must not appear in the output"
    );
}

#[skuld::test]
fn xml_noncharacters_panic_loudly_rather_than_emit_invalid_xml() {
    // XML 1.0's `Char` production excludes U+FFFE/U+FFFF entirely — no
    // escape can represent them, and `spec::resolve`'s control-character
    // gate does not reject them (`char::is_control()` doesn't cover
    // noncharacters). Until that upstream gate closes, this generator must
    // fail loudly rather than silently emit a plist no XML parser can load.
    let mut spec = minimal_spec();
    spec.command = vec!["frpc".to_string(), format!("bad{}arg", '\u{FFFF}')];

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plist(&spec, &minimal_identity())));
    std::panic::set_hook(previous_hook);

    assert!(
        result.is_err(),
        "plist() must panic rather than emit unrepresentable XML"
    );
}

// restart -> KeepAlive ================================================================================================

#[skuld::test]
fn restart_variants_map_to_keepalive() {
    let never = plist(&spec_with_restart(Restart::Never), &minimal_identity());
    assert!(
        !never.contains("<key>KeepAlive</key>"),
        "never must omit KeepAlive:\n{never}"
    );
    assert!(
        !never.contains("<key>RunAtLoad</key>"),
        "never must omit RunAtLoad:\n{never}"
    );

    let on_failure = plist(&spec_with_restart(Restart::OnFailure), &minimal_identity());
    assert!(
        on_failure.contains("<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n"),
        "on-failure must emit a KeepAlive dict with SuccessfulExit: false:\n{on_failure}"
    );
    assert!(on_failure.contains("<key>RunAtLoad</key>\n\t<true/>\n"));

    let always = plist(&spec_with_restart(Restart::Always), &minimal_identity());
    assert!(
        always.contains("<key>KeepAlive</key>\n\t<true/>\n"),
        "always must emit KeepAlive: true:\n{always}"
    );
    assert!(always.contains("<key>RunAtLoad</key>\n\t<true/>\n"));
}

// ThrottleInterval ====================================================================================================

#[skuld::test]
fn throttle_interval_rounds_up() {
    let cases = [
        (Duration::new(1, 500_000_000), 2), // 1.5s -> 2, per the plan's own example
        (Duration::from_millis(500), 1),    // 500ms would truncate to 0, which disables throttling
        (Duration::from_secs(2), 2),        // already whole: unchanged
        (Duration::ZERO, 0),                // explicit opt-out is left alone, not "rounded"
    ];

    for (delay, expected) in cases {
        let spec = DaemonSpec {
            restart: Restart::Always,
            restart_delay: Some(delay),
            ..minimal_spec()
        };
        let text = plist(&spec, &minimal_identity());
        let needle = format!("<key>ThrottleInterval</key>\n\t<integer>{expected}</integer>\n");
        assert!(
            text.contains(&needle),
            "restart_delay {delay:?} should round up to {expected}s:\n{text}"
        );
    }
}

#[skuld::test]
fn throttle_interval_rounding_does_not_overflow_at_the_duration_boundary() {
    // `Duration::as_secs()` can legitimately be `u64::MAX` (e.g.
    // `Duration::MAX`, whose `subsec_nanos()` is 999_999_999): rounding up
    // with a plain `+ 1` would overflow, wrapping to 0 in release — the
    // exact "disables throttling" failure the rounding exists to prevent.
    let spec = DaemonSpec {
        restart: Restart::Always,
        restart_delay: Some(Duration::MAX),
        ..minimal_spec()
    };
    let text = plist(&spec, &minimal_identity());
    let needle = format!("<key>ThrottleInterval</key>\n\t<integer>{}</integer>\n", u64::MAX);
    assert!(
        text.contains(&needle),
        "Duration::MAX should saturate, not overflow:\n{text}"
    );
}

#[skuld::test]
fn throttle_interval_omitted_without_restart_delay() {
    let spec = DaemonSpec {
        restart: Restart::Always,
        restart_delay: None,
        ..minimal_spec()
    };
    let text = plist(&spec, &minimal_identity());
    assert!(
        !text.contains("ThrottleInterval"),
        "no restart_delay means no ThrottleInterval:\n{text}"
    );
}

#[skuld::test]
fn throttle_interval_omitted_when_restart_is_never() {
    // A `restart_delay` with `restart: never` has nothing to throttle: never
    // installs a KeepAlive at all, so an emitted ThrottleInterval would be
    // meaningless.
    let spec = DaemonSpec {
        restart: Restart::Never,
        restart_delay: Some(Duration::from_secs(2)),
        ..minimal_spec()
    };
    let text = plist(&spec, &minimal_identity());
    assert!(
        !text.contains("ThrottleInterval"),
        "restart: never must omit ThrottleInterval:\n{text}"
    );
}

// Disabled is never emitted ===========================================================================================

#[skuld::test]
fn disabled_key_is_never_emitted() {
    for restart in [Restart::Never, Restart::OnFailure, Restart::Always] {
        let text = plist(&spec_with_restart(restart), &minimal_identity());
        assert!(
            !text.contains("Disabled"),
            "the generated plist must never mention Disabled ({restart:?}):\n{text}"
        );
    }

    // And explicitly: the text must be byte-identical regardless of any
    // notion of "enabled" — which does not exist as spec data at all, so
    // this is really just restating that `plist` takes no such parameter.
    let a = plist(&full_spec(), &full_identity());
    let b = plist(&full_spec(), &full_identity());
    assert_eq!(a, b);
}

// The base64 alphabet cannot break out of an XML comment ==============================================================

#[skuld::test]
fn base64_alphabet_cannot_produce_a_comment_terminator() {
    // Standard base64's alphabet is A-Z, a-z, 0-9, +, /, and the `=` padding
    // character — none of which is `-`, so an encoded blob can never
    // contain the `--` that XML forbids inside a comment (and which would,
    // if it could occur, let a crafted spec value break out of ours).
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
    assert!(!ALPHABET.contains(&b'-'), "base64's alphabet must not contain `-`");

    let encoded = blob::encode(&full_spec());
    assert!(
        !encoded.contains("--"),
        "an encoded blob must never contain `--`: {encoded}"
    );
}

// Non-UTF-8 paths ======================================================================================================

#[cfg(windows)]
fn non_utf8_path() -> PathBuf {
    // A lone UTF-16 surrogate: a byte sequence `OsString` can hold on
    // Windows but which is never valid UTF-8.
    use std::os::windows::ffi::OsStringExt as _;
    PathBuf::from(std::ffi::OsString::from_wide(&[0xD800]))
}
#[cfg(unix)]
fn non_utf8_path() -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    PathBuf::from(std::ffi::OsString::from_vec(vec![0xFF, 0xFE]))
}

#[skuld::test]
fn non_utf8_path_panics_loudly_rather_than_emit_a_corrupted_path() {
    // `DaemonSpec` is only ever legitimately constructed from `String`
    // sources (see `spec::resolve`/`blob::decode`), so a non-UTF-8 `cwd`
    // violates that contract. `to_string_lossy` would silently substitute
    // U+FFFD and emit a plist naming a path that does not exist; the
    // generator must panic instead.
    let spec = DaemonSpec {
        cwd: Some(non_utf8_path()),
        ..minimal_spec()
    };

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plist(&spec, &minimal_identity())));
    std::panic::set_hook(previous_hook);

    assert!(result.is_err(), "plist() must panic on a non-UTF-8 cwd");
}
