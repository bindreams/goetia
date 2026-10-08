//! The ways to ask "may this account do X": root itself, (c') per-thread identity, (d) an exec'd
//! helper, (f) fork without exec, and the static-groups control (s).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::sys::{self, Pw};

#[derive(Clone, Debug, Serialize)]
pub struct Target {
    pub need: String,
    pub bits: i32,
    pub path: PathBuf,
}

/// `errno` (0 = granted), or the mechanism could not produce a verdict at all.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Errno(i32),
    MechanismFailed(String),
}

#[derive(Debug, Serialize)]
pub struct MechResult {
    pub mech: &'static str,
    pub verdicts: Vec<Verdict>,
    /// Groups the mechanism's credential carried, where observable.
    pub groups: Option<Vec<u32>>,
}

pub fn root(targets: &[Target]) -> MechResult {
    MechResult {
        mech: "root",
        verdicts: targets.iter().map(|t| Verdict::Errno(sys::access(&t.path, t.bits))).collect(),
        groups: Some(sys::getgroups()),
    }
}

/// (c'): `pthread_setugid_np`, then `initgroups` on the thread (per-thread credential per
/// `kern_prot.c` setgroups1's notes), then `access`.
pub fn thread(acct: &Pw, targets: &[Target]) -> MechResult {
    let name = sys::cstr(&acct.name);
    let (uid, gid) = (acct.uid, acct.gid);
    let targets = targets.to_vec();
    let out = std::thread::spawn(move || {
        if unsafe { sys::pthread_setugid_np(uid, gid) } != 0 {
            let e = sys::errno();
            return (vec![Verdict::MechanismFailed(format!("setugid errno {e}")); targets.len()], None);
        }
        let verdicts_groups = if unsafe { libc::initgroups(name.as_ptr(), gid as libc::c_int) } != 0 {
            let e = sys::errno();
            (vec![Verdict::MechanismFailed(format!("initgroups errno {e}")); targets.len()], None)
        } else {
            let groups = sys::getgroups();
            let v = targets.iter().map(|t| Verdict::Errno(sys::access(&t.path, t.bits))).collect();
            (v, Some(groups))
        };
        unsafe { sys::pthread_setugid_np(sys::KAUTH_UID_NONE, sys::KAUTH_GID_NONE) };
        verdicts_groups
    })
    .join()
    .expect("probe thread panicked");
    MechResult {
        mech: "c_thread",
        verdicts: out.0,
        groups: out.1,
    }
}

/// (d): the probe binary re-exec'd as `access --as`; stdout must be exactly `verdict <errno>\n`.
pub fn helper(bin: &Path, acct: &Pw, targets: &[Target]) -> MechResult {
    let verdicts = targets
        .iter()
        .map(|t| {
            let out = Command::new(bin)
                .args(["access", "--as", &acct.name, &acct.uid.to_string(), &acct.gid.to_string()])
                .arg(t.bits.to_string())
                .arg(&t.path)
                .output();
            match out {
                Err(e) => Verdict::MechanismFailed(format!("spawn: {e}")),
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout);
                    match s.strip_prefix("verdict ").and_then(|r| r.strip_suffix('\n')).map(str::parse::<i32>) {
                        Some(Ok(e)) if o.status.success() => Verdict::Errno(e),
                        _ => Verdict::MechanismFailed(format!(
                            "status {:?} stdout {s:?} stderr {:?}",
                            o.status,
                            String::from_utf8_lossy(&o.stderr)
                        )),
                    }
                }
            }
        })
        .collect();
    MechResult {
        mech: "d_helper",
        verdicts,
        groups: None,
    }
}

/// The `access --as` subcommand body: what launchd does to become the account, then `access`.
pub fn helper_main(name: &str, uid: u32, gid: u32, bits: i32, path: &Path) -> i32 {
    let c = sys::cstr(name);
    unsafe {
        if libc::setgid(gid) != 0 {
            println!("credfail setgid {}", sys::errno());
            return 1;
        }
        if libc::initgroups(c.as_ptr(), gid as libc::c_int) != 0 {
            println!("credfail initgroups {}", sys::errno());
            return 1;
        }
        if libc::setuid(uid) != 0 {
            println!("credfail setuid {}", sys::errno());
            return 1;
        }
    }
    println!("verdict {}", sys::access(path, bits));
    0
}

/// How a forked child sets its groups before `setuid`.
#[derive(Clone, Copy)]
enum Groups {
    /// (f): the raw `initgroups` syscall with gmuid = uid (memberd stays on).
    InitgroupsSyscall,
    /// (s): `setgroups` (sets CRF_NOMEMBERD: static list only).
    Setgroups,
}

/// Fork without exec; the child makes only async-signal-safe calls and reports over a pipe.
fn forked(mech: &'static str, how: Groups, acct: &Pw, targets: &[Target]) -> MechResult {
    let list = sys::getgrouplist(&acct.name, acct.gid, 16);
    let paths: Vec<_> = targets.iter().map(|t| sys::cpath(&t.path)).collect();
    let verdicts = targets
        .iter()
        .zip(&paths)
        .map(|(t, p)| {
            let mut fds = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Verdict::MechanismFailed(format!("pipe {}", sys::errno()));
            }
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Verdict::MechanismFailed(format!("fork {}", sys::errno()));
            }
            if pid == 0 {
                unsafe {
                    libc::close(fds[0]);
                    let report = |v: i32| {
                        libc::write(fds[1], (&v as *const i32).cast(), 4);
                        libc::_exit(0);
                    };
                    if libc::setgid(acct.gid) != 0 {
                        report(-1000 - sys::errno());
                    }
                    let r = match how {
                        Groups::InitgroupsSyscall => libc::syscall(
                            sys::SYS_INITGROUPS,
                            list.len() as libc::c_uint,
                            list.as_ptr(),
                            acct.uid as libc::c_int,
                        ),
                        Groups::Setgroups => libc::setgroups(list.len() as libc::c_int, list.as_ptr()),
                    };
                    if r != 0 {
                        report(-2000 - sys::errno());
                    }
                    if libc::setuid(acct.uid) != 0 {
                        report(-3000 - sys::errno());
                    }
                    let e = if libc::access(p.as_ptr(), t.bits) == 0 { 0 } else { sys::errno() };
                    report(e);
                }
            }
            unsafe { libc::close(fds[1]) };
            let mut f = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fds[0]) };
            let mut b = Vec::new();
            let _ = f.read_to_end(&mut b);
            let mut st = 0;
            unsafe { libc::waitpid(pid, &mut st, 0) };
            match <[u8; 4]>::try_from(b.as_slice()).map(i32::from_ne_bytes) {
                Ok(v) if v >= 0 => Verdict::Errno(v),
                Ok(v) => Verdict::MechanismFailed(format!("child credential step {v}")),
                Err(_) => Verdict::MechanismFailed(format!("child wrote {} bytes, status {}", b.len(), sys::decode_status(st))),
            }
        })
        .collect();
    MechResult {
        mech,
        verdicts,
        groups: Some(list),
    }
}

pub fn fork_initgroups(acct: &Pw, targets: &[Target]) -> MechResult {
    forked("f_fork", Groups::InitgroupsSyscall, acct, targets)
}

pub fn static_control(acct: &Pw, targets: &[Target]) -> MechResult {
    forked("s_static", Groups::Setgroups, acct, targets)
}
