use crate::spec::test_support::abs;
use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::spec::test_support::program;
use crate::spec::{
    DaemonFields, DaemonSpec, HashSeed, Id, Protocol, Schedule, ServiceFields, ServiceSpec, TaskFields, TaskSpec, User,
};
use crate::testing::{base_fields, service_fields};

// Fixtures ============================================================================================================
//
// `ServiceSpec::new` requires absolute `command[0]`, `cwd` and `logs`, which is platform-defined; `abs` builds one
// for the host.

fn full_spec() -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        common: DaemonFields {
            id: Id::try_from("frpc").unwrap(),
            name: "Frpc Tunnel".to_string(),
            command: vec![program("frpc"), "-c".to_string(), "frpc.toml".to_string()],
            cwd: Some(abs("opt/frpc")),
            env: BTreeMap::from([
                ("RUST_LOG".to_string(), "info".to_string()),
                ("URL".to_string(), "http://x/a%20b".to_string()),
            ]),
            user: User::Name("svc-frpc".to_string()),
            logs: Some(abs("var/log/frpc.log")),
        },
        restart: Restart::Always,
        restart_delay: Some(Duration::new(2, 500_000_000)),
        protocol: Protocol::Native,
    })
    .map(DaemonSpec::from)
    .unwrap()
}

fn full_identity() -> Identity {
    Identity {
        user: "svc-frpc".to_string(),
    }
}

fn minimal_spec() -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        protocol: Protocol::Plain,
        ..base_fields("frpc", vec![program("frpc")])
    })
    .map(DaemonSpec::from)
    .unwrap()
}

fn minimal_identity() -> Identity {
    Identity {
        user: "root".to_string(),
    }
}

fn spec_with_restart(restart: Restart) -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        restart,
        ..service_fields(minimal_spec())
    })
    .map(DaemonSpec::from)
    .unwrap()
}

/// The job plist `spec` renders to: the golden tests below read the one text a service has.
fn plist(spec: &DaemonSpec, id: &Identity) -> Result<String, Error> {
    plists(spec, id).map(|plists| plists.job)
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
    let text = plist(&spec, &minimal_identity()).unwrap();

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
         \t\t<string>{program}</string>\n\
         \t</array>\n\
         \t<key>UserName</key>\n\
         \t<string>root</string>\n\
         </dict>\n\
         </plist>\n",
        version = crate::version(),
        blob = blob::encode(&spec),
        program = program("frpc"),
    );

    assert_eq!(text, expected);
}

#[skuld::test]
fn plist_snapshot_full() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity()).unwrap();

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
         \t\t<string>{program}</string>\n\
         \t\t<string>-c</string>\n\
         \t\t<string>frpc.toml</string>\n\
         \t</array>\n\
         \t<key>WorkingDirectory</key>\n\
         \t<string>{cwd}</string>\n\
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
         \t<string>{logs}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{logs}</string>\n\
         </dict>\n\
         </plist>\n",
        version = crate::version(),
        blob = blob::encode(&spec),
        program = program("frpc"),
        cwd = abs("opt/frpc").display(),
        logs = abs("var/log/frpc.log").display(),
    );

    assert_eq!(text, expected);
}

// Round trip and the generation invariant =============================================================================

#[skuld::test]
fn plist_round_trips_through_extract() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity()).unwrap();

    let decoded = extract(&text)
        .expect("extract should succeed on our own output")
        .expect("marker should be present");

    assert_eq!(decoded.spec, spec);
    assert_eq!(decoded.schema, blob::SCHEMA);
    assert_eq!(decoded.version, crate::version());
}

#[skuld::test]
fn plist_is_the_generation_invariant() {
    let spec = full_spec();
    let id = full_identity();
    let text = plist(&spec, &id).unwrap();

    let decoded = extract(&text).unwrap().unwrap();
    let regenerated = plist(&decoded.spec, &id).unwrap();

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
fn extract_errors_on_corrupt_blob_in_plist() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity()).unwrap();

    // Corrupt the base64 payload embedded in the comment, without disturbing
    // the surrounding sentinel structure. The comment is still recognizably
    // "ours" (the marker is present) but the blob no longer decodes.
    let corrupted = text.replacen("Spec: ", "Spec: !!!not-base64!!!", 1);

    let err = extract(&corrupted).expect_err("a corrupt blob must error, not silently decode");
    assert!(matches!(err, Error::Blob { .. }), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_on_duplicate_metadata_comment() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity()).unwrap();

    // Two well-formed metadata comments in one document.
    let comment_end = text.find("-->").expect("fixture must contain a comment") + "-->".len();
    let mut doubled = text[..comment_end].to_string();
    doubled.push('\n');
    doubled.push_str(&text[..comment_end]);
    doubled.push_str(&text[comment_end..]);

    let err = extract(&doubled).expect_err("more than one metadata comment must error");
    assert!(matches!(err, Error::Blob { .. }), "expected a Blob error, got {err:?}");
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
    assert!(matches!(err, Error::Blob { .. }), "expected a Blob error, got {err:?}");
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
    assert!(matches!(err, Error::Blob { .. }), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_errors_on_marker_mismatch() {
    let spec = full_spec();
    let text = plist(&spec, &full_identity()).unwrap();
    let tampered = text.replacen("Marker: goetia", "Marker: not-goetia", 1);

    let err = extract(&tampered).expect_err("a metadata comment naming the wrong marker must error");
    assert!(matches!(err, Error::Blob { .. }), "expected a Blob error, got {err:?}");
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
    assert!(matches!(err, Error::Blob { .. }), "expected a Blob error, got {err:?}");
}

#[skuld::test]
fn extract_tolerates_crlf_line_endings_in_the_metadata_comment() {
    // A plist that has been saved with CRLF line endings (e.g. by a Windows
    // editor) still carries a fully intact metadata comment — the sentinel
    // match must not be tied to the exact `\n` byte.
    let spec = full_spec();
    let text = plist(&spec, &full_identity()).unwrap();
    let crlf = text.replace('\n', "\r\n");

    let decoded = extract(&crlf)
        .expect("extract should tolerate CRLF line endings")
        .expect("marker should still be recognized after CRLF conversion");

    assert_eq!(decoded.spec, spec);
}

// Escaping ============================================================================================================

#[skuld::test]
fn xml_special_characters_are_escaped() {
    let mut spec = service_fields(minimal_spec());
    spec.common.command = vec![program("frpc"), "a&b<c>d".to_string()];
    spec.common.env = BTreeMap::from([("K&L<M>N".to_string(), "x&y<z>w".to_string())]);
    spec.common.cwd = Some(abs("opt/a&b<c>d"));
    spec.common.logs = Some(abs("var/log/a&b<c>d"));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();

    let text = plist(
        &spec,
        &Identity {
            user: "svc&name<x>y".to_string(),
        },
    )
    .unwrap();

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
        text.contains(&format!(
            "<key>WorkingDirectory</key>\n\t<string>{}</string>",
            abs("opt/a&amp;b&lt;c&gt;d").display()
        )),
        "cwd was not escaped:\n{text}"
    );
    assert!(
        text.contains(&format!(
            "<key>StandardOutPath</key>\n\t<string>{}</string>",
            abs("var/log/a&amp;b&lt;c&gt;d").display()
        )),
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

// restart -> KeepAlive ================================================================================================

#[skuld::test]
fn restart_variants_map_to_keepalive() {
    let never = plist(&spec_with_restart(Restart::Never), &minimal_identity()).unwrap();
    assert!(
        !never.contains("<key>KeepAlive</key>"),
        "never must omit KeepAlive:\n{never}"
    );
    assert!(
        !never.contains("<key>RunAtLoad</key>"),
        "never must omit RunAtLoad:\n{never}"
    );

    let on_failure = plist(&spec_with_restart(Restart::OnFailure), &minimal_identity()).unwrap();
    assert!(
        on_failure.contains("<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n"),
        "on-failure must emit a KeepAlive dict with SuccessfulExit: false:\n{on_failure}"
    );
    assert!(on_failure.contains("<key>RunAtLoad</key>\n\t<true/>\n"));

    let always = plist(&spec_with_restart(Restart::Always), &minimal_identity()).unwrap();
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
        (Duration::new(1, 500_000_000), 2), // 1.5s -> 2: a fraction rounds up, never down
        (Duration::from_millis(500), 1),    // 500ms would truncate to 0, which disables throttling
        (Duration::from_secs(2), 2),        // already whole: unchanged
        (Duration::ZERO, 0),                // explicit opt-out is left alone, not "rounded"
    ];

    for (delay, expected) in cases {
        let spec = ServiceSpec::new(ServiceFields {
            restart: Restart::Always,
            restart_delay: Some(delay),
            ..service_fields(minimal_spec())
        })
        .map(DaemonSpec::from)
        .unwrap();
        let text = plist(&spec, &minimal_identity()).unwrap();
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
    let spec = ServiceSpec::new(ServiceFields {
        restart: Restart::Always,
        restart_delay: Some(Duration::MAX),
        ..service_fields(minimal_spec())
    })
    .map(DaemonSpec::from)
    .unwrap();
    let text = plist(&spec, &minimal_identity()).unwrap();
    let needle = format!("<key>ThrottleInterval</key>\n\t<integer>{}</integer>\n", u64::MAX);
    assert!(
        text.contains(&needle),
        "Duration::MAX should saturate, not overflow:\n{text}"
    );
}

#[skuld::test]
fn throttle_interval_omitted_without_restart_delay() {
    let spec = ServiceSpec::new(ServiceFields {
        restart: Restart::Always,
        restart_delay: None,
        ..service_fields(minimal_spec())
    })
    .map(DaemonSpec::from)
    .unwrap();
    let text = plist(&spec, &minimal_identity()).unwrap();
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
    let spec = ServiceSpec::new(ServiceFields {
        restart: Restart::Never,
        restart_delay: Some(Duration::from_secs(2)),
        ..service_fields(minimal_spec())
    })
    .map(DaemonSpec::from)
    .unwrap();
    let text = plist(&spec, &minimal_identity()).unwrap();
    assert!(
        !text.contains("ThrottleInterval"),
        "restart: never must omit ThrottleInterval:\n{text}"
    );
}

// Disabled is never emitted ===========================================================================================

#[skuld::test]
fn disabled_key_is_never_emitted() {
    for restart in [Restart::Never, Restart::OnFailure, Restart::Always] {
        let text = plist(&spec_with_restart(restart), &minimal_identity()).unwrap();
        assert!(
            !text.contains("Disabled"),
            "the generated plist must never mention Disabled ({restart:?}):\n{text}"
        );
    }

    // And explicitly: the text must be byte-identical regardless of any
    // notion of "enabled" — which does not exist as spec data at all, so
    // this is really just restating that `plist` takes no such parameter.
    let a = plist(&full_spec(), &full_identity()).unwrap();
    let b = plist(&full_spec(), &full_identity()).unwrap();
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

// without_metadata ====================================================================================================

#[skuld::test]
fn without_metadata_drops_the_whole_comment_and_keeps_the_rest() {
    let text = plist(&minimal_spec(), &minimal_identity()).unwrap();
    let stripped = without_metadata(&text);

    assert!(!stripped.contains("goetia"), "{stripped}");
    assert!(!stripped.contains(&blob::encode(&minimal_spec())), "{stripped}");
    assert!(stripped.contains("<key>Label</key>"), "{stripped}");
    assert!(stripped.contains("<!DOCTYPE plist"), "{stripped}");
    assert!(
        !stripped.contains("\n\n"),
        "no blank line is left where the comment was: {stripped}"
    );
}

#[skuld::test]
fn without_metadata_makes_a_spec_change_the_plist_does_not_render_vanish() {
    // `name` is in the blob and nowhere in the plist.
    let renamed = ServiceSpec::new(ServiceFields {
        common: DaemonFields {
            name: "another name".to_string(),
            ..service_fields(minimal_spec()).common
        },
        ..service_fields(minimal_spec())
    })
    .map(DaemonSpec::from)
    .unwrap();
    let before = plist(&minimal_spec(), &minimal_identity()).unwrap();
    let after = plist(&renamed, &minimal_identity()).unwrap();

    assert_ne!(before, after);
    assert_eq!(without_metadata(&before), without_metadata(&after));
}

#[skuld::test]
fn without_metadata_makes_a_version_bump_vanish() {
    let text = plist(&minimal_spec(), &minimal_identity()).unwrap();
    let bumped = text.replace(&format!("Version: {}", crate::version()), "Version: 999.0.0");

    assert_ne!(text, bumped);
    assert_eq!(without_metadata(&text), without_metadata(&bumped));
}

#[skuld::test]
fn without_metadata_keeps_what_launchd_reads() {
    let before = plist(&minimal_spec(), &minimal_identity()).unwrap();
    let other_user = plist(
        &minimal_spec(),
        &Identity {
            user: "svc".to_string(),
        },
    )
    .unwrap();
    let other_command = plist(
        &ServiceSpec::new(ServiceFields {
            common: DaemonFields {
                command: vec![program("frpc"), "--verbose".to_string()],
                ..service_fields(minimal_spec()).common
            },
            ..service_fields(minimal_spec())
        })
        .map(DaemonSpec::from)
        .unwrap(),
        &minimal_identity(),
    )
    .unwrap();

    assert_ne!(without_metadata(&before), without_metadata(&other_user));
    assert_ne!(without_metadata(&before), without_metadata(&other_command));
}

#[skuld::test]
fn without_metadata_handles_crlf() {
    let text = plist(&minimal_spec(), &minimal_identity()).unwrap();
    let crlf = text.replace('\n', "\r\n");

    assert_eq!(without_metadata(&crlf), without_metadata(&text).replace('\n', "\r\n"));
}

#[skuld::test]
fn without_metadata_leaves_a_plist_with_no_comment_and_an_unterminated_one_as_they_are() {
    let foreign = "<?xml version=\"1.0\"?>\n<plist><dict/></plist>\n";
    assert_eq!(without_metadata(foreign), foreign);

    let unterminated = "<!-- goetia:begin\nMarker: goetia\n<plist/>\n";
    assert_eq!(without_metadata(unterminated), unterminated);
}

#[skuld::test]
fn without_metadata_drops_every_terminated_comment() {
    let two = "<a/>\n<!-- goetia:begin\nx\ngoetia:end -->\n<b/>\n<!-- goetia:begin\ny\ngoetia:end -->\n<c/>\n";
    assert_eq!(without_metadata(two), "<a/>\n<b/>\n<c/>\n");
}

#[skuld::test]
fn without_metadata_drops_a_terminated_comment_and_keeps_an_unterminated_one_after_it() {
    let text = "<a/>\n<!-- goetia:begin\nx\ngoetia:end -->\n<b/>\n<!-- goetia:begin\ny\n<c/>\n";
    assert_eq!(without_metadata(text), "<a/>\n<b/>\n<!-- goetia:begin\ny\n<c/>\n");
}

// Tasks ===============================================================================================================

fn schedule(text: &str) -> Schedule {
    let id = Id::try_from("frpc").unwrap();
    Schedule::parse(
        text,
        &HashSeed {
            hostname: "testing",
            id: &id,
        },
    )
    .unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

/// A task with `full_spec`'s env, cwd, user and logs, on `schedule_text` (`None`: no schedule).
fn full_task(schedule_text: Option<&str>) -> DaemonSpec {
    TaskSpec::new(TaskFields {
        common: service_fields(full_spec()).common,
        schedule: schedule_text.map(schedule),
    })
    .map(DaemonSpec::from)
    .unwrap()
}

/// A root task with no `cwd`, `env` or `logs`, on `30 3 * * *`, that `change` then varies.
fn task_with(change: impl FnOnce(&mut TaskFields)) -> DaemonSpec {
    let mut fields = TaskFields {
        common: service_fields(minimal_spec()).common,
        schedule: Some(schedule("30 3 * * *")),
    };
    change(&mut fields);
    TaskSpec::new(fields).map(DaemonSpec::from).unwrap()
}

/// A plist as the generator lays it out, around `body`: the dict's keys, one per line, tab-indented.
fn document(spec: &DaemonSpec, body: &str) -> String {
    format!(
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
         {body}\
         </dict>\n\
         </plist>\n",
        version = crate::version(),
        blob = blob::encode(spec),
    )
}

/// What a task with `logs` runs through: `sh` prints the run's marker and the pid, as 8 lowercase hex
/// digits, then execs the command. Spelled out, not built from the generator's constant, so the bytes
/// are pinned.
const SCRIPT: &str = r#"printf '%s %08x\n' goetia-run $$; exec "$0" "$@""#;

fn job_of(spec: &DaemonSpec) -> String {
    plists(spec, &full_identity()).unwrap().job
}

fn timer_of(spec: &DaemonSpec) -> Option<String> {
    plists(spec, &full_identity()).unwrap().timer
}

#[skuld::test]
fn service_plist_is_unchanged() {
    // The snapshots above are the golden bytes; this pins that a service has no timer and that
    // `Display` is its plist alone.
    for (spec, id) in [(full_spec(), full_identity()), (minimal_spec(), minimal_identity())] {
        let plists = plists(&spec, &id).unwrap();
        assert_eq!(plists.timer, None);
        assert_eq!(plists.to_string(), plists.job);
        assert_eq!(plist(&spec, &id).unwrap(), plists.job);
    }
}

#[skuld::test]
fn task_job_text_is_exact() {
    let spec = full_task(Some("30 3 * * *"));
    let expected = document(
        &spec,
        &format!(
            "\t<key>Label</key>\n\
             \t<string>frpc</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>/bin/sh</string>\n\
             \t\t<string>-c</string>\n\
             \t\t<string>{SCRIPT}</string>\n\
             \t\t<string>{program}</string>\n\
             \t\t<string>-c</string>\n\
             \t\t<string>frpc.toml</string>\n\
             \t</array>\n\
             \t<key>WorkingDirectory</key>\n\
             \t<string>{cwd}</string>\n\
             \t<key>EnvironmentVariables</key>\n\
             \t<dict>\n\
             \t\t<key>RUST_LOG</key>\n\
             \t\t<string>info</string>\n\
             \t\t<key>URL</key>\n\
             \t\t<string>http://x/a%20b</string>\n\
             \t</dict>\n\
             \t<key>UserName</key>\n\
             \t<string>svc-frpc</string>\n\
             \t<key>ThrottleInterval</key>\n\
             \t<integer>1</integer>\n\
             \t<key>StandardOutPath</key>\n\
             \t<string>{logs}</string>\n\
             \t<key>StandardErrorPath</key>\n\
             \t<string>{logs}</string>\n",
            program = program("frpc"),
            cwd = abs("opt/frpc").display(),
            logs = abs("var/log/frpc.log").display(),
        ),
    );
    assert_eq!(job_of(&spec), expected);

    // No `logs`: the command is `ProgramArguments`, with no `sh` and no log keys.
    let spec = task_with(|_| {});
    let expected = document(
        &spec,
        &format!(
            "\t<key>Label</key>\n\
             \t<string>frpc</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>{program}</string>\n\
             \t</array>\n\
             \t<key>UserName</key>\n\
             \t<string>svc-frpc</string>\n\
             \t<key>ThrottleInterval</key>\n\
             \t<integer>1</integer>\n",
            program = program("frpc"),
        ),
    );
    assert_eq!(job_of(&spec), expected);
}

/// With no `KeepAlive` the default throttle would hold a `kickstart` for 10 s after the last start; a
/// task never restarts, so the interval guards nothing.
#[skuld::test]
fn a_task_job_always_carries_throttle_interval_1() {
    for logs in [true, false] {
        for scheduled in [true, false] {
            let spec = task_with(|f| {
                f.common.logs = logs.then(|| abs("var/log/frpc.log"));
                f.schedule = f.schedule.take().filter(|_| scheduled);
            });
            let text = job_of(&spec);
            assert!(
                text.contains("\t<key>ThrottleInterval</key>\n\t<integer>1</integer>\n"),
                "logs {logs}, scheduled {scheduled}:\n{text}"
            );
            assert_eq!(text.matches("ThrottleInterval").count(), 1, "{text}");
        }
    }
}

#[skuld::test]
fn a_task_without_logs_has_no_wrapper() {
    // Its output goes nowhere launchd keeps, so there is nothing for a marker to attribute.
    let text = job_of(&task_with(|_| {}));
    for absent in [
        "/bin/sh",
        "printf",
        "goetia-run",
        "StandardOutPath",
        "StandardErrorPath",
    ] {
        assert!(!text.contains(absent), "{absent}:\n{text}");
    }

    let text = job_of(&task_with(|f| f.common.logs = Some(abs("var/log/frpc.log"))));
    assert!(
        text.contains(&format!(
            "\t\t<string>/bin/sh</string>\n\t\t<string>-c</string>\n\t\t<string>{SCRIPT}</string>\n\t\t<string>{}</string>\n",
            program("frpc")
        )),
        "{text}"
    );
}

#[skuld::test]
fn the_wrapper_prints_the_shared_run_marker() {
    assert!(SCRIPT.contains(crate::backend::RUN_MARKER), "{SCRIPT}");
}

#[skuld::test]
fn timer_text_is_exact() {
    let kickstart = "\t<key>Label</key>\n\
                     \t<string>frpc@timer</string>\n\
                     \t<key>ProgramArguments</key>\n\
                     \t<array>\n\
                     \t\t<string>/bin/launchctl</string>\n\
                     \t\t<string>kickstart</string>\n\
                     \t\t<string>system/frpc</string>\n\
                     \t</array>\n";

    let spec = full_task(Some("30 3 * * *"));
    let expected = document(
        &spec,
        &format!(
            "{kickstart}\
             \t<key>StartCalendarInterval</key>\n\
             \t<array>\n\
             \t\t<dict>\n\
             \t\t\t<key>Minute</key>\n\
             \t\t\t<integer>30</integer>\n\
             \t\t\t<key>Hour</key>\n\
             \t\t\t<integer>3</integer>\n\
             \t\t</dict>\n\
             \t</array>\n"
        ),
    );
    assert_eq!(timer_of(&spec), Some(expected));

    // An `Or` schedule: the days, then the weekday.
    let spec = full_task(Some("0 0 1,15 * 1"));
    let expected = document(
        &spec,
        &format!(
            "{kickstart}\
             \t<key>StartCalendarInterval</key>\n\
             \t<array>\n\
             \t\t<dict>\n\
             \t\t\t<key>Minute</key>\n\
             \t\t\t<integer>0</integer>\n\
             \t\t\t<key>Hour</key>\n\
             \t\t\t<integer>0</integer>\n\
             \t\t\t<key>Day</key>\n\
             \t\t\t<integer>1</integer>\n\
             \t\t</dict>\n\
             \t\t<dict>\n\
             \t\t\t<key>Minute</key>\n\
             \t\t\t<integer>0</integer>\n\
             \t\t\t<key>Hour</key>\n\
             \t\t\t<integer>0</integer>\n\
             \t\t\t<key>Day</key>\n\
             \t\t\t<integer>15</integer>\n\
             \t\t</dict>\n\
             \t\t<dict>\n\
             \t\t\t<key>Minute</key>\n\
             \t\t\t<integer>0</integer>\n\
             \t\t\t<key>Hour</key>\n\
             \t\t\t<integer>0</integer>\n\
             \t\t\t<key>Weekday</key>\n\
             \t\t\t<integer>1</integer>\n\
             \t\t</dict>\n\
             \t</array>\n"
        ),
    );
    assert_eq!(timer_of(&spec), Some(expected));
}

#[skuld::test]
fn a_month_is_the_last_key_of_a_dict() {
    let text = timer_of(&full_task(Some("15 4 1 6 *"))).unwrap();
    assert!(
        text.contains(
            "\t\t\t<key>Minute</key>\n\t\t\t<integer>15</integer>\n\t\t\t<key>Hour</key>\n\t\t\t<integer>4</integer>\n\
             \t\t\t<key>Day</key>\n\t\t\t<integer>1</integer>\n\t\t\t<key>Month</key>\n\t\t\t<integer>6</integer>\n"
        ),
        "{text}"
    );
}

#[skuld::test]
fn task_without_schedule_has_no_timer() {
    let spec = task_with(|f| f.schedule = None);
    let plists = plists(&spec, &full_identity()).unwrap();
    assert_eq!(plists.timer, None);
    assert_eq!(plists.to_string(), plists.job);
}

/// `0 0 30 2 *` names no date that exists: the timer is armed with nothing to fire on, and has no
/// `StartCalendarInterval`, which an empty array or dict would not be.
#[skuld::test]
fn a_schedule_with_no_date_has_a_timer_without_the_calendar_key() {
    let spec = full_task(Some("0 0 30 2 *"));
    let expected = document(
        &spec,
        "\t<key>Label</key>\n\
         \t<string>frpc@timer</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>/bin/launchctl</string>\n\
         \t\t<string>kickstart</string>\n\
         \t\t<string>system/frpc</string>\n\
         \t</array>\n",
    );
    let timer = timer_of(&spec).expect("the task has a schedule, so a timer");
    assert_eq!(timer, expected);
    assert!(!timer.contains("StartCalendarInterval"), "{timer}");
    assert_eq!(extract(&timer).unwrap().unwrap().spec, spec);
}

#[skuld::test]
fn a_schedule_that_runs_every_minute_has_one_empty_dict() {
    let timer = timer_of(&full_task(Some("* * * * *"))).unwrap();
    assert!(
        timer.contains("\t<key>StartCalendarInterval</key>\n\t<array>\n\t\t<dict>\n\t\t</dict>\n\t</array>\n"),
        "{timer}"
    );
}

#[skuld::test]
fn timer_for_a_service_is_none() {
    for spec in [full_spec(), minimal_spec(), task_with(|f| f.schedule = None)] {
        assert_eq!(timer_for(&spec).unwrap(), None);
    }
}

#[skuld::test]
fn timer_for_matches_plists_timer() {
    for spec in [
        full_task(Some("30 3 * * *")),
        full_task(Some("0 0 1,15 * 1")),
        full_task(Some("0 0 30 2 *")),
        full_task(Some("* * * * *")),
        full_task(None),
        full_spec(),
    ] {
        assert_eq!(timer_for(&spec).unwrap(), timer_of(&spec));
    }
}

#[skuld::test]
fn both_plists_carry_the_same_blob() {
    let spec = full_task(Some("30 3 * * *"));
    let plists = plists(&spec, &full_identity()).unwrap();
    let timer = plists.timer.as_deref().unwrap();

    let comment = metadata_comment(&spec);
    assert!(plists.job.contains(&comment), "{}", plists.job);
    assert!(timer.contains(&comment), "{timer}");
    assert_eq!(plists.job.matches("Spec: ").count(), 1);
    assert_eq!(timer.matches("Spec: ").count(), 1);
    for text in [&plists.job, timer] {
        assert!(!text.contains("Disabled"), "{text}");
    }
}

#[skuld::test]
fn extract_reads_both() {
    let spec = full_task(Some("30 3 * * *"));
    let plists = plists(&spec, &full_identity()).unwrap();

    for text in [&plists.job, plists.timer.as_ref().unwrap()] {
        let blob = extract(text).unwrap().expect("a marker");
        assert_eq!(blob.spec, spec);
        assert_eq!(blob.schema, blob::SCHEMA);
        assert_eq!(blob.version, crate::version());
    }
}

#[skuld::test]
fn generate_of_extract_is_the_identity_for_a_task() {
    for spec in [
        full_task(Some("30 3 * * *")),
        full_task(Some("0 0 1,15 * 1")),
        full_task(Some("0 0 30 2 *")),
        full_task(None),
    ] {
        let id = full_identity();
        let first = plists(&spec, &id).unwrap();

        let from_job = extract(&first.job).unwrap().unwrap().spec;
        assert_eq!(plists(&from_job, &id).unwrap(), first);
        if let Some(timer) = &first.timer {
            let from_timer = extract(timer).unwrap().unwrap().spec;
            assert_eq!(plists(&from_timer, &id).unwrap(), first);
            assert_eq!(timer_for(&from_timer).unwrap().as_ref(), Some(timer));
        }
    }
}

#[skuld::test]
fn plists_display_puts_the_timer_under_a_comment_line() {
    let spec = full_task(Some("30 3 * * *"));
    let plists = plists(&spec, &full_identity()).unwrap();
    assert_eq!(
        plists.to_string(),
        format!(
            "{}\n<!-- ---- timer plist ---- -->\n{}",
            plists.job,
            plists.timer.unwrap()
        )
    );
}

/// `0 0 29 2 *` is a schedule launchd cannot hold: the refusal is the calendar's, whichever entry
/// point meets it, and names the same daemon and field.
#[skuld::test]
fn a_refused_calendar_fails_plists_and_timer_for_alike() {
    let spec = full_task(Some("0 0 29 2 *"));

    let from_plists = plists(&spec, &full_identity()).unwrap_err();
    let from_timer_for = timer_for(&spec).unwrap_err();

    assert!(matches!(from_plists, Error::Invalid { .. }), "{from_plists:?}");
    assert!(matches!(from_timer_for, Error::Invalid { .. }), "{from_timer_for:?}");
    assert_eq!(from_plists.to_string(), from_timer_for.to_string());
    assert!(from_plists.to_string().contains("29 February"), "{from_plists}");
}

/// Only a task with `logs` starts through `sh`, which resets these on its way to the program (measured
/// on macOS); `SHLVL` and `PS4` pass.
#[skuld::test]
fn an_env_a_wrapper_resets_is_refused_only_with_logs() {
    let logs = || Some(abs("var/log/frpc.log"));
    for key in ["PWD", "IFS", "OLDPWD", "_"] {
        let env = || BTreeMap::from([(key.to_string(), "x".to_string())]);

        let with_logs = task_with(|f| {
            f.common.env = env();
            f.common.logs = logs();
        });
        let err = plists(&with_logs, &full_identity()).unwrap_err();
        let Error::Invalid { daemon, message, .. } = &err else {
            panic!("{key}: expected Error::Invalid, got {err:?}");
        };
        assert_eq!(daemon, "frpc");
        assert_eq!(
            message,
            &format!(
                "`env` sets `{key}`, but a task with `logs` starts through /bin/sh, which resets it, so the program \
                 would not see the value"
            ),
        );
        assert!(timer_for(&with_logs).is_ok(), "{key}: the timer has no env");

        let without_logs = task_with(|f| f.common.env = env());
        assert!(plists(&without_logs, &full_identity()).is_ok(), "{key}");
    }

    for key in ["SHLVL", "PS4"] {
        let spec = task_with(|f| {
            f.common.env = BTreeMap::from([(key.to_string(), "x".to_string())]);
            f.common.logs = logs();
        });
        assert!(plists(&spec, &full_identity()).is_ok(), "{key}");
    }

    // A service starts without `sh`, whatever its env and logs.
    let service = ServiceSpec::new(ServiceFields {
        common: DaemonFields {
            env: BTreeMap::from([("PWD".to_string(), "/x".to_string())]),
            logs: logs(),
            ..service_fields(minimal_spec()).common
        },
        ..service_fields(minimal_spec())
    })
    .map(DaemonSpec::from)
    .unwrap();
    assert!(plists(&service, &full_identity()).is_ok());
}

#[skuld::test]
fn timer_label_is_never_an_id() {
    for id in ["frpc", "a", "a.b-c_d", "A9", &"x".repeat(80)] {
        let label = timer_label(id);
        assert_eq!(label, format!("{id}@timer"));
        assert!(Id::try_from(label.as_str()).is_err(), "{label}");
        assert_eq!(id_of_timer_stem(&label), Some(id), "{label}");
    }
    for stem in [
        "frpc",
        "",
        "@timer",
        "a@timer@timer",
        "a b@timer",
        "a@time",
        "a@timers",
        "a@Timer",
    ] {
        assert_eq!(id_of_timer_stem(stem), None, "{stem:?}");
    }
}

/// The wrapper's words and the command's are escaped as every other value; the script has no `&`, `<`
/// or `>`, so it reads back as written.
#[skuld::test]
fn the_marker_command_words_survive_xml_escaping() {
    let spec = task_with(|f| {
        f.common.command = vec![
            program("frpc"),
            "a&b".to_string(),
            "<c>".to_string(),
            "say \"hi\"".to_string(),
        ];
        f.common.logs = Some(abs("var/log/frpc.log"));
    });
    let text = job_of(&spec);

    for escaped in [
        "<string>a&amp;b</string>",
        "<string>&lt;c&gt;</string>",
        "<string>say \"hi\"</string>",
        &format!("<string>{SCRIPT}</string>"),
    ] {
        assert!(text.contains(escaped), "{escaped}:\n{text}");
    }
    for raw in ["a&b", "<c>"] {
        assert!(!text.contains(raw), "{raw}:\n{text}");
    }
}
