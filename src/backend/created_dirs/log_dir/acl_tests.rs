use std::fs::File;
use std::io;
use std::os::fd::AsFd as _;

use super::*;

#[skuld::test]
fn a_directory_with_no_acl_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let open = File::open(dir.path()).unwrap();

    assert!(!has_acl(open.as_fd()).unwrap());
}

/// An NFSv4 file system keeps its ACL in `system.nfs4_acl`, which no file system a test can make has.
#[cfg(target_os = "linux")]
#[skuld::test]
fn a_directory_with_an_nfsv4_acl_has_one() {
    let found = has_any(ACL_ATTRIBUTES, |name| Ok(name == c"system.nfs4_acl")).unwrap();

    assert!(found, "system.nfs4_acl is among {ACL_ATTRIBUTES:?}");
}

#[cfg(target_os = "linux")]
#[skuld::test]
fn an_error_other_than_no_acl_is_the_reads() {
    let e = has_any(ACL_ATTRIBUTES, |_| Err(io::Error::from_raw_os_error(libc::EIO))).unwrap_err();

    assert_eq!(e.raw_os_error(), Some(libc::EIO));
}

#[cfg(target_os = "macos")]
#[skuld::test]
fn a_directory_with_an_extended_acl_has_one() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow add_file"])
        .arg(dir.path())
        .status()
        .expect("run chmod");
    assert!(status.success(), "chmod +a: {status}");
    let open = File::open(dir.path()).unwrap();

    assert!(has_acl(open.as_fd()).unwrap());
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, acl_type: libc::c_int) -> libc::c_int;
}

/// An ACL with no entries lets no one anything: it is none.
#[cfg(target_os = "macos")]
#[skuld::test]
fn a_directory_whose_acl_has_no_entries_has_none() {
    use std::os::fd::AsRawFd as _;
    let dir = tempfile::tempdir().unwrap();
    let open = File::open(dir.path()).unwrap();
    // SAFETY: the ACL is freed once, and the descriptor is open for the call.
    unsafe {
        let acl = acl_init(0);
        assert!(!acl.is_null(), "acl_init: {}", io::Error::last_os_error());
        let set = acl_set_fd_np(open.as_raw_fd(), acl, ACL_TYPE_EXTENDED);
        acl_free(acl);
        assert_eq!(set, 0, "acl_set_fd_np: {}", io::Error::last_os_error());
    }

    assert!(!has_acl(open.as_fd()).unwrap());
}

#[cfg(target_os = "macos")]
#[skuld::test]
fn a_file_system_that_has_no_acls_has_none_and_another_error_is_the_reads() {
    assert!(!no_acl_or_error(io::Error::from_raw_os_error(libc::ENOENT)).unwrap());
    assert!(!no_acl_or_error(io::Error::from_raw_os_error(libc::ENOTSUP)).unwrap());
    assert!(!no_acl_or_error(io::Error::from_raw_os_error(libc::EOPNOTSUPP)).unwrap());
    let e = no_acl_or_error(io::Error::from_raw_os_error(libc::EIO)).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EIO));
}
