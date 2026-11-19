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
