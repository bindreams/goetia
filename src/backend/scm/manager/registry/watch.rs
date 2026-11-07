//! A wait for a change under a service's key, so that a read that must be retried waits for
//! something to change instead of reading again at once.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, WAIT_OBJECT_0};
use windows_sys::Win32::System::Registry::{REG_NOTIFY_CHANGE_NAME, RegNotifyChangeKeyValue};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_NOTIFY};

/// A pending notification that a subkey of one key was created or deleted, or that the key itself
/// was deleted. Single-shot. Everything that changes after [`ServiceWatch::arm`] returns is caught by
/// the [`wait`](ServiceWatch::wait) that follows, however late it is called, so a read made after
/// arming loses no change between the read and the wait.
pub(super) struct ServiceWatch {
    // Closing the key signals the event, so the key outlives the wait.
    _key: RegKey,
    event: OwnedHandle,
}

impl ServiceWatch {
    /// Watch the names under `path`, which is a key under `HKEY_LOCAL_MACHINE`. The error is that of
    /// the open or of arming; a `path` that does not exist is `NotFound`.
    pub fn arm(path: &str) -> io::Result<Self> {
        let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(path, KEY_NOTIFY)?;
        // SAFETY: no pointers are passed but a null security descriptor and name; a null return is
        // the failure. A manual-reset event, unsignalled.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a valid handle `CreateEventW` just returned and nothing else owns.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        // SAFETY: `key` holds an open handle with `KEY_NOTIFY`, and `event` a live event handle.
        // Both are held by the returned value until the notification is delivered or dropped.
        let status =
            unsafe { RegNotifyChangeKeyValue(key.raw_handle(), 0, REG_NOTIFY_CHANGE_NAME, event.as_raw_handle(), 1) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Self { _key: key, event })
    }

    /// Block until something changes under the key. Unbounded on purpose: the change is the event,
    /// and there is no outcome to report before it.
    pub fn wait(self) -> io::Result<()> {
        // SAFETY: `event` is a live event handle.
        match unsafe { WaitForSingleObject(self.event.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
