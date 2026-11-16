use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use skuld::temp_dir;

use super::*;
use crate::spec::test_support::{abs, program};
use crate::spec::{
    DaemonFields, DaemonSpec, HashSeed, Protocol, Restart, Schedule, ServiceFields, ServiceSpec, StopAction,
    StopSignal, TaskFields, TaskSpec,
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
        stop_action: Default::default(),
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

/// (schedule text, its `schedule:` line for `nightly` on `build-host`)
const H_SCHEDULES: &[(&str, &str)] = &[
    // yaml quotes a text that starts with `*`
    ("*/5 H * * *", "schedule: '*/5 H * * *'  # resolves to: */5 2 * * *"),
    ("H H H H H", "schedule: H H H H H  # resolves to: 13 2 28 2 0"),
    (
        "H(0-29)/10 * * * *",
        "schedule: H(0-29)/10 * * * *  # resolves to: 3,13,23 * * * *",
    ),
    (
        "H,5 H(0-11) * * H(1-5)",
        "schedule: H,5 H(0-11) * * H(1-5)  # resolves to: 5,13 2 * * 4",
    ),
];

#[skuld::test]
fn the_h_resolution_is_the_comment_of_the_schedule_line() {
    for (text, line) in H_SCHEDULES {
        let rendered = render_yaml(&task_fixture(Some(text)));
        assert!(rendered.lines().any(|l| l == *line), "{text:?}: {rendered}");
    }
}

#[skuld::test]
fn an_annotated_schedule_resolves_back_to_the_task(#[fixture(temp_dir)] dir: &Path) {
    // `load` seeds `H` from this host's name.
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    for (text, _) in H_SCHEDULES {
        let spec = task_fixture_on(&host, Some(text));

        assert_eq!(resolved_from_rendering(&spec, dir), spec, "{text:?}");
    }
}

#[skuld::test]
fn specs_that_resolve_alike_diff_empty_whatever_the_host() {
    // `host0` and `host1` hash differently and both pick 0 of `H(0-1)`.
    let (a, b) = (
        task_fixture_on("host0", Some("H(0-1) * * * *")),
        task_fixture_on("host1", Some("H(0-1) * * * *")),
    );
    assert_eq!(a, b);
    assert_eq!(spec_diff(&a, &b), "");
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

// Stop action =========================================================================================================

fn with_stop_action(spec: DaemonSpec, stop_action: StopAction) -> DaemonSpec {
    match spec {
        DaemonSpec::Service(service) => ServiceSpec::new(ServiceFields {
            stop_action,
            ..service.into_fields()
        })
        .map(DaemonSpec::from)
        .unwrap(),
        DaemonSpec::Task(task) => TaskSpec::new(TaskFields {
            stop_action,
            ..task.into_fields()
        })
        .map(DaemonSpec::from)
        .unwrap(),
    }
}

/// A default is the absence of the key, as for every field: a rendered spec is a manifest, and a
/// Windows task or native service refuses a `stop-action` it was written.
#[skuld::test]
fn the_default_stop_action_renders_no_key() {
    for spec in [spec_fixture(), task_fixture(None)] {
        assert!(!rendered_keys(&spec).contains_key("stop-action"));
    }
}

#[skuld::test]
fn only_the_leaves_that_differ_from_the_default_are_rendered() {
    let render = |send_signal, timeout| {
        let spec = with_stop_action(spec_fixture(), StopAction { send_signal, timeout });
        rendered_keys(&spec)["stop-action"].as_mapping().unwrap().clone()
    };
    let only_signal = render(StopSignal::Quit, Some(Duration::from_secs(30)));
    assert_eq!(only_signal.len(), 1);
    assert_eq!(only_signal["send-signal"].as_str(), Some("quit"));

    let only_timeout = render(StopSignal::Terminate, Some(Duration::from_secs(120)));
    assert_eq!(only_timeout.len(), 1);
    assert_eq!(only_timeout["timeout"].as_str(), Some("2m"));

    // `kill`'s own default is no timeout, so `kill` alone is the whole entry.
    let kill = render(StopSignal::Kill, None);
    assert_eq!(kill.len(), 1);
    assert_eq!(kill["send-signal"].as_str(), Some("kill"));
}

#[skuld::test]
fn no_timeout_renders_as_null_and_kill_as_itself() {
    let patient = with_stop_action(
        spec_fixture(),
        StopAction {
            send_signal: StopSignal::parse("sigusr1").unwrap(),
            timeout: None,
        },
    );
    let keys = rendered_keys(&patient);
    let action = keys["stop-action"].as_mapping().unwrap();
    assert_eq!(action["send-signal"].as_str(), Some("SIGUSR1"));
    assert!(action["timeout"].is_null(), "{action:?}");

    let kill = with_stop_action(
        task_fixture(None),
        StopAction {
            send_signal: StopSignal::Kill,
            timeout: None,
        },
    );
    assert_eq!(
        rendered_keys(&kill)["stop-action"].as_mapping().unwrap()["send-signal"].as_str(),
        Some("kill")
    );
}

#[skuld::test]
fn spec_diff_shows_a_changed_stop_action() {
    let was = spec_fixture();
    let now = with_stop_action(
        spec_fixture(),
        StopAction {
            send_signal: StopSignal::Quit,
            timeout: Some(Duration::from_secs(90)),
        },
    );
    let diff = spec_diff(&was, &now);
    assert!(diff.contains("+stop-action:"), "{diff}");
    assert!(diff.contains("+  send-signal: quit"), "{diff}");
    assert!(diff.contains("+  timeout: 1m 30s"), "{diff}");

    let later = with_stop_action(
        spec_fixture(),
        StopAction {
            send_signal: StopSignal::Quit,
            timeout: None,
        },
    );
    let diff = spec_diff(&now, &later);
    assert!(
        diff.contains("-  timeout: 1m 30s") && diff.contains("+  timeout: null"),
        "{diff}"
    );
}

/// What `show` prints is what a manifest may say: it reads back to the same action. Only the signals
/// every native backend can send are written, so the test is the same on every host.
#[skuld::test]
fn the_rendered_stop_action_reads_back_as_a_manifest_would(#[fixture(temp_dir)] dir: &Path) {
    for (signal, timeout) in [("terminate", Some(45)), ("terminate", None), ("kill", None)] {
        // launchd refuses a wait with no limit, which is its own tests' to judge.
        if timeout.is_none()
            && signal != "kill"
            && crate::spec::BackendKind::native() == Some(crate::spec::BackendKind::Launchd)
        {
            continue;
        }
        let send_signal = StopSignal::parse(signal).unwrap();
        let stop_action = StopAction {
            send_signal,
            timeout: timeout.map(Duration::from_secs),
        };
        let rendered = render_yaml(&with_stop_action(spec_fixture(), stop_action));
        let manifest = format!(
            "id: frpc\ncommand: ['{}']\n{}",
            program("frpc"),
            stop_action_block(&rendered)
        );
        std::fs::write(dir.join("goetia.yaml"), manifest).unwrap();

        let (spec, _) = crate::spec::load(dir).unwrap_or_else(|e| panic!("{rendered}: {e}"));
        let DaemonSpec::Service(service) = spec else {
            panic!("a service")
        };
        assert_eq!(service.stop_action, stop_action, "{rendered}");
    }
}

/// The `stop-action:` entry of `rendered`, with its indented lines.
fn stop_action_block(rendered: &str) -> String {
    let mut lines = rendered.lines().skip_while(|line| !line.starts_with("stop-action:"));
    let mut block = vec![lines.next().expect("a stop-action entry").to_string()];
    block.extend(lines.take_while(|line| line.starts_with(' ')).map(str::to_string));
    block.join("\n") + "\n"
}
