//! Registry calls that `winreg` cannot make: a name with a NUL in it, a key that is a link. Shared
//! by the integration suites (through `support`) and the library's unit tests (as
//! `backend::test_raw_registry`, by path).

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::System::Registry::{NtCreateKey, NtSetValueKey};
use windows_sys::Win32::Foundation::{OBJ_CASE_INSENSITIVE, OBJ_OPENLINK, UNICODE_STRING};

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
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_OPENLINK,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
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
