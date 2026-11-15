//! What the kernel says of the process a launch is, read through `libproc`: where it runs from, whether
//! it is stopped, and when it started. `run` tells the launch its `debug` held from every other by these
//! (see [`super::run`]), not by anything `launchctl print` says, which lags the launch.
//!
//! Every read answers `None` for a process that is not there: no such pid, or one that has exited and is
//! not yet reaped. Any other failure is an error, a refused read included.

use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;

/// A process that is alive.
pub(super) struct Process {
    /// Whether the kernel has it stopped (`SSTOP`): a held launch is, from its exec until it is continued.
    /// Read through `proc_pidinfo`, not `sysctl`, whose `p_stat` was seen reading running on a held launch.
    pub(super) stopped: bool,
    /// When it started, in `mach_absolute_time` units: the same clock as [`now`].
    pub(super) started: u64,
}

/// `mach_absolute_time`: the clock a process's start is stamped with.
pub(super) fn now() -> u64 {
    // libc deprecates it in favor of the `mach2` crate, which would be one more direct dependency for this
    // one call; the symbol is the same.
    #[allow(deprecated)]
    // SAFETY: takes no arguments and reads no memory of ours.
    unsafe {
        libc::mach_absolute_time()
    }
}

/// The path of the executable of `pid`, or `None` if there is no such process. A launch runs `xpcproxy`
/// until it execs into the job's program.
pub(super) fn path_of(pid: u32) -> io::Result<Option<PathBuf>> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is valid for the `buffer.len()` bytes `proc_pidpath` may write.
    let length = unsafe { libc::proc_pidpath(pid as libc::c_int, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return none_if_gone(io::Error::last_os_error());
    }
    buffer.truncate(length as usize);
    Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(buffer))))
}

/// Whether `pid` is stopped, and when it started; `None` if there is no such process, or it has exited.
pub(super) fn read(pid: u32) -> io::Result<Option<Process>> {
    let pid = pid as libc::c_int;
    // SAFETY: an all-zero `proc_bsdinfo` is a valid place to read one into.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is valid for `size` bytes, which is what `PROC_PIDTBSDINFO` writes.
    let written = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if written <= 0 {
        return none_if_gone(io::Error::last_os_error());
    }
    if written != size {
        return Err(io::Error::other(format!(
            "proc_pidinfo wrote {written} bytes of {size} for process {pid}"
        )));
    }
    if info.pbi_status == libc::SZOMB {
        return Ok(None);
    }

    // SAFETY: an all-zero `rusage_info_v0` is a valid place to read one into.
    let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a `rusage_info_v0`, the flavor named, and `proc_pid_rusage` writes only that.
    let read = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut usage).cast()) };
    if read != 0 {
        return none_if_gone(io::Error::last_os_error());
    }
    Ok(Some(Process {
        stopped: info.pbi_status == libc::SSTOP,
        started: usage.ri_proc_start_abstime,
    }))
}

/// `None` for `ESRCH`, which a process that is gone answers; any other failure is the error.
fn none_if_gone<T>(error: io::Error) -> io::Result<Option<T>> {
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(test)]
#[path = "proc_tests.rs"]
mod proc_tests;
