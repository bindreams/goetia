//! The machine-wide install lock: a named mutex that serialises everything that places, repairs
//! or deletes shim files, across processes and across goetia versions.
//!
//! The mutex lives in a private namespace whose boundary is the Administrators SID, so a process
//! can create, open or hold it only with Administrators enabled in its token. A name in the
//! `Global\` namespace would not do: every service account holds `SeCreateGlobalPrivilege`, so a
//! process of `LocalService` could create `Global\goetia-shim` first and block every install. The
//! namespace and the mutex are both for SYSTEM and Administrators alone.
//!
//! A namespace lives while a handle to it is open, and a mutex is one object for two processes only
//! while the namespace they found it in is. So a process that holds the lock holds the namespace
//! too: it opens the namespace once, keeps the handle for the life of the process, and closes the
//! mutex alone when the lock is dropped.
use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::{Mutex, PoisonError};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, CreateBoundaryDescriptorW, CreateMutexW, CreatePrivateNamespaceW,
    DeleteBoundaryDescriptor, INFINITE, OpenPrivateNamespaceW, ReleaseMutex, WaitForSingleObject,
};

use super::acl::{Descriptor, administrators};
use crate::backend::scm::manager::identity::wide_null;
use crate::error::{Error, Result};

/// The namespace's alias, and the name of its boundary descriptor.
const NAMESPACE: &str = "goetia-shim";

/// The mutex's name in the namespace, which is also what its errors call it.
const NAME: &str = r"goetia-shim\install-lock";

/// Whether this process has the namespace open. It is opened once and never closed: a second open
/// of the alias in one process fails (`ERROR_DUP_NAME`), and a lock a thread of this process holds
/// must keep the namespace alive for the other processes that find it.
static NAMESPACE_OPEN: Mutex<bool> = Mutex::new(false);

/// Held from [`acquire`](Self::acquire) until it is dropped. Not `Send`: a mutex is released by the
/// thread that holds it.
pub(crate) struct InstallLock {
    mutex: OwnedHandle,
    _thread: PhantomData<*const ()>,
}

impl InstallLock {
    /// Takes the lock, waiting for whoever holds it, with no bound: the holder's release is the
    /// event. A lock whose holder died is taken all the same (`WAIT_ABANDONED`): what it guarded is
    /// verified by whoever takes it, not assumed from it. Fails when the caller's token does not
    /// have Administrators enabled (an unelevated administrator, a service account).
    pub(crate) fn acquire() -> Result<Self> {
        let descriptor = Descriptor::for_lock()?;
        let attributes = descriptor.attributes();
        open_namespace(&descriptor)?;
        let name = wide_null(NAME);
        // SAFETY: `attributes` points at `descriptor`, which is live for the call; `name` is
        // NUL-terminated; the mutex is not owned by the caller at creation.
        let raw = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(Error::os(
                format!("create the install lock `{NAME}`"),
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: a valid handle `CreateMutexW` just returned and nothing else owns.
        let mutex = unsafe { OwnedHandle::from_raw_handle(raw) };
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

impl Drop for InstallLock {
    fn drop(&mut self) {
        // SAFETY: the calling thread holds the mutex: it took it in `acquire` and `Self` is not
        // `Send`. The handle closes after.
        let released = unsafe { ReleaseMutex(self.mutex.as_raw_handle()) };
        debug_assert_ne!(released, 0, "ReleaseMutex: {}", io::Error::last_os_error());
    }
}

/// Makes sure this process has the namespace open: creates it, or opens it when another process has
/// it. A namespace that goes away between the failed create and the open (its last handle closed)
/// is created again; each round is another process's change, so there is no count to bound.
fn open_namespace(descriptor: &Descriptor) -> Result<()> {
    let mut open = NAMESPACE_OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    if *open {
        return Ok(());
    }
    let attributes = descriptor.attributes();
    let boundary = Boundary::administrators()?;
    let alias = wide_null(NAMESPACE);
    loop {
        // SAFETY: `attributes` points at `descriptor`, live for the call; `boundary` is a boundary
        // descriptor and `alias` is NUL-terminated. The handle is not closed: see
        // `NAMESPACE_OPEN`.
        if !unsafe { CreatePrivateNamespaceW(&attributes, boundary.0, alias.as_ptr()) }.is_null() {
            break;
        }
        // SAFETY: read straight after the call, before anything else can change it.
        let created = unsafe { GetLastError() };
        if created != ERROR_ALREADY_EXISTS {
            return Err(Error::os(
                format!("create the namespace of the install lock `{NAME}`"),
                io::Error::from_raw_os_error(created as i32),
            ));
        }
        // SAFETY: as above; the handle is not closed either.
        if !unsafe { OpenPrivateNamespaceW(boundary.0, alias.as_ptr()) }.is_null() {
            break;
        }
        // SAFETY: as above.
        let opened = unsafe { GetLastError() };
        if opened != ERROR_PATH_NOT_FOUND {
            return Err(Error::os(
                format!("open the namespace of the install lock `{NAME}`"),
                io::Error::from_raw_os_error(opened as i32),
            ));
        }
    }
    *open = true;
    Ok(())
}

/// A boundary descriptor that holds the Administrators SID, deleted on drop.
struct Boundary(HANDLE);

impl Boundary {
    fn administrators() -> Result<Self> {
        let name = wide_null(NAMESPACE);
        // SAFETY: `name` is NUL-terminated.
        let mut handle = unsafe { CreateBoundaryDescriptorW(name.as_ptr(), 0) };
        if handle.is_null() {
            return Err(Error::os(
                format!("describe the boundary of the install lock `{NAME}`"),
                io::Error::last_os_error(),
            ));
        }
        let sid = administrators();
        // SAFETY: `handle` is a boundary descriptor, which the call may replace; `sid` is a valid SID
        // that the call copies.
        if unsafe { AddSIDToBoundaryDescriptor(&mut handle, sid.as_ptr() as _) } == 0 {
            let e = io::Error::last_os_error();
            // SAFETY: `handle` is a boundary descriptor, deleted once.
            unsafe { DeleteBoundaryDescriptor(handle) };
            return Err(Error::os(
                format!("add Administrators to the boundary of the install lock `{NAME}`"),
                e,
            ));
        }
        Ok(Self(handle))
    }
}

impl Drop for Boundary {
    fn drop(&mut self) {
        // SAFETY: a boundary descriptor, deleted once.
        unsafe { DeleteBoundaryDescriptor(self.0) };
    }
}
