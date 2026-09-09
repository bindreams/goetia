//! Direct registry access for the two things `windows-service`'s
//! `ServiceConfig` cannot read back at all: the `Services\<name>\Parameters`
//! metadata blob (`Marker`/`Schema`/`Version`/`Spec` — see
//! `backend::scm::generate`), and the `Services\<name>\Environment`
//! `REG_MULTI_SZ` (see the module doc comment on `manager` for why this
//! carries `env` outside the drift-compared `ScmRegistration` surface).
//!
//! Both live under `HKLM\SYSTEM\CurrentControlSet\Services\<name>`, the same
//! tree `tests/marker_inertness/scm.rs`'s `scm_parameters_values_survive`
//! probe already exercises with raw `winreg`.

use std::collections::BTreeMap;

use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE};
use winreg::types::FromRegValue as _;

use crate::error::{Error, Result};

/// Also the `location` `manager::list` names in its aggregate entry when a pass over this key
/// stops early.
pub const SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";
const ENVIRONMENT_VALUE: &str = "Environment";

fn service_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}")
}

fn parameters_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}\Parameters")
}

/// What did not complete, as facts, with no claim about any id attached.
/// Shared by [`registry_error`] and [`registry_undetermined`], so one failure
/// cannot be described two ways depending on which class it lands in.
fn registry_detail(op: &str, path: &str, source: &std::io::Error) -> String {
    format!(r"registry {op} HKLM\{path}: {source}")
}

fn registry_error(op: &str, path: &str, source: std::io::Error) -> Error {
    Error::Other(registry_detail(op, path, &source))
}

/// The same failure, for a read that was supposed to say whether `name` is
/// goetia's at all — see [`super::undetermined`], whose doc comment carries the
/// whole argument, including why the class is not keyed on the denial while the
/// recovery is.
fn registry_undetermined(name: &str, op: &str, path: &str, source: &std::io::Error) -> Error {
    super::undetermined(
        name,
        registry_detail(op, path, source),
        source.kind() == std::io::ErrorKind::PermissionDenied,
    )
}

// Parameters (the metadata blob) ======================================================================================

/// Every string-valued entry under `Services\<name>\Parameters`, keyed
/// case-preserved (case-insensitive lookup of Goetia's own four fields is
/// `generate::extract`'s job, not this function's). Values of a non-string
/// registry type (`REG_DWORD`, `REG_BINARY`, ...) are skipped — they cannot
/// be one of Goetia's own fields, which are always strings, and are none of
/// Goetia's business otherwise.
///
/// An absent `Parameters` subkey — a service that exists but was never
/// touched by Goetia's metadata write (including one whose install crashed
/// between `CreateServiceW` and that write; see the module doc comment on
/// `manager`) — reads back as an empty map, not an error: to
/// `generate::extract`, that is indistinguishable from "no Goetia marker at
/// all", which is exactly the correct classification (`Ownership::Foreign`).
/// Absence is *established* there; every other failure establishes nothing,
/// and is [`registry_undetermined`] — this key holds the only proof of
/// ownership a `type: managed` service has, so a read of it that did not
/// complete leaves goetia unable to say even that much.
pub fn read_parameters(name: &str) -> Result<BTreeMap<String, String>> {
    let path = parameters_key_path(name);
    let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(&path, KEY_READ) {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(registry_undetermined(name, "open", &path, &e)),
    };

    let mut out = BTreeMap::new();
    for entry in key.enum_values() {
        let (field, value) = entry.map_err(|e| registry_undetermined(name, "enumerate", &path, &e))?;
        if let Ok(s) = String::from_reg_value(&value) {
            out.insert(field, s);
        }
    }
    Ok(out)
}

/// Write `params` under `Services\<name>\Parameters` as the subkey's
/// *entire* contents: any pre-existing value not in `params` is removed
/// first, not merely left alone. A hand-edit that adds a stray value under
/// `Parameters` must still be overwritable by `install --force` — leaving
/// it behind would mean `on_disk` (via [`read_parameters`], which reads
/// everything present) never converges with `desired`/`regenerated` (which
/// only ever have Goetia's own four fields), and `--force` would report
/// success while the very artifact it was asked to fix keeps reporting
/// `Conflict` on every subsequent `install`.
pub fn write_parameters(name: &str, params: &BTreeMap<String, String>) -> Result<()> {
    let service_path = service_key_path(name);
    let service_key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&service_path, KEY_WRITE)
        .map_err(|e| registry_error("open", &service_path, e))?;

    match service_key.delete_subkey_all("Parameters") {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(registry_error("delete", &parameters_key_path(name), e)),
    }

    let (key, _) = service_key
        .create_subkey("Parameters")
        .map_err(|e| registry_error("create", &parameters_key_path(name), e))?;
    for (field, value) in params {
        key.set_value(field, value)
            .map_err(|e| registry_error(&format!("write {field} under"), &parameters_key_path(name), e))?;
    }
    Ok(())
}

// Environment =========================================================================================================

/// `env` as the `REG_MULTI_SZ` lines `Services\<name>\Environment` needs —
/// the format SCM itself reads when starting the service process (see the
/// module doc comment on `manager` for the empirical basis). Pure, so the
/// one interesting decision here (what an empty `env` produces) is
/// unit-testable without a registry.
fn format_environment_lines(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Write `env` as `Services\<name>\Environment`. An empty `env` deletes the
/// value rather than writing an empty `REG_MULTI_SZ`, so a spec with no
/// `env` leaves nothing behind to misreport as "this daemon sets an
/// environment variable" on a later inspection with a native tool.
pub fn write_environment(name: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let path = service_key_path(name);
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
        .map_err(|e| registry_error("open", &path, e))?;
    if env.is_empty() {
        match key.delete_value(ENVIRONMENT_VALUE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(registry_error(&format!("delete {ENVIRONMENT_VALUE} under"), &path, e)),
        }
    } else {
        let lines = format_environment_lines(env);
        key.set_value(ENVIRONMENT_VALUE, &lines)
            .map_err(|e| registry_error(&format!("write {ENVIRONMENT_VALUE} under"), &path, e))
    }
}

// Discovery for `list` ================================================================================================

/// What one pass over the `Services` key found: the names it reached, and — when the pass stopped
/// early — what did not complete there. `manager::list` turns the latter into the aggregate entry
/// standing for every service the pass never got to.
#[derive(Debug)]
pub struct ServiceScan {
    pub names: Vec<String>,
    /// `Some` when the enumeration stopped before the end. The names collected up to that point
    /// stay in `names`: dropping them reports every one of them as absent, which is precisely what
    /// a pass that did not finish cannot establish.
    pub incomplete: Option<String>,
}

/// Every service currently registered with SCM, paired with its
/// `Parameters` map (empty when absent — see [`read_parameters`]). The
/// caller (`manager::list`) runs `generate::extract` over each to decide
/// which are Goetia's.
///
/// Read directly from the registry rather than `windows-service`'s
/// enumeration API: `SC_MANAGER_ENUMERATE_SERVICE`/`EnumServicesStatusExW`
/// need no more privilege than reading this key does, and going through the
/// registry once here avoids an `OpenService` round trip per candidate on a
/// host with hundreds of unrelated services.
/// No `Services` key at all is neither a failure nor an unfinished pass: absence is *established*
/// there, so it is an empty scan with nothing outstanding — see [`crate::manager::ServiceManager::list`].
pub fn list_service_names() -> ServiceScan {
    scan_names_under(SERVICES_KEY)
}

/// The pass over one named key rather than over [`SERVICES_KEY`] directly — the seam
/// [`collect_service_names`] is for the mid-pass fault, applied to the *open*. `Services` exists and
/// opens on every host that boots, so pointing the same code at another key is the only way a test
/// reaches the two outcomes that are not "it opened": see `registry_tests.rs`.
fn scan_names_under(path: &str) -> ServiceScan {
    let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(path, KEY_READ) {
        Ok(key) => key,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ServiceScan {
                names: Vec::new(),
                incomplete: None,
            };
        }
        Err(e) => {
            return ServiceScan {
                names: Vec::new(),
                incomplete: Some(registry_detail("open", path, &e)),
            };
        }
    };
    collect_service_names(path, key.enum_keys())
}

/// The pass itself, over an iterator rather than the `RegKey` directly — which is what makes the
/// mid-pass fault reachable from a test, since no `enum_keys` fails on request. That fault is the
/// half that matters: stop, keep, report. Collecting into a `Result` instead discards every name
/// already enumerated because a *later* one faulted, leaving `list` nothing to return but an error
/// — an empty document on exit `1`, which says this host runs no daemons.
fn collect_service_names(path: &str, keys: impl Iterator<Item = std::io::Result<String>>) -> ServiceScan {
    let mut names = Vec::new();
    for key in keys {
        match key {
            Ok(name) => names.push(name),
            Err(e) => {
                return ServiceScan {
                    names,
                    incomplete: Some(registry_detail("enumerate", path, &e)),
                };
            }
        }
    }
    ServiceScan {
        names,
        incomplete: None,
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;

/// Register the `Goetia` Event Log source so Event Viewer can render what the
/// shim writes.
///
/// Without this, `ReportEventW` still records the insertion string, but Event
/// Viewer shows "The operation completed successfully." — the fallback for a
/// source with no message resource. That string is the *only* thing an
/// administrator sees when asking why a daemon died at boot, so an unreadable
/// entry is barely better than no entry.
///
/// `EventCreate.exe` is used as the message file because its message 1 is a
/// bare `%1` passthrough of the single insertion string. That is the standard
/// trick for emitting readable events without shipping a compiled `.mc`
/// catalogue (which is what `~/src/windows-service-manager` does instead, at
/// the cost of a build-time `mc.exe` step).
///
/// Idempotent, and best-effort by contract: a failure here must not fail an
/// install, since the daemon itself is fine either way.
pub fn register_event_source() {
    const KEY: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application\Goetia";
    const TYPES_SUPPORTED: u32 = 7; // ERROR | WARNING | INFORMATION

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok((key, _)) = hklm.create_subkey(KEY) else {
        return;
    };
    // Expanded here rather than stored as `%SystemRoot%\...`: that form needs
    // `REG_EXPAND_SZ`, and `winreg`'s `set_value` for `&str` writes `REG_SZ`,
    // which nothing would expand — leaving a message file that never resolves.
    let Ok(system_root) = std::env::var("SystemRoot") else {
        return;
    };
    let message_file = format!(r"{system_root}\System32\EventCreate.exe");
    let _ = key.set_value("EventMessageFile", &message_file);
    let _ = key.set_value("TypesSupported", &TYPES_SUPPORTED);
}
