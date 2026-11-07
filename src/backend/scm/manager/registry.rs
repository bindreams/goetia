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
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE};
use winreg::types::FromRegValue as _;
use winreg::{RegKey, RegValue};

use crate::backend::scm::generate::field_named;
use crate::error::{Error, Result};
use crate::manager::{Installed, flow};

mod settle;
mod watch;
use settle::{Failure, Settled, Stop, is_gone, settle, stands};
use watch::ServiceWatch;

const SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";
const ENVIRONMENT_VALUE: &str = "Environment";

fn service_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}")
}

fn parameters_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}\Parameters")
}

/// What did not complete, as facts, with no claim about any id attached. Every registry failure
/// is built here — single-id reads, [`registry_undetermined`] and the `list` scan — so one
/// failure cannot be described two ways depending on which class it lands in.
fn registry_error(op: &str, path: &str, source: std::io::Error) -> Error {
    Error::os(format!(r"{op} HKLM\{path}"), source)
}

/// The same failure, for a read that was supposed to say whether `name` is
/// goetia's at all — see [`super::undetermined`], whose doc comment carries the
/// whole argument, including why the class is not keyed on the denial while the
/// recovery is.
fn registry_undetermined(name: &str, op: &str, path: &str, source: std::io::Error) -> Error {
    let access_denied = source.kind() == std::io::ErrorKind::PermissionDenied;
    super::undetermined(name, registry_error(op, path, source), access_denied)
}

// Parameters (the metadata blob) ======================================================================================

/// What a read of a service's registry values found.
pub enum RegistryReadOutcome<T> {
    /// The service no longer exists.
    ServiceGone,
    Read(T),
}

/// Runs `pass` until it completes or [`settle`] ends it. `fail` makes the error for a failure that
/// leaves the read undecided.
///
/// A read that must be retried waits for a change under the service key first, rather than reading
/// again at once: a key marked for deletion stays so while another process holds it open, and reads
/// until then would spin. The notification is armed before the pass whose failure is waited on, so a
/// change after that pass began wakes the wait and none is lost. A service key that is already gone
/// cannot be watched, and the next pass finds it so.
fn read_settled<T>(
    name: &str,
    fail: impl Fn(Failure) -> Error,
    mut pass: impl FnMut() -> std::result::Result<T, Stop<T>>,
) -> Result<RegistryReadOutcome<T>> {
    let path = service_key_path(name);
    let mut armed: Option<ServiceWatch> = None;
    loop {
        let stop = match pass() {
            Ok(value) => return Ok(RegistryReadOutcome::Read(value)),
            Err(stop) => stop,
        };
        match settle(stop, || service_stands(name)) {
            Settled::Gone => return Ok(RegistryReadOutcome::ServiceGone),
            Settled::Value(value) => return Ok(RegistryReadOutcome::Read(value)),
            Settled::Retry => {
                if let Some(watch) = armed.take() {
                    watch
                        .wait()
                        .map_err(|source| fail(failure("wait for a change under", &path, source)))?;
                }
                armed = match ServiceWatch::arm(&path) {
                    Ok(watch) => Some(watch),
                    Err(source) if is_gone(&source) => None,
                    Err(source) => return Err(fail(failure("watch", &path, source))),
                };
            }
            Settled::Failed(failure) => return Err(fail(failure)),
        }
    }
}

/// Whether `Services\<name>` exists, asked only after a read met a missing key.
fn service_stands(name: &str) -> std::result::Result<bool, Failure> {
    let path = service_key_path(name);
    stands(
        RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(&path, KEY_READ)
            .map(drop),
    )
    .map_err(|source| failure("open", &path, source))
}

fn failure(op: &str, path: &str, source: std::io::Error) -> Failure {
    Failure {
        op: op.to_string(),
        path: path.to_string(),
        source,
    }
}

/// Every entry under `Services\<name>\Parameters`, keyed case-preserved (case-insensitive lookup of
/// Goetia's own four fields is `generate::extract`'s job, not this function's). A string value is
/// its text. Anything else goetia does not write is read too, as an entry no field name of Goetia's
/// can match ([`value_entry`], [`subkey_entry`]): a stray value of another type or a stray subkey
/// is drift that `install --force` removes, and it must not read as equal to what goetia wants.
///
/// The result classifies the id:
///
/// - A service with no `Parameters` key reads as an empty map: it has no goetia marker, so
///   `generate::extract` finds none and the service is `Ownership::Foreign`. This covers a service
///   goetia never touched and one whose install crashed between `CreateServiceW` and the metadata
///   write (see the module doc comment on `manager`).
/// - [`RegistryReadOutcome::ServiceGone`] when the service key is gone, whether the read found it
///   so at the start or lost the `Parameters` key under it. A key that vanishes or is marked
///   deleted under a service that still stands is read again from the start, until the read
///   completes or the service is gone.
/// - [`registry_undetermined`] for every other failure: this key holds the only proof of ownership
///   a `type: managed` service has, so a read of it that did not complete leaves goetia unable to
///   say even that much. Never Foreign.
///
/// goetia's own writes never remove `Parameters` (see [`write_parameters`]). SCM's delete of the
/// service does: it removes `Parameters` before the service key, so a read that lands between the
/// two steps of a concurrent uninstall sees a standing service with no `Parameters`, and reads it
/// as Foreign. That window stays open: goetia does not guard against a user's own concurrent runs
/// (owner, 2026-11-03).
pub fn read_parameters(name: &str) -> Result<RegistryReadOutcome<BTreeMap<String, String>>> {
    read_parameters_as(name, false)
}

/// [`read_parameters`] for a `name` that the `list` scan has just read from the `Services`
/// enumeration: a missing `Parameters` key is then Foreign without asking whether the service
/// stands. A service deleted since leaves a stale Foreign row, which `list` omits.
pub fn read_listed_parameters(name: &str) -> Result<RegistryReadOutcome<BTreeMap<String, String>>> {
    read_parameters_as(name, true)
}

fn read_parameters_as(name: &str, listed: bool) -> Result<RegistryReadOutcome<BTreeMap<String, String>>> {
    let path = parameters_key_path(name);
    read_settled(
        name,
        |f| registry_undetermined(name, &f.op, &f.path, f.source),
        || {
            let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(&path, KEY_READ) {
                Ok(key) => key,
                Err(e) if listed && e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
                Err(e) => return Err(Stop::at_open(failure("open", &path, e), Some(BTreeMap::new()))),
            };
            let mut out = BTreeMap::new();
            for entry in key.enum_values() {
                let (field, value) = entry.map_err(|e| Stop::mid_read(failure("enumerate", &path, e)))?;
                let (name, shown) = value_entry(field, &value);
                out.insert(name, shown);
            }
            for entry in key.enum_keys() {
                let subkey = entry.map_err(|e| Stop::mid_read(failure("enumerate subkeys of", &path, e)))?;
                let (name, shown) = subkey_entry(&subkey);
                out.insert(name, shown);
            }
            Ok(out)
        },
    )
}

/// How one `Parameters` value reads: its text when it is a string, which is all of Goetia's own
/// fields. Any other type reads as an entry named for the value and its type, so it differs from
/// every desired entry and still cannot be taken for one of Goetia's fields: a foreign service's
/// `Marker` of type `REG_DWORD` stays foreign.
fn value_entry(name: String, value: &RegValue) -> (String, String) {
    match String::from_reg_value(value) {
        Ok(text) => (name, text),
        Err(_) => (
            format!("{name} ({})", type_name(value.vtype.clone() as u32)),
            "(value is not a string)".to_string(),
        ),
    }
}

/// How one `Parameters` subkey reads: an entry whose name ends in the backslash a key name cannot
/// hold, so it is none of Goetia's fields.
fn subkey_entry(name: &str) -> (String, String) {
    (format!(r"{name}\"), "(subkey)".to_string())
}

/// Write `params` as the *entire* contents of `Services\<name>\Parameters`, in place: every value
/// is set, then every value that matches no name in `params` and every subkey is deleted. The key
/// itself is never deleted, so a concurrent reader never sees a missing key. Each value is set in
/// one call, so a reader sees the old or the new value of each, not of the set of four. `Spec` is
/// the only value read for meaning (`Schema` and `Version` are copies for a human), so a reader
/// sees the old spec or the new one. Fails if the service key is gone, rather than creating it.
///
/// goetia owns `Parameters` whole. A stray value or subkey added by hand must still be
/// overwritable by `install --force`: left behind, it would keep `on_disk` (via [`read_parameters`],
/// which reads everything present) from ever converging with `desired`, which has only Goetia's own
/// four fields, and `--force` would report success while every later `install` still said
/// `Conflict`. Names compare case-insensitively, as the registry does: setting `Marker` updates a
/// stored `marker` and keeps its case, so that value is wanted, not stray.
pub fn write_parameters(name: &str, params: &BTreeMap<String, String>) -> Result<()> {
    debug_assert!(
        params.keys().all(|field| field_named(field) == Some(field.as_str())),
        "Parameters holds only Goetia's own fields, spelled as goetia does: {:?}",
        params.keys().collect::<Vec<_>>()
    );
    let service_path = service_key_path(name);
    let path = parameters_key_path(name);
    let service = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&service_path, KEY_READ | KEY_WRITE)
        .map_err(|e| registry_error("open", &service_path, e))?;
    let (key, _) = service
        .create_subkey_with_flags("Parameters", KEY_READ | KEY_WRITE)
        .map_err(|e| registry_error("create", &path, e))?;

    for (field, value) in params {
        key.set_value(field, value)
            .map_err(|e| registry_error(&format!("write {field} under"), &path, e))?;
    }

    let present: Vec<String> = key
        .enum_values()
        .map(|entry| entry.map(|(field, _)| field))
        .collect::<std::io::Result<_>>()
        .map_err(|e| registry_error("enumerate", &path, e))?;
    for field in strays(&present, params) {
        tolerate_not_found(key.delete_value(field))
            .map_err(|e| registry_error(&format!("delete {field} under"), &path, e))?;
    }

    let subkeys: Vec<String> = key
        .enum_keys()
        .collect::<std::io::Result<_>>()
        .map_err(|e| registry_error("enumerate subkeys of", &path, e))?;
    for subkey in &subkeys {
        tolerate_not_found(key.delete_subkey_all(subkey))
            .map_err(|e| registry_error(&format!("delete subkey {subkey} under"), &path, e))?;
    }
    Ok(())
}

/// The names in `present` that are none of the fields in `wanted`, which holds only Goetia's own
/// field names. Compared by [`field_named`]: the registry ignores case.
fn strays<'a>(present: &'a [String], wanted: &BTreeMap<String, String>) -> impl Iterator<Item = &'a String> {
    present
        .iter()
        .filter(|name| field_named(name).is_none_or(|field| !wanted.contains_key(field)))
}

/// A delete of something already gone is done.
fn tolerate_not_found(deleted: std::io::Result<()>) -> std::io::Result<()> {
    match deleted {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
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
/// marker lines [`environment_lines`] describes. [`RegistryReadOutcome::ServiceGone`] when the service
/// is gone, the same classification as [`read_parameters`]'s.
pub fn read_environment(name: &str) -> Result<RegistryReadOutcome<Vec<String>>> {
    let path = service_key_path(name);
    read_settled(
        name,
        |f| registry_error(&f.op, &f.path, f.source),
        || {
            let key = RegKey::predef(HKEY_LOCAL_MACHINE)
                .open_subkey_with_flags(&path, KEY_READ)
                .map_err(|e| Stop::at_open(failure("open", &path, e), None))?;
            match query_raw_value(&key, ENVIRONMENT_VALUE) {
                Ok(value) => Ok(environment_lines(Some(value))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(environment_lines(None)),
                Err(e) => Err(Stop::mid_read(failure(
                    &format!("read {ENVIRONMENT_VALUE} under"),
                    &path,
                    e,
                ))),
            }
        },
    )
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

    let (pairs, remainder) = value.bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    let mut problems = Vec::new();
    if let [last] = remainder {
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

// Enumeration for `list` ==============================================================================================

/// Hands every service currently registered with SCM to `keep`, and returns the entry for what the
/// pass did not reach, if anything (see [`flow::scan`]).
pub fn scan_services(keep: impl FnMut(String)) -> Option<Installed> {
    scan_names_under(SERVICES_KEY, keep)
}

/// [`scan_services`] over another key: `Services` opens on every host that boots, so another key
/// is the only way a test reaches an open that fails, or finds nothing.
fn scan_names_under(path: &str, keep: impl FnMut(String)) -> Option<Installed> {
    flow::scan(
        path,
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(path, KEY_READ),
        |key| Box::new(key.enum_keys()),
        |op, source| registry_error(op, path, source),
        keep,
    )
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
    let system_root = system_root.map_err(|e| Error::os("read SystemRoot to register the Goetia event source", e))?;
    Ok(format!(r"{system_root}\System32\EventCreate.exe"))
}
