//! Access to a value or subkey by its name's stated length, and to a subkey as the key it is.
//!
//! A name read back from a key may hold a NUL, which the Win32 calls (`RegDeleteValueW`,
//! `RegDeleteTreeW`) take as the end of the name: they would remove goetia's `Marker` for a stray
//! `Marker\0x`, or report success and leave the stray. These go through `ntdll`'s counted names.
//! No call that takes a NUL-terminated name is made here, at any depth of a subtree.
//!
//! A registry link is a key that points at another. `RegOpenKeyExW` without a flag, and so `winreg`'s
//! `open_subkey`, enters the target: a write or a delete there would reach a key that is not the
//! one named. A child is opened here as itself, so that a link is a key to delete, never a path,
//! unless the caller asks for [`LinkHandling::Followed`]: only a read of what SCM reads does.

use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::{NtClose, OBJECT_ATTRIBUTES};
use windows_sys::Wdk::System::Registry::{
    KeyBasicInformation, KeyFlagsInformation, NtCreateKey, NtDeleteKey, NtDeleteValueKey, NtEnumerateKey, NtOpenKey,
    NtQueryKey,
};
use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_OPENLINK, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::System::Registry::{KEY_ENUMERATE_SUB_KEYS, REG_OPTION_NON_VOLATILE};
use winreg::RegKey;

use super::settle::{gone_is_done, tolerate_not_found};

/// The access right to delete a key (`winnt.h`).
const DELETE: u32 = 0x0001_0000;

/// A key or value name, or a path of key names, as UTF-16 with no terminator.
///
/// Every function here takes its names as a `Name`, and `ntdll` reads them as counted strings: the
/// length is stated, so a NUL in a name is part of it, not its end. [`Name::new`] adds no
/// terminator; [`Name::from_units`] takes units as the registry returned them and checks nothing,
/// so a terminated buffer (`wide_null`) passed to it would name a different key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Name(Vec<u16>);

impl Name {
    /// The name `text` spells.
    pub(super) fn new(text: &str) -> Self {
        Self(text.encode_utf16().collect())
    }

    /// A name as the registry returned it, any units it holds included.
    pub(super) fn from_units(units: impl Into<Vec<u16>>) -> Self {
        Self(units.into())
    }

    /// The units of the name.
    pub(super) fn units(&self) -> &[u16] {
        &self.0
    }
}

/// `name` as the counted string `ntdll` takes.
fn counted(name: &Name) -> UNICODE_STRING {
    // A value name has at most 16 383 units, so its length in bytes fits.
    let bytes = u16::try_from(name.0.len() * 2).expect("a registry name is at most 16 383 units");
    UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.0.as_ptr().cast_mut(),
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
pub(super) fn delete_value(key: &RegKey, name: &Name) -> io::Result<()> {
    let name = counted(name);
    // SAFETY: `key` holds an open handle with `KEY_SET_VALUE`; `name` outlives the call.
    checked(unsafe { NtDeleteValueKey(key.raw_handle(), &name) })
}

/// Deletes the subkey of `key` named `name`, with everything under it.
///
/// A child is opened as the key it is: a registry link is deleted itself, never followed, so what
/// it points at is neither entered nor removed. The walk is through `ntdll` alone, so a name at
/// any depth keeps a NUL. A key that is not there, or is deleted, at its listing or its delete, or
/// at its open below `name`, is gone already, which is what this wants: not an error. At the open
/// of `name` itself only NotFound is: KEY_DELETED there says that `key`, the held parent, was
/// marked deleted, not that the child is gone.
pub(super) fn delete_subkey(key: &RegKey, name: &Name) -> io::Result<()> {
    match open_raw(key.raw_handle(), name, CHILD_ACCESS, LinkHandling::Itself) {
        Ok(child) => delete_tree(child),
        Err(e) => tolerate_not_found(Err(e)),
    }
}

/// Whether an open stops at a registry link or enters it.
#[derive(Clone, Copy)]
pub(super) enum LinkHandling {
    /// The link is the key opened.
    Itself,
    /// The link's target is the key opened.
    Followed,
}

/// Opens the subkey of the key `parent` named `name`, which may be a path of names, with `access`.
/// `parent` is an open key, whose own access is not asked.
pub(super) fn open_child(parent: HANDLE, name: &Name, access: u32, links: LinkHandling) -> io::Result<RegKey> {
    Ok(open_raw(parent, name, access, links)?.into_key())
}

/// `REG_FLAG_LINK` (`ntregapi.h`): the key is a registry link.
const REG_FLAG_LINK: u32 = 0x0002;

/// Whether `key`, opened with `KEY_QUERY_VALUE`, is a registry link, by its own flags: not by a
/// value it may hold, which a key can hold without being a link and a link can lack.
pub(super) fn is_link(key: &RegKey) -> io::Result<bool> {
    // `KEY_FLAGS_INFORMATION`: the Wow64 flags, the key's flags, the control flags.
    let mut flags = [0u32; 3];
    let mut written = 0u32;
    // SAFETY: `key` holds an open handle; `flags` is 12 writable bytes; `written` is a valid
    // out-param.
    checked(unsafe {
        NtQueryKey(
            key.raw_handle(),
            KeyFlagsInformation,
            flags.as_mut_ptr().cast(),
            size_of_val(&flags) as u32,
            &mut written,
        )
    })?;
    link_flag(flags, written)
}

/// The decision of [`is_link`], from the flags `NtQueryKey` wrote in `written` bytes. A reply too
/// short to hold the key's flags says nothing about them, and is an error, not "not a link".
fn link_flag(flags: [u32; 3], written: u32) -> io::Result<bool> {
    // The key's flags are the second of the words.
    const FLAGS_END: u32 = 2 * size_of::<u32>() as u32;
    if written < FLAGS_END {
        return Err(io::Error::other(format!(
            "NtQueryKey wrote {written} bytes of the key's flags, which need {FLAGS_END}"
        )));
    }
    Ok(flags[1] & REG_FLAG_LINK != 0)
}

/// Creates the subkey of `key` named `name`, or opens it if it exists, as the key it is: an
/// existing link is returned as the link, not its target.
pub(super) fn create_child(key: &RegKey, name: &Name, access: u32) -> io::Result<RegKey> {
    let name = counted(name);
    let attributes = child_attributes(key.raw_handle(), &name, LinkHandling::Itself);
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

/// How to open the child `name` of `parent`, a link as `links` says.
fn child_attributes(parent: HANDLE, name: &UNICODE_STRING, links: LinkHandling) -> OBJECT_ATTRIBUTES {
    let link = match links {
        LinkHandling::Itself => OBJ_OPENLINK,
        LinkHandling::Followed => 0,
    };
    OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: name,
        Attributes: OBJ_CASE_INSENSITIVE | link,
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

/// Opens the child `name` of `parent` with `access`, a link as `links` says.
fn open_raw(parent: HANDLE, name: &Name, access: u32, links: LinkHandling) -> io::Result<Opened> {
    let name = counted(name);
    let attributes = child_attributes(parent, &name, links);
    let mut handle: HANDLE = null_mut();
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` is a valid
    // out-param.
    checked(unsafe { NtOpenKey(&mut handle, access, &attributes) })?;
    Ok(Opened(handle))
}

/// Deletes the open `child`, everything under it first. Its values go with it. A key that is gone
/// by then, at its listing, below it or at its delete, is done.
fn delete_tree(child: Opened) -> io::Result<()> {
    let names = match child_names(child.0) {
        Ok(names) => names,
        Err(e) => return gone_is_done(Err(e)),
    };
    for grandchild in names {
        match open_raw(child.0, &grandchild, CHILD_ACCESS, LinkHandling::Itself) {
            Ok(grandchild) => delete_tree(grandchild)?,
            Err(e) => gone_is_done(Err(e))?,
        }
    }
    // SAFETY: `child` holds an open handle with `DELETE`.
    gone_is_done(checked(unsafe { NtDeleteKey(child.0) }))
}

/// The name of every subkey of the open `key`, by counted name.
fn child_names(key: HANDLE) -> io::Result<Vec<Name>> {
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
        names.push(Name::from_units(basic_name(&bytes[..written as usize])));
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
