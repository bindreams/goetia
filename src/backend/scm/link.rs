//! Whether an open file is a link: another name for something else.

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle as _;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo, GetFileInformationByHandleEx,
};

/// `IsReparseTagNameSurrogate`: whether a reparse `tag` says the file is another name for something else
/// (a mount point, a symlink), which is bit 29. Any other tag is data on the file itself, which goetia has no
/// reason to refuse.
pub(crate) const fn is_name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

/// Whether the file `file` is open on is a link ([`is_name_surrogate`]). It must have been opened as
/// [`reparse_tag`] asks.
pub(crate) fn is_link(file: &File) -> io::Result<bool> {
    Ok(reparse_tag(file)?.is_some_and(is_name_surrogate))
}

/// The reparse tag of the file `file` is open on, or `None` when it is not a reparse point. `file` must
/// have been opened for `FILE_READ_ATTRIBUTES` and with `FILE_FLAG_OPEN_REPARSE_POINT`: a handle that
/// followed a link is on what the link names, which says nothing of the link.
fn reparse_tag(file: &File) -> io::Result<Option<u32>> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is `size_of::<FILE_ATTRIBUTE_TAG_INFO>()` writable bytes; the handle is live.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag))
}

#[cfg(test)]
#[path = "link_tests.rs"]
mod link_tests;
