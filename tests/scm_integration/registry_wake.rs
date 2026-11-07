//! Probe behind `read_settled`'s wait (`src/backend/scm/manager/registry/watch.rs`): does a
//! `REG_NOTIFY_CHANGE_NAME` notification on a key fire when a subkey that another handle held
//! open, after it was deleted (marked for deletion), is finally removed by that handle closing?
//! Only the answer is recorded; no outcome fails the test.
//!
//! The wait ends on the notification or on `done`, which the probe sets itself once it has closed
//! the handle and seen the subkey gone, so both outcomes are recorded and neither can hang.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, WAIT_OBJECT_0};
use windows_sys::Win32::System::Registry::{REG_NOTIFY_CHANGE_NAME, RegNotifyChangeKeyValue};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_NOTIFY, KEY_READ};

use crate::support::cleanup::{retrying, warn_left_behind};
use crate::support::{self, ELEVATED};

/// Removes `HKLM\<path>` and everything under it on drop, retried; never panics.
struct HklmTreeGuard(String);

impl Drop for HklmTreeGuard {
    fn drop(&mut self) {
        warn_left_behind(retrying(|| {
            match RegKey::predef(HKEY_LOCAL_MACHINE).delete_subkey_all(&self.0) {
                Ok(()) => None,
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => Some(format!(r"remove HKLM\{}: {e}", self.0)),
            }
        }));
    }
}

/// A manual-reset, unsignalled event.
fn event() -> OwnedHandle {
    // SAFETY: null security descriptor and name; a null return is the failure.
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    assert!(!handle.is_null(), "CreateEventW: {}", io::Error::last_os_error());
    // SAFETY: a valid handle `CreateEventW` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

fn is_signalled(event: &OwnedHandle) -> bool {
    // SAFETY: `event` is a live event handle. A zero-length wait only reads its state.
    unsafe { WaitForSingleObject(event.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn registry_notification_on_final_removal_of_a_held_deleted_subkey() {
    let parent_path = format!(r"SOFTWARE\{}", support::random_test_id());
    let _guard = HklmTreeGuard(parent_path.clone());
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let subkey_path = format!(r"{parent_path}\Parameters");

    let (parent, _) = hklm.create_subkey(&parent_path).expect("create the parent key");
    let (held, _) = parent.create_subkey("Parameters").expect("create the subkey");

    // The deletion happens while `held` is open, so the subkey is only marked for deletion. The
    // watch is armed after it, as `ServiceWatch` is armed after the read that found the key gone.
    parent
        .delete_subkey("Parameters")
        .expect("delete the subkey while another handle holds it");
    let open_while_held = hklm.open_subkey_with_flags(&subkey_path, KEY_READ).err();

    let watched = hklm
        .open_subkey_with_flags(&parent_path, KEY_NOTIFY)
        .expect("open the parent for notification");
    let fired = event();
    // SAFETY: `watched` holds an open `KEY_NOTIFY` handle and `fired` a live event; both outlive the wait.
    let status = unsafe {
        RegNotifyChangeKeyValue(
            watched.raw_handle(),
            0,
            REG_NOTIFY_CHANGE_NAME,
            fired.as_raw_handle(),
            1,
        )
    };
    assert_eq!(
        status,
        ERROR_SUCCESS,
        "RegNotifyChangeKeyValue: {}",
        io::Error::from_raw_os_error(status as i32)
    );
    let signalled_before_close = is_signalled(&fired);

    drop(held);
    let open_after_close = hklm.open_subkey_with_flags(&subkey_path, KEY_READ).err();
    let done = event();
    // SAFETY: `done` is a live event handle.
    assert_ne!(
        unsafe { SetEvent(done.as_raw_handle()) },
        0,
        "SetEvent: {}",
        io::Error::last_os_error()
    );

    let handles = [fired.as_raw_handle(), done.as_raw_handle()];
    // SAFETY: two live event handles, in an array that outlives the call.
    let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
    let fired_by_close = woke == WAIT_OBJECT_0;
    assert!(
        fired_by_close || woke == WAIT_OBJECT_0 + 1,
        "WaitForMultipleObjects: {woke:#x} ({})",
        io::Error::last_os_error()
    );

    let describe = |e: &Option<io::Error>| e.as_ref().map_or("opened".to_string(), |e| e.to_string());
    support::record_probe(
        "scm-registry-wake-on-final-removal",
        &format!(
            "mechanism: RegNotifyChangeKeyValue(parent, REG_NOTIFY_CHANGE_NAME) armed after the subkey was deleted while held\n\
             open of the deleted subkey while held: {}\n\
             signalled before the handle closed: {signalled_before_close}\n\
             open of the subkey after the handle closed: {}\n\
             fired by the time the subkey was seen gone: {fired_by_close}\n",
            describe(&open_while_held),
            describe(&open_after_close),
        ),
    );
}
