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

const SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";
const ENVIRONMENT_VALUE: &str = "Environment";

fn service_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}")
}

fn parameters_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}\Parameters")
}

fn registry_error(op: &str, path: &str, source: std::io::Error) -> Error {
    Error::Other(format!(r"registry {op} HKLM\{path}: {source}"))
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
pub fn read_parameters(name: &str) -> Result<BTreeMap<String, String>> {
    let path = parameters_key_path(name);
    let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(&path, KEY_READ) {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(registry_error("open", &path, e)),
    };

    let mut out = BTreeMap::new();
    for entry in key.enum_values() {
        let (name, value) = entry.map_err(|e| registry_error("enumerate", &path, e))?;
        if let Ok(s) = String::from_reg_value(&value) {
            out.insert(name, s);
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
pub fn list_service_names() -> Result<Vec<String>> {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SERVICES_KEY, KEY_READ)
        .map_err(|e| registry_error("open", SERVICES_KEY, e))?;
    key.enum_keys()
        .collect::<std::result::Result<Vec<String>, _>>()
        .map_err(|e| registry_error("enumerate", SERVICES_KEY, e))
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
