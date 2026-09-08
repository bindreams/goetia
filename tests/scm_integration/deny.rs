//! Denying an *elevated* reader access to one object, so a test can observe
//! what goetia does when a read it needs does not complete.
//!
//! An explicit `Deny` ACE stops an elevated reader: administrators do not
//! bypass one. Measured on a real Windows host before this was written — an
//! `HKLM` key given a `Deny`/`ReadKey` ACE for `Everyone`, read back from the
//! same elevated session that set it, was blocked. CI runs every test binary
//! elevated, which is exactly the condition that probe simulated.
//!
//! Raw `windows-sys` FFI, sanctioned here the same way `common.rs`'s SID lookup
//! and `fixture.rs`'s SCM dispatch already are: neither `windows-service` nor
//! `winreg` exposes any security-descriptor surface.

use std::os::windows::ffi::OsStrExt as _;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_OBJECT_TYPE, SE_REGISTRY_KEY, SE_SERVICE,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR};

use crate::support;

// SDDL ================================================================================================================
//
// A DACL is walked in the order its ACEs are written, so a `Deny` ACE placed
// first is decisive for every trustee it names — `Everyone` (`WD` as a
// trustee) covers the elevated token too. `SetNamedSecurityInfoW` replaces the
// *whole* explicit DACL, so each string below has to restate the accesses that
// must survive: without them the object would be left with nothing granted at
// all, and `ServiceGuard`'s own cleanup could no longer delete it.

/// Every service access, as `sc sdshow` spells it: the four standard rights
/// (`SD`/`RC`/`WD`/`WO` — delete, read-control, write-DAC, write-owner) plus
/// the nine service-specific ones.
const SERVICE_ALL: &str = "CCDCLCSWRPWPDTLOCRSDRCWDWO";

/// Administrators and `LocalSystem` keep full access under both of a pair, so
/// the deny never locks out the two things that still have to happen:
/// `SetNamedSecurityInfoW` needs `WRITE_DAC` to write the second string, and
/// SCM — which runs as `LocalSystem` — needs `DELETE` to remove the service the
/// test installed.
fn service_dacl(deny: Option<&str>) -> String {
    let deny = deny.map(|rights| format!("(D;;{rights};;;WD)")).unwrap_or_default();
    format!("D:{deny}(A;;{SERVICE_ALL};;;BA)(A;;{SERVICE_ALL};;;SY)")
}

/// The registry twin of [`service_dacl`]. `KA` (`KEY_ALL_ACCESS`) subsumes
/// `DELETE` and `WRITE_DAC`; `KR` (`KEY_READ`) does not, which is why denying
/// reads still leaves the key rewritable and still leaves it deletable with the
/// service.
fn key_dacl(deny: Option<&str>) -> String {
    let deny = deny.map(|rights| format!("(D;;{rights};;;WD)")).unwrap_or_default();
    format!("D:{deny}(A;;KA;;;BA)(A;;KA;;;SY)")
}

// SetNamedSecurityInfoW ===============================================================================================

fn wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Replace `object`'s explicit DACL with the one `sddl` describes.
///
/// Returns the failure rather than panicking: [`Denied`]'s `Drop` calls this
/// too, and a panic there during an unwind aborts the process, taking the
/// original assertion message with it.
fn set_dacl(object: &str, object_type: SE_OBJECT_TYPE, sddl: &str) -> Result<(), String> {
    let wide_sddl = wide_null(sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide_sddl` is a valid null-terminated UTF-16 string that
    // outlives the call, and `descriptor` is a valid out-param. On success it
    // owns a `LocalAlloc` block that must be freed with `LocalFree`.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!("parse SDDL `{sddl}`: {}", std::io::Error::last_os_error()));
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut present: windows_sys::core::BOOL = 0;
    let mut defaulted: windows_sys::core::BOOL = 0;
    // SAFETY: `descriptor` is the security descriptor just parsed; the three
    // out-params are valid lvalues. `dacl` points *into* `descriptor` and stays
    // valid until the `LocalFree` below.
    let ok = unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    let read = if ok == 0 {
        Err(format!(
            "read the DACL out of `{sddl}`: {}",
            std::io::Error::last_os_error()
        ))
    } else if present == 0 {
        Err(format!("`{sddl}` carries no DACL"))
    } else {
        Ok(())
    };

    let status = read.map(|()| {
        let wide_object = wide_null(object);
        // SAFETY: `wide_object` is a valid null-terminated UTF-16 string;
        // `dacl` points into the still-live `descriptor`; every other pointer
        // is null, so `DACL_SECURITY_INFORMATION` is the only thing written.
        unsafe {
            SetNamedSecurityInfoW(
                wide_object.as_ptr(),
                object_type,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        }
    });

    // SAFETY: `descriptor` was allocated by
    // `ConvertStringSecurityDescriptorToSecurityDescriptorW` and is freed
    // exactly once, after the last read through `dacl` above.
    unsafe {
        LocalFree(descriptor);
    }

    match status? {
        ERROR_SUCCESS => Ok(()),
        code => Err(format!(
            "set the DACL of `{object}` to `{sddl}`: {}",
            std::io::Error::from_raw_os_error(code as i32)
        )),
    }
}

// Denied ==============================================================================================================

/// One object whose DACL denies an access for as long as this value lives.
///
/// **`Drop` does not restore the original DACL.** It writes a fresh one
/// granting Administrators and `LocalSystem` full access and denying nothing —
/// the same string the deny was built from, minus the deny ACE. The original is
/// never captured, and capturing it would not help: `GetNamedSecurityInfoW`
/// returns the *effective* DACL, inherited ACEs included, so writing that back
/// as an explicit one converts inheritance into explicit ACEs and calls the
/// result a restore.
///
/// Sufficient here, and only here, because every caller deletes the object
/// moments later: both live under a `goetia-test-<random>` service that
/// `ServiceGuard` removes at the end of the same test, so all the written DACL
/// has to do is stay out of that cleanup's way. A caller that needs the object
/// to survive needs a real save-and-restore, which this is not.
///
/// Writing *something* rather than leaving the deny in place: a `Drop` that
/// reported nothing would surface only as a straggler in `services.msc` long
/// after the run that left it — the discipline `ServiceGuard` itself follows,
/// which is also why a failure here is logged rather than panicked.
pub struct Denied {
    object: String,
    object_type: SE_OBJECT_TYPE,
    on_drop: String,
}

impl Denied {
    /// Deny `KEY_READ` on `Services\<id>\Parameters` — the key carrying the
    /// metadata blob, and the only proof of ownership a `type: managed`
    /// service has.
    pub fn parameters(id: &str) -> Self {
        let object = format!(r"MACHINE\{}\{id}\Parameters", support::SCM_SERVICES_KEY);
        Self::seal(object, SE_REGISTRY_KEY, key_dacl(Some("KR")), key_dacl(None))
    }

    /// Deny `SERVICE_QUERY_CONFIG` (`CC`) and `SERVICE_QUERY_STATUS` (`LC`) on
    /// the service object itself — exactly the pair `manager::open_existing`
    /// asks for, and a boundary the `Parameters` deny cannot reach, since
    /// every verb opens the service object first.
    pub fn service_query(id: &str) -> Self {
        Self::seal(
            id.to_string(),
            SE_SERVICE,
            service_dacl(Some("CCLC")),
            service_dacl(None),
        )
    }

    fn seal(object: String, object_type: SE_OBJECT_TYPE, deny: String, on_drop: String) -> Self {
        set_dacl(&object, object_type, &deny).unwrap_or_else(|e| panic!("{e}"));
        Self {
            object,
            object_type,
            on_drop,
        }
    }
}

impl Drop for Denied {
    fn drop(&mut self) {
        if let Err(e) = set_dacl(&self.object, self.object_type, &self.on_drop) {
            eprintln!("Denied[{}]: cleanup failed: {e}", self.object);
        }
    }
}
