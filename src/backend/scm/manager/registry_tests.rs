use std::collections::BTreeMap;

use super::*;

#[skuld::test]
fn service_key_path_is_under_current_control_set_services() {
    assert_eq!(
        service_key_path("goetia-test"),
        r"SYSTEM\CurrentControlSet\Services\goetia-test"
    );
}

#[skuld::test]
fn parameters_key_path_is_the_service_key_plus_parameters() {
    assert_eq!(
        parameters_key_path("goetia-test"),
        r"SYSTEM\CurrentControlSet\Services\goetia-test\Parameters"
    );
}

#[skuld::test]
fn format_environment_lines_is_key_equals_value_per_entry() {
    let mut env = BTreeMap::new();
    env.insert("B".to_string(), "2".to_string());
    env.insert("A".to_string(), "1".to_string());
    // `BTreeMap` iterates key-sorted, so the lines come out deterministically
    // ordered regardless of insertion order.
    assert_eq!(
        format_environment_lines(&env),
        vec!["A=1".to_string(), "B=2".to_string()]
    );
}

#[skuld::test]
fn format_environment_lines_is_empty_for_empty_env() {
    assert!(format_environment_lines(&BTreeMap::new()).is_empty());
}

/// The `Parameters` half of this backend's [`Error::Undetermined`] keying. `winreg` surfaces
/// `ERROR_ACCESS_DENIED` as `ErrorKind::PermissionDenied`, and that chooses the *recovery* alone —
/// the class is the same either way, because a corrupt hive leaves goetia exactly as ignorant of
/// the id as a denial does.
#[skuld::test]
fn a_parameters_read_that_did_not_complete_is_undetermined() {
    let path = parameters_key_path("x");
    for (source, wants_elevation) in [
        (std::io::Error::from(std::io::ErrorKind::PermissionDenied), true),
        (std::io::Error::other("the hive is corrupt"), false),
    ] {
        let rendered = format!("{source:?}");
        let Error::Undetermined { id, reason, recovery } = registry_undetermined("x", "open", &path, &source) else {
            panic!("{rendered}: a read that did not complete establishes nothing about the id");
        };

        assert_eq!(id, "x");
        assert!(
            reason.contains(&path),
            "{rendered}: the reason must name the key: {reason}"
        );
        assert_eq!(
            recovery.contains("re-run as Administrator"),
            wants_elevation,
            "{rendered}: {recovery}"
        );
    }
}

#[skuld::test]
fn format_environment_lines_preserves_equals_signs_in_the_value() {
    let mut env = BTreeMap::new();
    env.insert("URL".to_string(), "http://x/a=b".to_string());
    assert_eq!(format_environment_lines(&env), vec!["URL=http://x/a=b".to_string()]);
}

/// The mid-pass fault, and the reason `list` no longer propagates one: a key that could not be
/// enumerated must not take down the services the very same pass already named. Injected through
/// `collect_service_names`' iterator because no real `enum_keys` fails on request.
#[skuld::test]
fn a_key_that_cannot_be_enumerated_keeps_what_the_scan_already_named() {
    let keys = vec![
        Ok("named-before-the-fault".to_string()),
        Err(std::io::Error::other("injected mid-scan failure")),
        Ok("never-reached".to_string()),
    ];

    let scan = collect_service_names(keys.into_iter());

    assert_eq!(
        scan.names,
        ["named-before-the-fault"],
        "what the pass named before the fault survives it, and the pass stops there: {scan:?}"
    );
    let detail = scan.incomplete.expect("a pass that stopped early must say so");
    assert!(detail.contains("injected mid-scan failure"), "{detail}");
    assert!(
        detail.contains(SERVICES_KEY),
        "the detail must name what was being scanned: {detail}"
    );
}

#[skuld::test]
fn a_pass_that_finishes_reports_nothing_undetermined() {
    let keys = vec![Ok("a".to_string()), Ok("b".to_string())];

    let scan = collect_service_names(keys.into_iter());

    assert_eq!(scan.names, ["a", "b"]);
    assert!(
        scan.incomplete.is_none(),
        "a pass that finished has nothing to report: {scan:?}"
    );
}

// write_parameters ====================================================================================================

/// A private key under `HKCU\Software`, removed on drop — a real registry key to write into that
/// needs neither elevation nor a registered service, standing in for the
/// `HKLM\SYSTEM\CurrentControlSet\Services\<name>` key `write_parameters` opens.
struct ScratchKey {
    key: RegKey,
    path: String,
}

impl ScratchKey {
    fn new(what: &str) -> Self {
        let path = format!(r"Software\goetia-test\{what}-{pid:x}", pid = std::process::id());
        let (key, _) = RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .create_subkey(&path)
            .expect("create the scratch key");
        Self { key, path }
    }

    fn parameters(&self) -> RegKey {
        self.key
            .open_subkey_with_flags("Parameters", KEY_READ)
            .expect("open the Parameters subkey")
    }
}

impl Drop for ScratchKey {
    fn drop(&mut self) {
        let _ = RegKey::predef(winreg::enums::HKEY_CURRENT_USER).delete_subkey_all(&self.path);
    }
}

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// The contract the doc comment states: what is under `Parameters` afterwards is exactly `params`.
/// A stray value left behind would make `on_disk` permanently unequal to `regenerated`, so
/// `install --force` would report success and the next `install` would report `Conflict` again.
#[skuld::test]
fn a_write_leaves_exactly_the_params_it_was_given() {
    let scratch = ScratchKey::new("prune");
    let (existing, _) = scratch.key.create_subkey("Parameters").expect("seed Parameters");
    existing.set_value("Marker", &"old").expect("seed the marker");
    existing.set_value("Stray", &"hand-edited").expect("seed a stray value");
    existing.create_subkey("StraySubkey").expect("seed a stray subkey");

    write_parameters_under(&scratch.key, "scratch", &params(&[("Marker", "new"), ("Spec", "blob")])).expect("write");

    let after = scratch.parameters();
    let values: BTreeMap<String, String> = after
        .enum_values()
        .map(|e| e.expect("enumerate"))
        .filter_map(|(field, value)| String::from_reg_value(&value).ok().map(|v| (field, v)))
        .collect();
    assert_eq!(values, params(&[("Marker", "new"), ("Spec", "blob")]));
    assert_eq!(
        after.enum_keys().count(),
        0,
        "a stray subkey is part of the contents too"
    );
}

/// The window this write shape exists to close, from the reader's side: `Marker` is the only proof
/// of ownership a `type: managed` service has, so an instant with no `Marker` is an instant in
/// which `list` classifies a service goetia is updating as a stranger's and **omits it** — exit
/// `0`, on a host where the daemon is installed. Deleting the key and re-creating it, which is what
/// this used to do, opens exactly that instant on every update.
#[skuld::test]
fn an_update_never_leaves_the_marker_missing() {
    let scratch = ScratchKey::new("marker");
    write_parameters_under(&scratch.key, "scratch", &params(&[("Marker", "goetia")])).expect("seed");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (path, stop) = (scratch.path.clone(), std::sync::Arc::clone(&stop));
        std::thread::spawn(move || {
            let key = RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
                .open_subkey_with_flags(&path, KEY_WRITE)
                .expect("open the scratch key");
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let spec = format!("blob-{n}");
                write_parameters_under(&key, "scratch", &params(&[("Marker", "goetia"), ("Spec", &spec)]))
                    .expect("rewrite");
                n += 1;
            }
            n
        })
    };

    // A workload, not a wait: each pass checks a real read, and the writer is stopped by a flag.
    let reads = 2000;
    for _ in 0..reads {
        let key = scratch.key.open_subkey_with_flags("Parameters", KEY_READ);
        let marker = key.and_then(|key| key.get_value::<String, _>("Marker"));
        assert!(
            marker.is_ok(),
            "the marker is what says the service is goetia's, and a rewrite must not take it away \
             — nor the key holding it: {marker:?}"
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let writes = writer.join().expect("the writer thread");

    assert!(
        writes > 0,
        "the writer has to have rewritten something for this to prove anything"
    );
    eprintln!("{reads} reads against {writes} rewrites");
}
