use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use skuld::temp_dir;

use super::*;
use crate::spec::test_support::{abs, program};
use crate::spec::{
    DaemonFields, DaemonSpec, HashSeed, Protocol, Restart, Schedule, ServiceFields, ServiceSpec, TaskFields, TaskSpec,
};
use crate::testing::{base_fields, service_fields};

// The paths are absolute because `ServiceSpec::new` requires it; no assertion reads them, so their
// platform spelling does not matter.
fn spec_fixture() -> DaemonSpec {
    ServiceSpec::new({
        let base = base_fields("frpc", vec![program("frpc"), "-c".to_owned(), "frpc.toml".to_owned()]);
        ServiceFields {
            common: DaemonFields {
                name: "FRP tunnel client".to_owned(),
                cwd: Some(abs("app")),
                env: BTreeMap::from([("FRP_LOG_LEVEL".to_owned(), "info".to_owned())]),
                logs: Some(abs("frpc.log")),
                ..base.common
            },
            restart: Restart::OnFailure,
            protocol: Protocol::Plain,
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap()
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
    let mut now = service_fields(spec_fixture());
    now.restart_delay = Some(Duration::from_secs(2));
    let now = ServiceSpec::new(now).map(DaemonSpec::from).unwrap();

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

    let parsed: yaml_serde::Mapping = yaml_serde::from_str(&first).expect("render_yaml output should parse as YAML");
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
    let mut spec = service_fields(spec_fixture());
    spec.restart_delay = Some(Duration::from_secs(2));
    let spec = ServiceSpec::new(spec).map(DaemonSpec::from).unwrap();

    let rendered = render_yaml(&spec);

    assert!(
        rendered.contains("restart-delay:"),
        "expected source key `restart-delay`: {rendered}"
    );
    assert!(rendered.contains("type:"), "expected source key `type`: {rendered}");
    assert!(
        rendered.contains("protocol:"),
        "expected source key `protocol`: {rendered}"
    );
    assert!(
        !rendered.contains("restart_delay"),
        "should not leak the Rust field name `restart_delay`: {rendered}"
    );
    assert!(
        !rendered.contains("kind:"),
        "should not leak the Rust field name `kind`: {rendered}"
    );
}

fn task_fixture(schedule: Option<&str>) -> DaemonSpec {
    task_fixture_on("build-host", schedule)
}

/// A task whose `H` resolves for `hostname`.
fn task_fixture_on(hostname: &str, schedule: Option<&str>) -> DaemonSpec {
    let base = base_fields("nightly", vec![program("backup")]);
    let schedule = schedule.map(|text| {
        let seed = HashSeed {
            hostname,
            id: &base.common.id,
        };
        Schedule::parse(text, &seed).unwrap()
    });
    TaskSpec::new(TaskFields {
        common: base.common,
        schedule,
    })
    .map(DaemonSpec::from)
    .unwrap()
}

fn rendered_keys(spec: &DaemonSpec) -> yaml_serde::Mapping {
    yaml_serde::from_str(&render_yaml(spec)).expect("render_yaml output should parse as YAML")
}

#[skuld::test]
fn a_service_renders_its_type_and_protocol() {
    for (protocol, key) in [(Protocol::Plain, "plain"), (Protocol::Native, "native")] {
        let mut fields = service_fields(spec_fixture());
        fields.protocol = protocol;
        let spec = ServiceSpec::new(fields).map(DaemonSpec::from).unwrap();

        let keys = rendered_keys(&spec);

        assert_eq!(keys["type"].as_str(), Some("service"));
        assert_eq!(keys["protocol"].as_str(), Some(key));
        assert!(!keys.contains_key("schedule"));
    }
}

#[skuld::test]
fn a_task_renders_its_type_and_schedule_and_no_service_keys() {
    let keys = rendered_keys(&task_fixture(Some("30 3 * * *")));

    assert_eq!(keys["type"].as_str(), Some("task"));
    assert_eq!(keys["schedule"].as_str(), Some("30 3 * * *"));
    for service_key in ["protocol", "restart", "restart-delay"] {
        assert!(!keys.contains_key(service_key), "{service_key}");
    }
}

#[skuld::test]
fn a_task_without_a_schedule_renders_none() {
    let keys = rendered_keys(&task_fixture(None));

    assert_eq!(keys["type"].as_str(), Some("task"));
    assert!(!keys.contains_key("schedule"));
}

#[skuld::test]
fn spec_diff_shows_a_changed_schedule() {
    let diff = spec_diff(&task_fixture(Some("30 3 * * *")), &task_fixture(Some("0 4 * * *")));

    assert!(diff.contains("-schedule: 30 3 * * *"), "{diff}");
    assert!(diff.contains("+schedule: 0 4 * * *"), "{diff}");
}

#[skuld::test]
fn render_yaml_shows_what_h_resolved_to() {
    let (a, b) = (
        task_fixture_on("build-host", Some("H 3 * * *")),
        task_fixture_on("other-host", Some("H 3 * * *")),
    );
    assert_ne!(a, b, "the host names must resolve `H` differently");
    let (was, now) = (render_yaml(&a), render_yaml(&b));
    assert!(was.contains("schedule: H 3 * * *  # resolves to: 13 3 * * *"), "{was}");
    assert_ne!(was, now);
    assert_ne!(spec_diff(&a, &b), "");
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

/// `spec` rendered by [`render_yaml`] and read back as a manifest in `dir`.
fn resolved_from_rendering(spec: &DaemonSpec, dir: &Path) -> DaemonSpec {
    std::fs::write(dir.join("goetia.yaml"), render_yaml(spec)).expect("the manifest is writable");
    crate::spec::load(dir).expect("a rendered spec is a manifest").0
}

#[skuld::test]
fn render_yaml_resolves_back_to_the_service(#[fixture(temp_dir)] dir: &Path) {
    for protocol in [Protocol::Plain, Protocol::Native] {
        let mut fields = service_fields(spec_fixture());
        fields.protocol = protocol;
        fields.restart_delay = Some(Duration::from_secs(2));
        let spec = ServiceSpec::new(fields).map(DaemonSpec::from).unwrap();

        assert_eq!(resolved_from_rendering(&spec, dir), spec, "{protocol:?}");
    }
}

#[skuld::test]
fn render_yaml_resolves_back_to_the_task(#[fixture(temp_dir)] dir: &Path) {
    // A schedule without `H`: its values do not depend on the host, which `load` reads.
    for schedule in [Some("30 3 * * 1-5"), Some("*/15 0,12 1 * *"), None] {
        let spec = task_fixture(schedule);

        assert_eq!(resolved_from_rendering(&spec, dir), spec, "{schedule:?}");
    }
}
