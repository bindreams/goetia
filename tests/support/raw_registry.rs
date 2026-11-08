//! Registry calls that `winreg` cannot make: a name with a NUL in it, a key that is a link. Shared
//! by the integration suites (through `support`) and the library's unit tests (as
//! `backend::test_raw_registry`, by path).

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::System::Registry::{
    KeyFlagsInformation, NtCreateKey, NtDeleteKey, NtOpenKey, NtQueryKey, NtSetValueKey,
};
use windows_sys::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, OBJ_OPENLINK, STATUS_OBJECT_NAME_NOT_FOUND, UNICODE_STRING,
};

use super::cleanup::{retrying, warn_left_behind};

/// `name` (UTF-16 units, any, a NUL included) as the counted string `ntdll` takes. It points at
/// `name`, which must outlive the call it is passed to.
fn counted(name: &[u16]) -> UNICODE_STRING {
    // A registry name has at most 16 383 units, so its length in bytes fits.
    let bytes = u16::try_from(name.len() * 2).expect("a registry name of at most 16 383 units");
    UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    }
}

/// How to open the child `name` of `parent`: itself even when it is a link.
fn child_attributes(parent: HANDLE, name: &UNICODE_STRING) -> OBJECT_ATTRIBUTES {
    OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_OPENLINK,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    }
}

/// A raw `NtSetValueKey` of `name` (UTF-16 units, any, a NUL included) with `vtype` and `data`.
pub fn set_raw_value(key: &winreg::RegKey, name: &[u16], vtype: u32, data: &[u8]) {
    let name = counted(name);
    // SAFETY: `key` holds an open handle with write access; `name` points at units that outlive the
    // call, and `data` is `data.len()` readable bytes.
    let status = unsafe {
        NtSetValueKey(
            key.raw_handle(),
            &name,
            0,
            vtype,
            data.as_ptr().cast(),
            data.len() as u32,
        )
    };
    assert!(status >= 0, "write a value of type {vtype}: NTSTATUS {status:#x}");
}

/// A raw `NtCreateKey` of the child `name` (UTF-16 units, any, a NUL included) of `parent`, with
/// `options` (`REG_OPTION_CREATE_LINK` makes a link). The key is returned open.
pub fn create_raw_key(parent: &winreg::RegKey, name: &[u16], options: u32) -> winreg::RegKey {
    const KEY_ALL_ACCESS: u32 = 0xF003F;
    let name = counted(name);
    let attributes = child_attributes(parent.raw_handle(), &name);
    let mut handle = std::ptr::null_mut();
    let mut disposition = 0u32;
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` and `disposition`
    // are valid out-params; the class is null, which the API allows.
    let status = unsafe {
        NtCreateKey(
            &mut handle,
            KEY_ALL_ACCESS,
            &attributes,
            0,
            std::ptr::null(),
            options,
            &mut disposition,
        )
    };
    assert!(status >= 0, "create a key: NTSTATUS {status:#x}");
    // `winreg` closes a handle it holds on drop, which is all this needs from it.
    winreg::RegKey::predef(handle)
}

/// A registry link made by [`create_link`], removed on drop if it is still a link.
pub struct LinkGuard<'p> {
    parent: &'p winreg::RegKey,
    name: Vec<u16>,
}

/// Makes the link `name` below `parent`, pointing at `target_nt_path` (as [`ScratchKey::nt_path`]
/// names a key), and returns what removes it. The guard exists before the link, so that a failed
/// step still removes it. Declare it after the guards of `parent`'s owner and of the target, so that
/// the link goes first.
///
/// [`ScratchKey::nt_path`]: super::scratch::ScratchKey::nt_path
pub fn create_link<'p>(parent: &'p winreg::RegKey, name: &str, target_nt_path: &str) -> LinkGuard<'p> {
    const REG_LINK: u32 = 6;
    const REG_OPTION_CREATE_LINK: u32 = 2;
    let guard = LinkGuard {
        parent,
        name: name.encode_utf16().collect(),
    };
    let link = create_raw_key(parent, &guard.name, REG_OPTION_CREATE_LINK);
    // No closing NUL, which is how a link's target is stored, so that the link resolves.
    let target: Vec<u8> = target_nt_path.encode_utf16().flat_map(u16::to_le_bytes).collect();
    set_raw_value(
        &link,
        &"SymbolicLinkValue".encode_utf16().collect::<Vec<u16>>(),
        REG_LINK,
        &target,
    );
    guard
}

impl LinkGuard<'_> {
    /// One attempt to remove the link; the problem it leaves, if any. A key that is gone, or a real
    /// key that took the link's place (the product replaced it), is nothing to remove.
    fn remove(&self) -> Option<String> {
        // `DELETE` and `KEY_QUERY_VALUE`.
        const ACCESS: u32 = 0x0001_0000 | 0x0001;
        // `REG_FLAG_LINK` (`ntregapi.h`) in the key's own flags, the second word of
        // `KEY_FLAGS_INFORMATION`.
        const REG_FLAG_LINK: u32 = 0x0002;
        let name = counted(&self.name);
        let attributes = child_attributes(self.parent.raw_handle(), &name);
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: `attributes` and the name it points at outlive the call; `handle` is a valid
        // out-param.
        let status = unsafe { NtOpenKey(&mut handle, ACCESS, &attributes) };
        if status == STATUS_OBJECT_NAME_NOT_FOUND {
            return None;
        }
        if status < 0 {
            return Some(format!("open the link: NTSTATUS {status:#x}"));
        }
        // `winreg` closes a handle it holds on drop.
        let key = winreg::RegKey::predef(handle);
        let mut flags = [0u32; 3];
        let mut written = 0u32;
        // SAFETY: `key` holds an open handle; `flags` is 12 writable bytes; `written` is a valid
        // out-param.
        let status = unsafe {
            NtQueryKey(
                key.raw_handle(),
                KeyFlagsInformation,
                flags.as_mut_ptr().cast(),
                size_of_val(&flags) as u32,
                &mut written,
            )
        };
        if status < 0 || written < 8 {
            return Some(format!("query the link's flags: NTSTATUS {status:#x}, {written} bytes"));
        }
        if flags[1] & REG_FLAG_LINK == 0 {
            return None;
        }
        // SAFETY: `key` holds an open handle with `DELETE`. It was opened as itself, so this deletes
        // the link, never its target.
        let status = unsafe { NtDeleteKey(key.raw_handle()) };
        (status < 0).then(|| format!("delete the link: NTSTATUS {status:#x}"))
    }
}

impl Drop for LinkGuard<'_> {
    /// Never panics, so a drop that runs while the test unwinds still removes the link.
    fn drop(&mut self) {
        warn_left_behind(retrying(|| self.remove()));
    }
}
