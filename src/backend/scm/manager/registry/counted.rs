//! Access to a value or subkey by its name's stated length, and to a subkey as the key it is.
//!
//! A name read back from a key may hold a NUL, which the Win32 calls (`RegDeleteValueW`,
//! `RegDeleteTreeW`) take as the end of the name: they would remove goetia's `Marker` for a stray
//! `Marker\0x`, or report success and leave the stray. These go through `ntdll`'s counted names.
//! No call that takes a NUL-terminated name is made here, at any depth of a subtree.
//!
//! A registry link is a key that points at another. `RegOpenKeyExW` without a flag, and so `winreg`'s
//! `open_subkey`, enters the target: a write or a delete there would reach a key that is not the
//! one named. A child is opened here as itself, so that a link is a key to delete, never a path.

use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::{NtClose, OBJECT_ATTRIBUTES};
use windows_sys::Wdk::System::Registry::{
    KeyBasicInformation, NtCreateKey, NtDeleteKey, NtDeleteValueKey, NtEnumerateKey, NtOpenKey,
};
use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_OPENLINK, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::System::Registry::{KEY_ENUMERATE_SUB_KEYS, REG_OPTION_NON_VOLATILE};
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
///
/// A child is opened as the key it is: a registry link is deleted itself, never followed, so what
/// it points at is neither entered nor removed. The walk is through `ntdll` alone, so a name at
/// any depth keeps a NUL. `NotFound` is the top-level subkey not being there; one that vanishes
/// below it, between the listing of its parent and its open, is gone already and not an error.
pub(super) fn delete_subkey(key: &RegKey, name: &[u16]) -> io::Result<()> {
    delete_tree(open_raw(key.raw_handle(), name, CHILD_ACCESS)?)
}

/// Opens the subkey of `key` named `name` as the key it is, a registry link included.
pub(super) fn open_child(key: &RegKey, name: &[u16], access: u32) -> io::Result<RegKey> {
    Ok(open_raw(key.raw_handle(), name, access)?.into_key())
}

/// Creates the subkey of `key` named `name`, or opens it if it exists, as the key it is: an
/// existing link is returned as the link, not its target.
pub(super) fn create_child(key: &RegKey, name: &[u16], access: u32) -> io::Result<RegKey> {
    let name = counted(name);
    let attributes = child_attributes(key.raw_handle(), &name);
    let mut handle: HANDLE = null_mut();
    let mut disposition = 0u32;
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` and `disposition`
    // are valid out-params; the class is null, which the API allows.
    checked(unsafe {
        NtCreateKey(
            &mut handle,
            access,
            &attributes,
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            &mut disposition,
        )
    })?;
    Ok(Opened(handle).into_key())
}

/// What a child needs to be listed, entered and deleted.
const CHILD_ACCESS: u32 = DELETE | KEY_ENUMERATE_SUB_KEYS;

/// `NtEnumerateKey` has no more entries.
const STATUS_NO_MORE_ENTRIES: NTSTATUS = 0x8000_001A_u32 as NTSTATUS;

/// The longest key name is 255 units; this holds the header and that.
const BASIC_INFORMATION_BYTES: usize = 1024;

/// Where `KEY_BASIC_INFORMATION` states the length of the name in bytes, and where the name starts
/// (`winternl.h`: an 8-byte time, a 4-byte title index, then those).
const BASIC_NAME_LENGTH_AT: usize = 12;
const BASIC_NAME_AT: usize = 16;

/// How to open the child `name` of `parent`: itself even when it is a link.
fn child_attributes(parent: HANDLE, name: &UNICODE_STRING) -> OBJECT_ATTRIBUTES {
    OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_OPENLINK,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    }
}

/// The name in a `KEY_BASIC_INFORMATION` that `NtEnumerateKey` wrote to `buffer`.
fn basic_name(buffer: &[u8]) -> Vec<u16> {
    let length = u32::from_le_bytes(buffer[BASIC_NAME_LENGTH_AT..BASIC_NAME_AT].try_into().unwrap()) as usize;
    let (units, _) = buffer[BASIC_NAME_AT..BASIC_NAME_AT + length].as_chunks::<2>();
    units.iter().map(|pair| u16::from_le_bytes(*pair)).collect()
}

/// Opens the child `name` of `parent` as the key it is, with `access`.
fn open_raw(parent: HANDLE, name: &[u16], access: u32) -> io::Result<Opened> {
    let name = counted(name);
    let attributes = child_attributes(parent, &name);
    let mut handle: HANDLE = null_mut();
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` is a valid
    // out-param.
    checked(unsafe { NtOpenKey(&mut handle, access, &attributes) })?;
    Ok(Opened(handle))
}

/// Deletes the open `child`, everything under it first. Its values go with it.
fn delete_tree(child: Opened) -> io::Result<()> {
    for grandchild in child_names(child.0)? {
        match open_raw(child.0, &grandchild, CHILD_ACCESS) {
            Ok(grandchild) => delete_tree(grandchild)?,
            // Gone since the listing: what this wanted.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    // SAFETY: `child` holds an open handle with `DELETE`.
    checked(unsafe { NtDeleteKey(child.0) })
}

/// The name of every subkey of the open `key`, by counted name.
fn child_names(key: HANDLE) -> io::Result<Vec<Vec<u16>>> {
    // `u64`s, so that the buffer is aligned for the header's 8-byte time.
    let mut buffer = [0u64; BASIC_INFORMATION_BYTES / 8];
    let mut names = Vec::new();
    for index in 0u32.. {
        let mut written = 0u32;
        // SAFETY: `key` holds an open handle with `KEY_ENUMERATE_SUB_KEYS`; `buffer` is
        // `BASIC_INFORMATION_BYTES` writable bytes; `written` is a valid out-param.
        let status = unsafe {
            NtEnumerateKey(
                key,
                index,
                KeyBasicInformation,
                buffer.as_mut_ptr().cast(),
                BASIC_INFORMATION_BYTES as u32,
                &mut written,
            )
        };
        if status == STATUS_NO_MORE_ENTRIES {
            break;
        }
        checked(status)?;
        // SAFETY: the buffer is initialized, and `buffer.len() * 8` bytes long.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), BASIC_INFORMATION_BYTES) };
        names.push(basic_name(&bytes[..written as usize]));
    }
    Ok(names)
}

/// A key opened through `NtOpenKey`, closed on drop.
struct Opened(HANDLE);

impl Opened {
    /// The key as a `RegKey`, which closes the handle on drop.
    fn into_key(self) -> RegKey {
        let handle = self.0;
        std::mem::forget(self);
        RegKey::predef(handle)
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: the handle is open and not closed elsewhere.
        unsafe { NtClose(self.0) };
    }
}

#[cfg(test)]
#[path = "counted_tests.rs"]
mod counted_tests;
