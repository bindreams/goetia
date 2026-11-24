use std::os::windows::fs::OpenOptionsExt as _;

use super::*;

/// Only a tag that names a surrogate of another name (a mount point, a symlink) is a link: a file with any
/// other tag is data. The tag values are Windows's (`ntifs.h`).
#[skuld::test]
fn only_a_name_surrogate_tag_is_a_link() {
    for (tag, link) in [
        (0xA000_0003u32, true), // IO_REPARSE_TAG_MOUNT_POINT
        (0xA000_000C, true),    // IO_REPARSE_TAG_SYMLINK
        (0x8000_0007, false),   // IO_REPARSE_TAG_HSM2
        (0x9000_001A, false),   // IO_REPARSE_TAG_CLOUD_6
        (0x8000_0023, false),   // IO_REPARSE_TAG_AF_UNIX
        (0x0000_0000, false),
    ] {
        assert_eq!(is_name_surrogate(tag), link, "{tag:#x}");
    }
}

/// The names a file has are counted by the file system: a hard link is another name for it.
#[skuld::test]
fn a_hard_link_is_another_name_for_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let name = dir.path().join("a");
    std::fs::write(&name, b"x").unwrap();
    let open = |path: &std::path::Path| std::fs::File::open(path).unwrap();

    let before = standard_info(&open(&name)).unwrap();
    assert_eq!(
        (before.NumberOfLinks, before.DeletePending, before.Directory),
        (1, false, false)
    );

    let other = dir.path().join("b");
    std::fs::hard_link(&name, &other).unwrap();
    assert_eq!(standard_info(&open(&name)).unwrap().NumberOfLinks, 2);
    let folder = std::fs::OpenOptions::new()
        .access_mode(windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir.path())
        .unwrap();
    assert!(standard_info(&folder).unwrap().Directory);
}
