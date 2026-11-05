//! Openers for a service's registry keys under `HKLM\SYSTEM\CurrentControlSet\Services`.

use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;

use super::SCM_SERVICES_KEY;

/// `Services\<id>` opened with `access` (`KEY_READ`, `KEY_WRITE`, ...), or why it could not be.
pub fn try_service_key(id: &str, access: u32) -> std::io::Result<RegKey> {
    RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(format!(r"{SCM_SERVICES_KEY}\{id}"), access)
}

/// [`try_service_key`] for a key that must open.
pub fn service_key(id: &str, access: u32) -> RegKey {
    try_service_key(id, access).unwrap_or_else(|e| panic!(r"open HKLM\{SCM_SERVICES_KEY}\{id}: {e}"))
}

/// `Services\<id>\Parameters` opened with `access`, or why it could not be.
pub fn try_parameters_key(id: &str, access: u32) -> std::io::Result<RegKey> {
    RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(format!(r"{SCM_SERVICES_KEY}\{id}\Parameters"), access)
}

/// [`try_parameters_key`] for a key that must open.
pub fn parameters_key(id: &str, access: u32) -> RegKey {
    try_parameters_key(id, access).unwrap_or_else(|e| panic!(r"open HKLM\{SCM_SERVICES_KEY}\{id}\Parameters: {e}"))
}
