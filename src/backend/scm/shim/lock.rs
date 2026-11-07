//! The machine-wide install lock: a named mutex that serialises everything that places, repairs
//! or deletes shim files, across processes and across goetia versions.
//!
//! The mutex is created with a DACL for SYSTEM and Administrators only, so no other account can
//! open it, hold it or release it, and one that exists already is used only if a trusted account
//! owns it: a mutex another account made first could be held forever, or never held at all.
//!
//! A service account holds `SeCreateGlobalPrivilege`, so it can create `Global\goetia-shim` before
//! any install does. The refusal above is all the defence there is: a global name specific enough
//! to avoid an accidental collision needs none against a deliberate one, as with an environment
//! variable.
use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_INVALID_OWNER, GetLastError, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Threading::{CreateMutexW, INFINITE, OpenMutexW, ReleaseMutex, WaitForSingleObject};

use super::acl::{Descriptor, account_name, is_trusted, kernel_object_owner};
use crate::backend::scm::manager::identity::wide_null;
use crate::elevation::is_elevated;
use crate::error::{Error, Result};

/// The mutex's name, which is also what its errors call it.
const NAME: &str = r"Global\goetia-shim";

/// Held from [`acquire`](Self::acquire) until it is dropped. Not `Send`: a mutex is released by the
/// thread that holds it.
pub(crate) struct InstallLock {
    mutex: OwnedHandle,
    _thread: PhantomData<*const ()>,
}

impl InstallLock {
    /// Takes the lock, waiting for whoever holds it, with no bound: the holder's release is the
    /// event. A lock whose holder died is taken all the same (`WAIT_ABANDONED`): what it guarded is
    /// verified by whoever takes it, not assumed from it. A lock that exists and belongs to an
    /// account other than SYSTEM, Administrators or TrustedInstaller is
    /// [`Error::ShimLockForeign`], before any wait.
    pub(crate) fn acquire() -> Result<Self> {
        let descriptor = Descriptor::for_lock()?;
        let attributes = descriptor.attributes();
        let name = wide_null(NAME);
        // SAFETY: `attributes` points at `descriptor`, which is live for the call; `name` is
        // NUL-terminated; the mutex is not owned by the caller at creation.
        let raw = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
        // SAFETY: read straight after the call, before anything else can change it.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if raw.is_null() {
            return Err(create_failed(io::Error::last_os_error()));
        }
        // SAFETY: a valid handle `CreateMutexW` just returned and nothing else owns.
        let mutex = unsafe { OwnedHandle::from_raw_handle(raw) };
        if existed {
            let owner = kernel_object_owner(mutex.as_raw_handle(), Path::new(NAME))?;
            if !is_trusted(&owner) {
                return Err(Error::ShimLockForeign {
                    account: account_name(&owner),
                });
            }
        }
        // SAFETY: `mutex` is a live handle with `SYNCHRONIZE`.
        match unsafe { WaitForSingleObject(mutex.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self {
                mutex,
                _thread: PhantomData,
            }),
            _ => Err(Error::os(
                format!("wait for the install lock `{NAME}`"),
                io::Error::last_os_error(),
            )),
        }
    }
}

/// What a failed `CreateMutexW` says about the lock and the caller.
#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    /// The caller's token lacks Administrators: [`Error::ElevationRequired`].
    NeedsElevation,
    /// Someone else made the lock, with a DACL that shuts Administrators out:
    /// [`Error::ShimLockForeign`].
    ForeignLock,
    /// The OS's failure, as it was.
    Os,
}

/// The lock's owner is Administrators, which a token that does not have it enabled cannot give an
/// object (`ERROR_INVALID_OWNER`), and its DACL admits only SYSTEM and Administrators, so that token
/// cannot open one that exists either (`ERROR_ACCESS_DENIED`): both are its lack of elevation. An
/// `elevated` caller refused by that DACL was shut out by whoever made the lock.
fn failure_kind(code: Option<i32>, elevated: bool) -> FailureKind {
    match code {
        Some(code) if code == ERROR_INVALID_OWNER as i32 => FailureKind::NeedsElevation,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 && elevated => FailureKind::ForeignLock,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 => FailureKind::NeedsElevation,
        _ => FailureKind::Os,
    }
}

/// The error for a failed `CreateMutexW`.
fn create_failed(source: io::Error) -> Error {
    let code = source.raw_os_error();
    // Only a denial depends on the caller, and asking opens SCM.
    let elevated = code == Some(ERROR_ACCESS_DENIED as i32) && is_elevated();
    match failure_kind(code, elevated) {
        FailureKind::NeedsElevation => Error::ElevationRequired {
            what: format!("creating or opening the install lock `{NAME}`"),
        },
        FailureKind::ForeignLock => Error::ShimLockForeign {
            account: denied_lock_owner(),
        },
        FailureKind::Os => Error::os(format!("create the install lock `{NAME}`"), source),
    }
}

/// The owner of the lock that refused the caller, read through an open that asks for no more than
/// `READ_CONTROL`, or a note that it could not be read: the DACL that refused the caller may refuse
/// that too.
fn denied_lock_owner() -> String {
    let name = wide_null(NAME);
    // SAFETY: `name` is NUL-terminated.
    let raw = unsafe { OpenMutexW(READ_CONTROL, 0, name.as_ptr()) };
    if raw.is_null() {
        return UNREADABLE_OWNER.to_string();
    }
    // SAFETY: a valid handle `OpenMutexW` just returned and nothing else owns.
    let mutex = unsafe { OwnedHandle::from_raw_handle(raw) };
    kernel_object_owner(mutex.as_raw_handle(), Path::new(NAME))
        .map(|owner| account_name(&owner))
        .unwrap_or_else(|_| UNREADABLE_OWNER.to_string())
}

/// What [`Error::ShimLockForeign`] names as the owner when the owner cannot be read.
const UNREADABLE_OWNER: &str = "an account that could not be read";

impl Drop for InstallLock {
    fn drop(&mut self) {
        // SAFETY: the calling thread holds the mutex: it took it in `acquire` and `Self` is not
        // `Send`. The handle closes after.
        let released = unsafe { ReleaseMutex(self.mutex.as_raw_handle()) };
        debug_assert_ne!(released, 0, "ReleaseMutex: {}", io::Error::last_os_error());
    }
}

#[cfg(test)]
#[path = "lock_tests.rs"]
mod lock_tests;
