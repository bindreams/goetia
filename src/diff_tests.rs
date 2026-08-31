use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::spec::{Id, Kind, Restart, User};

// `DaemonSpec` is built literally here, never via `resolve()`: a resolved
// path renders with `\` on Windows and `/` elsewhere, which would make
// these assertions platform-dependent. Every path below is a single
// component for the same reason.
fn spec_fixture() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "FRP tunnel client".to_owned(),
        command: vec!["frpc".to_owned(), "-c".to_owned(), "frpc.toml".to_owned()],
        cwd: Some(PathBuf::from("app")),
        env: BTreeMap::from([("FRP_LOG_LEVEL".to_owned(), "info".to_owned())]),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: Some(PathBuf::from("frpc.log")),
        kind: Kind::Simple,
    }
}

#[skuld::test]
fn artifact_diff_shows_a_hand_edit_the_spec_cannot_express() {
    let was = "\
[Unit]
Description=frpc

[Service]
ExecStart=/usr/bin/frpc
Restart=on-failure

[Install]
WantedBy=multi-user.target
";
    // A hand-added `MemoryMax=8G` is not expressible in `DaemonSpec` at
    // all, so a spec-level diff would render this pair as identical — the
    // one outcome `artifact_diff` exists to avoid.
    let now = "\
[Unit]
Description=frpc

[Service]
ExecStart=/usr/bin/frpc
Restart=on-failure
MemoryMax=8G

[Install]
WantedBy=multi-user.target
";

    let diff = artifact_diff(was, now);

    assert!(
        diff.contains("+MemoryMax=8G"),
        "artifact diff should show the added directive as an insertion: {diff}"
    );
}

#[skuld::test]
fn spec_diff_names_source_keys() {
    let was = spec_fixture();
    let mut now = spec_fixture();
    now.restart_delay = Some(Duration::from_secs(2));

    let diff = spec_diff(&was, &now);

    assert!(
        diff.contains("restart-delay"),
        "spec diff should name the source key `restart-delay`: {diff}"
    );
    assert!(
        !diff.contains("restart_delay"),
        "spec diff should not leak the Rust field name `restart_delay`: {diff}"
    );
}

#[skuld::test]
fn render_yaml_is_key_sorted_and_deterministic() {
    let spec = spec_fixture();

    let first = render_yaml(&spec);
    let second = render_yaml(&spec);
    assert_eq!(first, second, "rendering the same spec twice should be byte-identical");

    let parsed: serde_yaml_ng::Mapping =
        serde_yaml_ng::from_str(&first).expect("render_yaml output should parse as YAML");
    let keys: Vec<&str> = parsed
        .keys()
        .map(|k| k.as_str().expect("every key is a string"))
        .collect();
    let mut sorted_keys = keys.clone();
    sorted_keys.sort_unstable();
    assert_eq!(keys, sorted_keys, "top-level keys should already be sorted: {first}");
}

#[skuld::test]
fn render_yaml_uses_source_key_names() {
    let mut spec = spec_fixture();
    spec.restart_delay = Some(Duration::from_secs(2));

    let rendered = render_yaml(&spec);

    assert!(
        rendered.contains("restart-delay:"),
        "expected source key `restart-delay`: {rendered}"
    );
    assert!(rendered.contains("type:"), "expected source key `type`: {rendered}");
    assert!(
        !rendered.contains("restart_delay"),
        "should not leak the Rust field name `restart_delay`: {rendered}"
    );
    assert!(
        !rendered.contains("kind:"),
        "should not leak the Rust field name `kind`: {rendered}"
    );
}

#[skuld::test]
fn identical_inputs_produce_empty_diff() {
    let spec = spec_fixture();
    let same = spec_fixture();

    assert_eq!(spec_diff(&spec, &same), "", "identical specs should produce no diff");
    assert_eq!(
        artifact_diff("same text\n", "same text\n"),
        "",
        "identical artifact text should produce no diff"
    );
}
