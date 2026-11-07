//! Removal of a value or subkey by its name's stated length.
//!
//! A name read back from a key may hold a NUL, which the Win32 calls (`RegDeleteValueW`,
//! `RegDeleteTreeW`) take as the end of the name: they would remove goetia's `Marker` for a stray
//! `Marker\0x`, or report success and leave the stray. These go through `ntdll`'s counted names.

use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::{NtClose, OBJECT_ATTRIBUTES};
use windows_sys::Wdk::System::Registry::{NtDeleteKey, NtDeleteValueKey, NtOpenKey};
use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::System::Registry::{KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_SET_VALUE, RegDeleteTreeW};
use winreg::RegKey;

/// The access right to delete a key (`winnt.h`).
const DELETE: u32 = 0x0001_0000;

/// `name` as the counted string `ntdll` takes: its length is stated, and no unit ends it.
fn counted(name: &[u16]) -> UNICODE_STRING {
    // A value name has at most 16 383 units, so its length in bytes fits.
    let bytes = u16::try_from(name.len() * 2).expect("a registry name is at most 16 383 units");
    UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    }
}

fn os_error(status: NTSTATUS) -> io::Error {
    // SAFETY: a pure status-to-code mapping.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

fn checked(status: NTSTATUS) -> io::Result<()> {
    if status >= 0 { Ok(()) } else { Err(os_error(status)) }
}

/// Deletes the value of `key` named `name`, compared case-insensitively.
pub(super) fn delete_value(key: &RegKey, name: &[u16]) -> io::Result<()> {
    let name = counted(name);
    // SAFETY: `key` holds an open handle with `KEY_SET_VALUE`; `name` outlives the call.
    checked(unsafe { NtDeleteValueKey(key.raw_handle(), &name) })
}

/// Deletes the subkey of `key` named `name`, with everything under it.
pub(super) fn delete_subkey(key: &RegKey, name: &[u16]) -> io::Result<()> {
    let name = counted(name);
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: key.raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` is a valid
    // out-param.
    checked(unsafe {
        NtOpenKey(
            &mut handle,
            DELETE | KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE | KEY_SET_VALUE,
            &attributes,
        )
    })?;
    let subkey = Opened(handle);

    // SAFETY: `subkey` holds an open handle with the access `RegDeleteTreeW` documents.
    let status = unsafe { RegDeleteTreeW(subkey.0, null()) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: `subkey` holds an open handle with `DELETE`.
    checked(unsafe { NtDeleteKey(subkey.0) })
}

/// A key opened through `NtOpenKey`, closed on drop.
struct Opened(HANDLE);

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: the handle is open and not closed elsewhere.
        unsafe { NtClose(self.0) };
    }
}

#[cfg(test)]
#[path = "counted_tests.rs"]
mod counted_tests;
