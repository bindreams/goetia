//! Openers for a service's registry keys under `HKLM\SYSTEM\CurrentControlSet\Services`.

use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;

use super::SCM_SERVICES_KEY;

/// `Services\<id>`, relative to `HKLM`.
pub fn service_path(id: &str) -> String {
    format!(r"{SCM_SERVICES_KEY}\{id}")
}

/// `Services\<id>\Parameters`, relative to `HKLM`.
pub fn parameters_path(id: &str) -> String {
    format!(r"{}\Parameters", service_path(id))
}

fn open(path: &str, access: u32) -> std::io::Result<RegKey> {
    RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(path, access)
}

fn create(path: &str) -> RegKey {
    let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey(path)
        .unwrap_or_else(|e| panic!(r"create HKLM\{path}: {e}"));
    key
}

/// `Services\<id>` opened with `access` (`KEY_READ`, `KEY_WRITE`, ...), or why it could not be.
pub fn try_service_key(id: &str, access: u32) -> std::io::Result<RegKey> {
    open(&service_path(id), access)
}

/// [`try_service_key`] for a key that must open.
pub fn service_key(id: &str, access: u32) -> RegKey {
    try_service_key(id, access).unwrap_or_else(|e| panic!(r"open HKLM\{}: {e}", service_path(id)))
}

/// `Services\<id>` opened for writing, created if absent.
pub fn create_service_key(id: &str) -> RegKey {
    create(&service_path(id))
}

/// `Services\<id>\Parameters` opened with `access`, or why it could not be.
pub fn try_parameters_key(id: &str, access: u32) -> std::io::Result<RegKey> {
    open(&parameters_path(id), access)
}

/// [`try_parameters_key`] for a key that must open.
pub fn parameters_key(id: &str, access: u32) -> RegKey {
    try_parameters_key(id, access).unwrap_or_else(|e| panic!(r"open HKLM\{}: {e}", parameters_path(id)))
}

/// `Services\<id>\Parameters` opened for writing, created if absent.
pub fn create_parameters_key(id: &str) -> RegKey {
    create(&parameters_path(id))
}
