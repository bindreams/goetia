use std::fs::File;
#[cfg(target_os = "linux")]
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
