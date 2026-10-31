//! Direct registry access for the two things `windows-service`'s
//! `ServiceConfig` cannot read back at all: the `Services\<name>\Parameters`
//! metadata blob (`Marker`/`Schema`/`Version`/`Spec` — see
//! `backend::scm::generate`), and the `Services\<name>\Environment`
//! `REG_MULTI_SZ` (see the module doc comment on `manager`).
//!
//! Both live under `HKLM\SYSTEM\CurrentControlSet\Services\<name>`, the same
//! tree `tests/marker_inertness/scm.rs`'s `scm_parameters_values_survive`
//! probe already exercises with raw `winreg`.

use std::collections::BTreeMap;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    REG_BINARY, REG_DWORD, REG_DWORD_BIG_ENDIAN, REG_EXPAND_SZ, REG_FULL_RESOURCE_DESCRIPTOR, REG_LINK, REG_MULTI_SZ,
    REG_NONE, REG_QWORD, REG_RESOURCE_LIST, REG_RESOURCE_REQUIREMENTS_LIST, REG_SZ, RegQueryValueExW,
};
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

/// A registry value as stored: its type number and bytes, unconverted. `winreg`'s `RegValue` cannot
/// hold a type above `REG_QWORD`, and its `get_raw_value` fails on one.
struct RawValue {
    vtype: u32,
    bytes: Vec<u8>,
}

/// The lines of `Services\<name>\Environment`; empty when the value is absent. Errs only when the
/// value cannot be read at all; anything that is not what [`write_environment`] writes reads as the
/// marker lines [`environment_lines`] describes.
pub fn read_environment(name: &str) -> Result<Vec<String>> {
    let path = service_key_path(name);
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_READ)
        .map_err(|e| registry_error("open", &path, e))?;
    match query_raw_value(&key, ENVIRONMENT_VALUE) {
        Ok(value) => Ok(environment_lines(Some(value))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(environment_lines(None)),
        Err(e) => Err(registry_error(&format!("read {ENVIRONMENT_VALUE} under"), &path, e)),
    }
}

/// `RegQueryValueExW` for `name` under `key`, whatever its type.
fn query_raw_value(key: &RegKey, name: &str) -> std::io::Result<RawValue> {
    let wide_name = super::identity::wide_null(name);
    let mut bytes = vec![0u8; 1024];
    loop {
        let mut vtype = 0u32;
        let mut len = bytes.len() as u32;
        // SAFETY: `key` holds an open handle; `wide_name` is a NUL-terminated UTF-16 string that
        // outlives the call; `vtype` and `len` are valid out-params, and `bytes` is `len` writable
        // bytes.
        let status = unsafe {
            RegQueryValueExW(
                key.raw_handle(),
                wide_name.as_ptr(),
                std::ptr::null(),
                &mut vtype,
                bytes.as_mut_ptr(),
                &mut len,
            )
        };
        match status {
            ERROR_SUCCESS => {
                bytes.truncate(len as usize);
                return Ok(RawValue { vtype, bytes });
            }
            // `len` is now the size the value needs; it grows only if the value was rewritten meanwhile.
            ERROR_MORE_DATA => bytes.resize(len as usize, 0),
            err => return Err(std::io::Error::from_raw_os_error(err as i32)),
        }
    }
}

/// [`read_environment`]'s parsing, apart from the registry so it is testable without one.
///
/// Only a value byte-identical to what [`write_environment`] writes reads as the lines it was
/// written from. Anything else yields at least one line without `=`, and a desired line is always
/// `NAME=value`, so that line never equals desired text: the hand edit is a `Conflict` the diff
/// shows, and `install --force` overwrites it, type included.
///
/// - A value that is not a `REG_MULTI_SZ` is one line naming its type.
/// - `write_environment` writes each line followed by a NUL, then one more NUL, and writes an empty
///   list as no value. Any other NUL layout, an odd trailing byte or a present empty value adds a
///   line naming what differs, after the lines that could be read.
/// - A line that is not valid UTF-16 is shown with each unpaired surrogate and every `=` escaped.
fn environment_lines(value: Option<RawValue>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if value.vtype != REG_MULTI_SZ {
        return vec![format!("(value of type {}, not REG_MULTI_SZ)", type_name(value.vtype))];
    }

    let units: Vec<u16> = value
        .bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut problems = Vec::new();
    if let [last] = value.bytes.chunks_exact(2).remainder() {
        problems.push(format!("a trailing odd byte 0x{last:02X}"));
    }
    let body = match units.as_slice() {
        [body @ .., 0, 0] => body,
        all => {
            problems.push("no closing pair of NULs".to_string());
            all
        }
    };
    let segments: Vec<&[u16]> = body.split(|unit| *unit == 0).collect();
    if body.is_empty() {
        problems.push("no strings".to_string());
    } else if segments.iter().any(|segment| segment.is_empty()) {
        problems.push("an empty string".to_string());
    }

    let mut lines: Vec<String> = segments
        .into_iter()
        .filter(|segment| !segment.is_empty())
        .map(line_from_utf16)
        .collect();
    if !problems.is_empty() {
        lines.push(format!(
            "(REG_MULTI_SZ not laid out as goetia writes it: {})",
            problems.join(", ")
        ));
    }
    lines
}

/// One line, exactly. A line that is not valid UTF-16 shows as a line without `=`.
fn line_from_utf16(units: &[u16]) -> String {
    let decoded: Vec<_> = char::decode_utf16(units.iter().copied()).collect();
    if decoded.iter().all(|c| c.is_ok()) {
        return decoded.into_iter().flatten().collect();
    }
    let mut shown = String::from("(not valid UTF-16: ");
    for c in decoded {
        match c {
            Ok('=') => shown.push_str(r"\x3D"),
            Ok(c) => shown.extend(c.escape_debug()),
            Err(e) => shown.push_str(&format!(r"\u{{{:04X}}}", e.unpaired_surrogate())),
        }
    }
    shown.push(')');
    shown
}

/// The registry's own name for a value type, or its number when it has none.
fn type_name(vtype: u32) -> String {
    match vtype {
        REG_NONE => "REG_NONE",
        REG_SZ => "REG_SZ",
        REG_EXPAND_SZ => "REG_EXPAND_SZ",
        REG_BINARY => "REG_BINARY",
        REG_DWORD => "REG_DWORD",
        REG_DWORD_BIG_ENDIAN => "REG_DWORD_BIG_ENDIAN",
        REG_LINK => "REG_LINK",
        REG_RESOURCE_LIST => "REG_RESOURCE_LIST",
        REG_FULL_RESOURCE_DESCRIPTOR => "REG_FULL_RESOURCE_DESCRIPTOR",
        REG_RESOURCE_REQUIREMENTS_LIST => "REG_RESOURCE_REQUIREMENTS_LIST",
        REG_QWORD => "REG_QWORD",
        other => return other.to_string(),
    }
    .to_string()
}

/// Write `lines` as `Services\<name>\Environment`. No lines deletes the value rather than writing
/// an empty `REG_MULTI_SZ`.
pub fn write_environment(name: &str, lines: &[String]) -> Result<()> {
    let path = service_key_path(name);
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
        .map_err(|e| registry_error("open", &path, e))?;
    if lines.is_empty() {
        match key.delete_value(ENVIRONMENT_VALUE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(registry_error(&format!("delete {ENVIRONMENT_VALUE} under"), &path, e)),
        }
    } else {
        key.set_value(ENVIRONMENT_VALUE, &lines.to_vec())
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
/// Idempotent. `apply` runs it before it writes the service, and a failure there fails the install
/// with nothing written, so the next install retries it.
pub fn register_event_source() -> Result<()> {
    const KEY: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application\Goetia";
    const TYPES_SUPPORTED: u32 = 7; // ERROR | WARNING | INFORMATION

    let message_file = event_message_file(std::env::var("SystemRoot"))?;
    let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey(KEY)
        .map_err(|e| registry_error("create", KEY, e))?;
    key.set_value("EventMessageFile", &message_file)
        .map_err(|e| registry_error("write EventMessageFile under", KEY, e))?;
    key.set_value("TypesSupported", &TYPES_SUPPORTED)
        .map_err(|e| registry_error("write TypesSupported under", KEY, e))
}

/// The message file path, expanded here rather than stored as `%SystemRoot%\...`: that form needs
/// `REG_EXPAND_SZ`, and `winreg`'s `set_value` for `&str` writes `REG_SZ`, which nothing would
/// expand — leaving a message file that never resolves.
fn event_message_file(system_root: std::result::Result<String, std::env::VarError>) -> Result<String> {
    let system_root = system_root.map_err(|e| {
        Error::Other(format!(
            "register the Goetia event source: SystemRoot is not readable: {e}"
        ))
    })?;
    Ok(format!(r"{system_root}\System32\EventCreate.exe"))
}
