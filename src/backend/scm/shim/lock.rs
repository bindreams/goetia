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
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_INVALID_OWNER, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Threading::{CreateMutexW, INFINITE, OpenMutexW, ReleaseMutex, WaitForSingleObject};

use super::acl::{Descriptor, Read, account_name, is_trusted, kernel_object_security};
use crate::backend::scm::manager::identity::wide_null;
use crate::backend::scm::sid::Sid;
use crate::elevation::token_has_administrators;
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
            let owner = owner_of(mutex.as_raw_handle())?;
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
    /// The lock refused a token that has Administrators enabled: [`Error::ShimLockDenied`].
    Denied,
    /// The OS's failure, as it was.
    Os,
}

/// The lock's owner is Administrators, which a token that does not have it enabled cannot give an
/// object (`ERROR_INVALID_OWNER`), and its DACL admits only SYSTEM and Administrators, so that token
/// cannot open one that exists either (`ERROR_ACCESS_DENIED`): both are its lack of elevation. A
/// token with `administrators` that is refused has a cause this does not know (the lock's DACL, or
/// the token's integrity level or privileges).
fn failure_kind(code: Option<i32>, administrators: bool) -> FailureKind {
    match code {
        Some(code) if code == ERROR_INVALID_OWNER as i32 => FailureKind::NeedsElevation,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 && administrators => FailureKind::Denied,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 => FailureKind::NeedsElevation,
        _ => FailureKind::Os,
    }
}

/// The error for a failed `CreateMutexW`.
fn create_failed(source: io::Error) -> Error {
    failure(source, token_has_administrators, denied_lock_owner)
}

/// [`create_failed`] over what it asks of the system: whether the caller's token has
/// `administrators` enabled, asked only for a denial, the one code that depends on it; and the
/// `owner` of the lock that refused it, asked only when it is told to the caller.
fn failure(
    source: io::Error,
    administrators: impl FnOnce() -> io::Result<bool>,
    owner: impl FnOnce() -> Option<String>,
) -> Error {
    let create = format!("create the install lock `{NAME}`");
    let code = source.raw_os_error();
    let administrators = if code == Some(ERROR_ACCESS_DENIED as i32) {
        match administrators() {
            Ok(enabled) => enabled,
            Err(asked) => {
                return Error::Both {
                    first: Box::new(Error::os(create, source)),
                    then: Box::new(Error::os(
                        "check whether the token has Administrators enabled".to_string(),
                        asked,
                    )),
                };
            }
        }
    } else {
        false
    };
    match failure_kind(code, administrators) {
        FailureKind::NeedsElevation => Error::ElevationRequired {
            what: format!("creating or opening the install lock `{NAME}`"),
        },
        FailureKind::Denied => Error::ShimLockDenied { owner: owner() },
        FailureKind::Os => Error::os(create, source),
    }
}

/// The owner of the lock that `mutex`, open with `READ_CONTROL`, is a handle to. Its DACL is not read.
fn owner_of(mutex: HANDLE) -> Result<Sid> {
    let security = kernel_object_security(mutex, Path::new(NAME), Read::Owner)?;
    Ok(security.owner.expect("the owner was asked for"))
}

/// The owner of the lock that refused the caller, read through an open that asks for no more than
/// `READ_CONTROL`, or `None` when it could not be read: the DACL that refused the caller may refuse
/// that too.
fn denied_lock_owner() -> Option<String> {
    let name = wide_null(NAME);
    // SAFETY: `name` is NUL-terminated.
    let raw = unsafe { OpenMutexW(READ_CONTROL, 0, name.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: a valid handle `OpenMutexW` just returned and nothing else owns.
    let mutex = unsafe { OwnedHandle::from_raw_handle(raw) };
    owner_of(mutex.as_raw_handle()).map(|owner| account_name(&owner)).ok()
}

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
