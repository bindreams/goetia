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
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt as _;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::System::Registry::{REG_MULTI_SZ, RegEnumValueW, RegQueryValueExW};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE};

use crate::backend::scm::generate::{FIELDS, field_named};
use crate::backend::scm::parameters::{Parameters, field_named_utf16, shown_name, text_from_utf16, type_name};
use crate::error::{Error, Result};
use crate::manager::{Installed, flow};

mod counted;
mod settle;
pub use settle::Origin;
use settle::{Failure, Settled, Stop, settle, stands, tolerate_not_found};

const SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";
const ENVIRONMENT_VALUE: &str = "Environment";
const PARAMETERS_KEY: &str = "Parameters";

fn service_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}")
}

fn parameters_key_path(name: &str) -> String {
    format!(r"{SERVICES_KEY}\{name}\{PARAMETERS_KEY}")
}

fn parameters_key_name() -> Vec<u16> {
    PARAMETERS_KEY.encode_utf16().collect()
}

/// What a read asks `Services` for: `Parameters` of `name` as a path below it, for one open that
/// passes the service key without opening it, and the access to open it with, which is a read's.
fn parameters_read_request(name: &str) -> (Vec<u16>, u32) {
    (format!(r"{name}\{PARAMETERS_KEY}").encode_utf16().collect(), KEY_READ)
}

/// An open `Services` key, which a read opens one service's `Parameters` below. It borrows the key
/// it is made from, so it cannot outlive the handle.
pub struct ServicesRoot<'r>(HANDLE, PhantomData<&'r RegKey>);

impl<'r> ServicesRoot<'r> {
    fn new(key: &'r RegKey) -> Self {
        Self(key.raw_handle(), PhantomData)
    }
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

/// Runs `pass` once and settles what it met (see [`settle`]). `service_stands` asks whether the
/// service is still there; `fail` makes the error for a failure that leaves the read undecided.
fn read_settled<T>(
    origin: Origin,
    service_stands: impl FnOnce() -> std::result::Result<bool, Failure>,
    fail: impl Fn(Failure) -> Error,
    pass: impl FnOnce() -> std::result::Result<T, Stop<T>>,
) -> Result<RegistryReadOutcome<T>> {
    let stop = match pass() {
        Ok(value) => return Ok(RegistryReadOutcome::Read(value)),
        Err(stop) => stop,
    };
    match settle(stop, origin, service_stands) {
        Settled::Gone => Ok(RegistryReadOutcome::ServiceGone),
        Settled::Value(value) => Ok(RegistryReadOutcome::Read(value)),
        Settled::Failed(failure) => Err(fail(failure)),
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

/// What `Services\<name>\Parameters` holds: goetia's four fields, each read by its name, and every
/// other value and subkey as a stray (see [`Parameters`]). A stray is drift that `install --force`
/// removes. `Parameters` is opened as the key it is: one that is a registry link reads as the link
/// (a link key holds no value but its `SymbolicLinkValue`, so it holds no marker, and a marker in
/// its target is not goetia's proof): the service reads as not goetia's. `origin` says what vouches
/// for `name` (see [`Origin`]); the result classifies the id:
///
/// - A service with no `Parameters` key reads as empty `Parameters`, so `generate::extract` finds
///   no marker and the service is `Ownership::Foreign`. This covers a service goetia never touched
///   and one whose install crashed between `CreateServiceW` and the metadata write. `Parameters` is
///   opened in one step below `Services`, so the service key is never opened, and a service whose
///   own key denies the reader is read as any other. A key not found does not say whether the
///   service key is there: unless `origin` vouches for it, that key is opened to check that it
///   stands, and gone, the read is [`RegistryReadOutcome::ServiceGone`].
/// - [`RegistryReadOutcome::ServiceGone`] when the service key is gone: the read found the
///   `Parameters` key missing and the check above finds the service key missing too, or it lost the
///   `Parameters` key (marked deleted, or gone mid-read) and the same check finds the service key
///   gone. A [`Origin::Vouched`] name is not checked for a plain missing `Parameters` key, which
///   reads as the service standing without it; it is checked only when the key is lost mid-read.
/// - [`registry_undetermined`] for every other failure, among them a key that vanishes or is marked
///   deleted under a service that still stands. Never Foreign: this key holds the only proof of
///   ownership a `type: managed` service has.
///
/// goetia's own writes remove `Parameters` only when it is a registry link (see
/// [`write_parameters`]), which a read classifies as not goetia's, so a write meets one only when
/// it appeared after the read. SCM's delete of the service removes it, before the service key: a
/// read between the two steps sees a standing service with no `Parameters`, and reads it as
/// Foreign. A concurrent uninstall can leave this window open; goetia does not guard against it
/// (owner, 2026-11-03).
pub fn read_parameters(name: &str, origin: Origin) -> Result<RegistryReadOutcome<Parameters>> {
    let services = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SERVICES_KEY, KEY_READ)
        .map_err(|e| registry_undetermined(name, "open", SERVICES_KEY, e))?;
    read_parameters_in(&ServicesRoot::new(&services), name, origin)
}

/// [`read_parameters`] below a `Services` key that is already open, as a scan holds one.
pub fn read_parameters_in(
    services: &ServicesRoot<'_>,
    name: &str,
    origin: Origin,
) -> Result<RegistryReadOutcome<Parameters>> {
    let path = parameters_key_path(name);
    read_settled(
        origin,
        || service_stands(name),
        |f| registry_undetermined(name, &f.op, &f.path, f.source),
        || {
            let (relative, access) = parameters_read_request(name);
            let key = counted::open_child(services.0, &relative, access)
                .map_err(|e| Stop::at_open(failure("open", &path, e), Some(Parameters::default())))?;
            let mut parameters = Parameters::default();
            // The names as stored, for a field found not to be a string.
            let entries = enum_values(&key).map_err(|e| Stop::mid_read(failure("enumerate", &path, e)))?;
            let mut buffer = Vec::new();
            // By name, not by position: a concurrent write that deletes a stray moves the later
            // values up one place in the enumeration, and a read by position would skip one.
            for field in FIELDS {
                match query_raw_value(&key, field, &mut buffer) {
                    Ok(vtype) => parameters.add_value(stored_name(&entries, field), vtype, &buffer),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(Stop::mid_read(failure(&format!("read {field} under"), &path, e))),
                }
            }
            for entry in entries.into_iter().filter(|entry| entry.field.is_none()) {
                parameters.add_value(entry.name, entry.vtype, &[]);
            }
            for entry in key.enum_keys_os_string() {
                let subkey = entry.map_err(|e| Stop::mid_read(failure("enumerate subkeys of", &path, e)))?;
                parameters.add_subkey(subkey.encode_wide().collect());
            }
            Ok(parameters)
        },
    )
}

/// `field`'s name as `entries` hold it, which may differ from goetia's spelling in case; goetia's
/// spelling when the enumeration did not reach it.
fn stored_name(entries: &[RawEntry], field: &str) -> Vec<u16> {
    entries
        .iter()
        .find(|entry| entry.field == Some(field))
        .map_or_else(|| field.encode_utf16().collect(), |entry| entry.name.clone())
}

/// One value of an open key: its name as UTF-16, the field of goetia's that name spells (decoded
/// once, here), and its type number.
struct RawEntry {
    name: Vec<u16>,
    field: Option<&'static str>,
    vtype: u32,
}

impl RawEntry {
    fn new(name: Vec<u16>, vtype: u32) -> Self {
        let field = field_named_utf16(&name);
        Self { name, field, vtype }
    }
}

/// What [`query_raw_value`] first offers a value, in bytes.
const INITIAL_VALUE_BYTES: usize = 1024;

/// The longest value name the registry holds, in UTF-16 units.
const MAX_VALUE_NAME_UNITS: usize = 16_383;

/// Runs a registry call that fills a value's data until it has all of it, into `buffer`. `call`
/// gets the buffer, its size in bytes, which it lowers to what it wrote, and the type; it returns
/// the status. `ERROR_MORE_DATA` grows `buffer` to the size the call reported, and a size that did
/// not grow would change nothing, so that status is returned as it is, as is any other failure.
/// On success the value is the type and the first `len` bytes of `buffer`.
fn read_growing(
    buffer: &mut Vec<u8>,
    mut call: impl FnMut(*mut u8, &mut u32, &mut u32) -> u32,
) -> std::result::Result<(u32, usize), u32> {
    loop {
        let mut vtype = 0u32;
        let mut len = buffer.len() as u32;
        match call(buffer.as_mut_ptr(), &mut len, &mut vtype) {
            ERROR_SUCCESS => return Ok((vtype, len as usize)),
            ERROR_MORE_DATA if len as usize > buffer.len() => buffer.resize(len as usize, 0),
            status => return Err(status),
        }
    }
}

/// The name and type of every value of `key`, through `RegEnumValueW` alone. `winreg`'s enumeration
/// fails on a type above `REG_QWORD` and on a name that is not valid UTF-16, which would make a
/// value goetia could neither report nor remove.
fn enum_values(key: &RegKey) -> std::io::Result<Vec<RawEntry>> {
    let mut entries = Vec::new();
    let mut name = vec![0u16; MAX_VALUE_NAME_UNITS + 1];
    for index in 0u32.. {
        let mut name_len = name.len() as u32;
        let mut vtype = 0u32;
        // SAFETY: `key` holds an open handle; `name` is writable for `name_len` units, which the
        // call lowers to what it wrote; `vtype` is a valid out-param; the data is not wanted, so
        // its buffer and size are null, which the API allows; the reserved parameter is null.
        let status = unsafe {
            RegEnumValueW(
                key.raw_handle(),
                index,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null(),
                &mut vtype,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        match status {
            ERROR_SUCCESS => entries.push(RawEntry::new(name[..name_len as usize].to_vec(), vtype)),
            ERROR_NO_MORE_ITEMS => break,
            status => return Err(std::io::Error::from_raw_os_error(status as i32)),
        }
    }
    Ok(entries)
}

/// Write `params` as the *entire* contents of `Services\<name>\Parameters`, in place: every value
/// is set, then every other value and every subkey is deleted. The key is never deleted, and each
/// value is set in one call, so a concurrent reader sees the old or the new value of each. Fails if
/// the service key is gone, rather than creating it.
///
/// The one exception is a `Parameters` that is a registry link: it is deleted itself, never entered,
/// and a real key takes its place, so its target is left as it was. A service whose `Parameters` was
/// a link at the read is not goetia's (see [`read_parameters`]) and is never written, so this is
/// reached only by a link that appeared between that read and this write.
///
/// goetia owns `Parameters` whole: a stray left behind would keep `read_parameters` from ever
/// equalling `desired`, so `--force` could not converge. Names compare case-insensitively, as the
/// registry does: setting `Marker` updates a stored `marker` and keeps its case, so that value is
/// wanted, not stray.
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
    let key = create_parameters_key(&service).map_err(|e| registry_error("create", &path, e))?;

    for (field, value) in params {
        key.set_value(field, value)
            .map_err(|e| registry_error(&format!("write {field} under"), &path, e))?;
    }

    let present = enum_values(&key).map_err(|e| registry_error("enumerate", &path, e))?;
    for stray in strays(&present, params) {
        tolerate_not_found(counted::delete_value(&key, &stray.name))
            .map_err(|e| registry_error(&format!("delete value \"{}\" under", shown_name(&stray.name)), &path, e))?;
    }

    let subkeys: Vec<Vec<u16>> = key
        .enum_keys_os_string()
        .map(|subkey| subkey.map(|name| name.encode_wide().collect()))
        .collect::<std::io::Result<_>>()
        .map_err(|e| registry_error("enumerate subkeys of", &path, e))?;
    for subkey in &subkeys {
        counted::delete_subkey(&key, subkey)
            .map_err(|e| registry_error(&format!("delete subkey \"{}\" under", shown_name(subkey)), &path, e))?;
    }
    Ok(())
}

/// The entries of `present` that are none of the fields in `wanted`, which holds only Goetia's own
/// field names. The registry ignores case, and a name that is not valid UTF-16 is none of them.
fn strays<'a>(present: &'a [RawEntry], wanted: &BTreeMap<String, String>) -> impl Iterator<Item = &'a RawEntry> {
    present
        .iter()
        .filter(|entry| entry.field.is_none_or(|field| !wanted.contains_key(field)))
}

/// `Parameters` under `service`, created if absent, as the key it is. A link is deleted and a real
/// key created in its place: writing through it would change a key goetia does not own. A link is
/// met only if it appeared after the read that cleared the write (see [`write_parameters`]); this
/// is the guard against writing through it.
fn create_parameters_key(service: &RegKey) -> std::io::Result<RegKey> {
    let name = parameters_key_name();
    let access = KEY_READ | KEY_WRITE;
    let key = counted::create_child(service, &name, access)?;
    if !counted::is_link(&key)? {
        return Ok(key);
    }
    drop(key);
    counted::delete_subkey(service, &name)?;
    counted::create_child(service, &name, access)
}

// Environment =========================================================================================================

/// The lines of `Services\<name>\Environment`; empty when the value is absent. Errs only when the
/// value cannot be read at all; anything that is not what [`write_environment`] writes reads as the
/// marker lines [`environment_lines`] describes. [`RegistryReadOutcome::ServiceGone`] when the service
/// is gone, the same classification as [`read_parameters`]'s.
pub fn read_environment(name: &str) -> Result<RegistryReadOutcome<Vec<String>>> {
    let path = service_key_path(name);
    read_settled(
        Origin::Opened,
        || service_stands(name),
        |f| registry_error(&f.op, &f.path, f.source),
        || {
            let key = RegKey::predef(HKEY_LOCAL_MACHINE)
                .open_subkey_with_flags(&path, KEY_READ)
                .map_err(|e| Stop::at_open(failure("open", &path, e), None))?;
            let mut bytes = Vec::new();
            match query_raw_value(&key, ENVIRONMENT_VALUE, &mut bytes) {
                Ok(vtype) => Ok(environment_lines(Some((vtype, &bytes)))),
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

/// `RegQueryValueExW` for `name` under `key`, whatever its type: the type, with the data in
/// `buffer` (its contents are replaced, and its allocation is reused across calls).
fn query_raw_value(key: &RegKey, name: &str, buffer: &mut Vec<u8>) -> std::io::Result<u32> {
    let wide_name = super::identity::wide_null(name);
    buffer.clear();
    buffer.resize(INITIAL_VALUE_BYTES.max(buffer.capacity()), 0);
    let (vtype, len) = read_growing(buffer, |data, len, vtype| {
        // SAFETY: `key` holds an open handle; `wide_name` is a NUL-terminated UTF-16 string that
        // outlives the call; `vtype` and `len` are valid out-params, and `data` is `len` writable
        // bytes.
        unsafe { RegQueryValueExW(key.raw_handle(), wide_name.as_ptr(), std::ptr::null(), vtype, data, len) }
    })
    .map_err(|status| std::io::Error::from_raw_os_error(status as i32))?;
    buffer.truncate(len);
    Ok(vtype)
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
fn environment_lines(value: Option<(u32, &[u8])>) -> Vec<String> {
    let Some((vtype, bytes)) = value else {
        return Vec::new();
    };
    if vtype != REG_MULTI_SZ {
        return vec![format!("(value of type {}, not REG_MULTI_SZ)", type_name(vtype))];
    }

    let (pairs, remainder) = bytes.as_chunks::<2>();
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
        .map(text_from_utf16)
        .collect();
    if !problems.is_empty() {
        lines.push(format!(
            "(REG_MULTI_SZ not laid out as goetia writes it: {})",
            problems.join(", ")
        ));
    }
    lines
}

/// Write `lines` as `Services\<name>\Environment`. No lines deletes the value rather than writing
/// an empty `REG_MULTI_SZ`.
pub fn write_environment(name: &str, lines: &[String]) -> Result<()> {
    let path = service_key_path(name);
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
        .map_err(|e| registry_error("open", &path, e))?;
    if lines.is_empty() {
        tolerate_not_found(key.delete_value(ENVIRONMENT_VALUE))
            .map_err(|e| registry_error(&format!("delete {ENVIRONMENT_VALUE} under"), &path, e))
    } else {
        key.set_value(ENVIRONMENT_VALUE, &lines.to_vec())
            .map_err(|e| registry_error(&format!("write {ENVIRONMENT_VALUE} under"), &path, e))
    }
}

// Enumeration for `list` ==============================================================================================

/// Hands every service currently registered with SCM to `keep`, and returns the entry for what the
/// pass did not reach, if anything (see [`flow::scan`]).
pub fn scan_services(keep: impl FnMut(String, &ServicesRoot<'_>)) -> Option<Installed> {
    scan_names_under(SERVICES_KEY, keep)
}

/// [`scan_services`] over another key: `Services` opens on every host that boots, so another key
/// is the only way a test reaches an open that fails, or finds nothing.
fn scan_names_under(path: &str, mut keep: impl FnMut(String, &ServicesRoot<'_>)) -> Option<Installed> {
    let opened = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(path, KEY_READ);
    // The scan borrows the key, so that each name carries a root that cannot outlive it.
    let borrowed = match opened {
        Ok(ref key) => Ok(key),
        Err(e) => Err(e),
    };
    flow::scan(
        path,
        borrowed,
        |key| {
            let key = *key;
            Box::new(
                key.enum_keys()
                    .map(move |name| name.map(|name| (name, ServicesRoot::new(key)))),
            )
        },
        |op, source| registry_error(op, path, source),
        |(name, root)| keep(name, &root),
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
