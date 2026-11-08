//! Windows: do unknown values under `Services\<name>\Parameters` survive?
//!
//! Microsoft documents `Parameters` as the place for service-private data the
//! SCM does not read, but documents nothing about pruning. So the values are
//! subjected to every registry-rewriting operation Goetia will perform or
//! invite — `ChangeServiceConfig`, `ChangeServiceConfig2` for failure actions
//! and description, and a genuine start/stop cycle — and read back byte-for-
//! byte after each.
//!
//! The flat-value fallback (unknown values on the service key itself) is
//! measured alongside, since `sc config` rewrites exactly that key.

use std::collections::BTreeMap;

use winreg::RegKey;
use winreg::enums::{KEY_READ, KEY_WRITE};

use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard, cmd, registry};

/// Byte-exact field names; renaming one changes every installed artifact.
const PRIMARY: Names = Names {
    marker: "Marker",
    schema: "Schema",
    spec: "Spec",
};

/// The fallback site shares one flat key with SCM's own values, so the names
/// are prefixed there.
const FALLBACK: Names = Names {
    marker: "GoetiaMarker",
    schema: "GoetiaSchema",
    spec: "GoetiaSpec",
};

// Probe ===============================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn scm_parameters_values_survive() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let blob = support::probe_blob();
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();

    let image_path = format!(
        "\"{}\" {} {id} {} {}",
        support::current_exe_str(),
        support::sentinel::SERVICE_HOST,
        started.port(),
        stopped.port(),
    );
    cmd::run(
        "sc.exe",
        &[
            "create",
            guard.id(),
            "binPath=",
            &image_path,
            "start=",
            "demand",
            "DisplayName=",
            &format!("Goetia probe {id}"),
        ],
    )
    .expect_ok();

    let services = support::SCM_SERVICES_KEY;
    let expected = written(&blob);
    write(&registry::create_parameters_key(&id), PRIMARY, &expected);
    write(&registry::create_service_key(&id), FALLBACK, &expected);

    let mut fallback_survived = BTreeMap::new();
    let mut check = |stage: &str| {
        assert_eq!(
            read(registry::try_parameters_key(&id, KEY_READ), PRIMARY),
            expected,
            "`Parameters` values did not survive {stage}"
        );
        fallback_survived.insert(
            stage.to_string(),
            read(registry::try_service_key(&id, KEY_READ), FALLBACK) == expected,
        );
    };

    check("CreateService followed by the metadata write");

    cmd::run(
        "sc.exe",
        &["config", guard.id(), "DisplayName=", &format!("Goetia probe {id} v2")],
    )
    .expect_ok();
    check("`sc config` (ChangeServiceConfig)");

    cmd::run(
        "sc.exe",
        &["failure", guard.id(), "reset=", "86400", "actions=", "restart/2000"],
    )
    .expect_ok();
    check("`sc failure` (ChangeServiceConfig2, failure actions)");

    cmd::run("sc.exe", &["description", guard.id(), "goetia marker inertness probe"]).expect_ok();
    check("`sc description` (ChangeServiceConfig2, description)");

    // A start that fails leaves SCM having read the key but never having run
    // the service, which is not the cycle this claims to test. The sentinel
    // reports in from inside `service_main`, after SERVICE_RUNNING, and again
    // after it has handled SERVICE_CONTROL_STOP.
    cmd::run("sc.exe", &["start", guard.id()]).expect_ok();
    started.accept("the probe service to report SERVICE_RUNNING");
    cmd::run("sc.exe", &["stop", guard.id()]).expect_ok();
    stopped.accept("the probe service to handle SERVICE_CONTROL_STOP");
    // The sentinel reports in before it reports STOPPED, and `sc stop` returns once the control is
    // accepted: the cycle is over only when SCM itself says so.
    support::scm_wait::wait_for_stopped_after_stop(&mut support::scm_wait::open(guard.id()), guard.id());
    check("a start/stop cycle");

    // Control: the readback has to be able to come out negative. Without this,
    // "the values survived" is indistinguishable from a reader that reports
    // whatever it was asked about.
    registry::parameters_key(&id, KEY_READ | KEY_WRITE)
        .delete_value(PRIMARY.marker)
        .expect("delete Marker");
    assert_ne!(
        read(registry::try_parameters_key(&id, KEY_READ), PRIMARY),
        expected,
        "the readback did not notice a deleted value, so it cannot detect pruning either"
    );

    let fallback_report: String = fallback_survived
        .iter()
        .map(|(stage, ok)| format!("  after {stage}: {ok}\n"))
        .collect();
    support::record_probe(
        "scm-parameters",
        &format!(
            "site: HKLM\\{services}\\<name>\\Parameters\n\
             survives_config_failure_description_and_start_stop: yes\n\
             blob_len: {} base64 chars\n\
             fallback (flat values on the service key):\n{fallback_report}",
            blob.len(),
        ),
    );
}

// Helpers -------------------------------------------------------------------------------------------------------------

struct Names {
    marker: &'static str,
    schema: &'static str,
    spec: &'static str,
}

/// What a readback found. Absent values are absent rather than defaulted, so a
/// pruned value cannot masquerade as an empty one.
#[derive(Debug, PartialEq, Eq)]
struct Metadata {
    marker: Option<String>,
    schema: Option<u32>,
    spec: Option<String>,
}

fn written(blob: &str) -> Metadata {
    Metadata {
        marker: Some("goetia".to_string()),
        schema: Some(1),
        spec: Some(blob.to_string()),
    }
}

fn write(key: &RegKey, names: Names, values: &Metadata) {
    key.set_value(names.marker, values.marker.as_ref().unwrap())
        .expect("write marker");
    key.set_value(names.schema, values.schema.as_ref().unwrap())
        .expect("write schema");
    key.set_value(names.spec, values.spec.as_ref().unwrap())
        .expect("write spec");
}

fn read(key: std::io::Result<RegKey>, names: Names) -> Metadata {
    let Ok(key) = key else {
        return Metadata {
            marker: None,
            schema: None,
            spec: None,
        };
    };
    Metadata {
        marker: key.get_value(names.marker).ok(),
        schema: key.get_value(names.schema).ok(),
        spec: key.get_value(names.spec).ok(),
    }
}
