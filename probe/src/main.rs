//! Throwaway measurement probe: what launchd does with a daemon's cwd/log paths, and which
//! "as the account" mechanism agrees with it. Never merged. Runs only on a throwaway CI runner.

mod candidates;
mod fixtures;
mod lifecycle;
mod sys;

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let run_id = std::env::var("PROBE_RUN").unwrap_or_else(|_| "local".into());
    let code = match args.get(1).map(String::as_str) {
        Some("row") => lifecycle::run_row(&args[2], &run_id),
        Some("access") if args.get(2).map(String::as_str) == Some("--as") => candidates::helper_main(
            &args[3],
            args[4].parse().expect("uid"),
            args[5].parse().expect("gid"),
            args[6].parse().expect("bits"),
            Path::new(&args[7]),
        ),
        Some("selftest") => selftest(),
        Some("exfat") => exfat(&run_id),
        Some("ids") => {
            println!("{}", fixtures::ALL.join(" "));
            0
        }
        _ => {
            eprintln!("usage: launchd-probe row <id> | access --as <name> <uid> <gid> <bits> <path> | selftest | exfat | ids");
            2
        }
    };
    std::process::exit(code);
}

// selftest ----------------------------------------------------------------------------------------

fn kq_add(kq: &OwnedFd, pid: i32, fflags: u32) -> i64 {
    let ch = libc::kevent {
        ident: pid as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_RECEIPT,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let mut out: libc::kevent = unsafe { std::mem::zeroed() };
    let n = unsafe { libc::kevent(kq.as_raw_fd(), &ch, 1, &mut out, 1, std::ptr::null()) };
    if n != 1 || out.flags & libc::EV_ERROR == 0 {
        return -1;
    }
    out.data as i64
}

fn kq_next(kq: &OwnedFd) -> libc::kevent {
    let mut out: libc::kevent = unsafe { std::mem::zeroed() };
    loop {
        let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut out, 1, std::ptr::null()) };
        if n == 1 {
            return out;
        }
        assert!(n == 0 || sys::errno() == libc::EINTR, "kevent errno {}", sys::errno());
    }
}

fn new_kq() -> OwnedFd {
    let fd = unsafe { libc::kqueue() };
    assert!(fd >= 0);
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn pipe() -> (i32, i32) {
    let mut p = [0; 2];
    assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
    (p[0], p[1])
}

/// The lifecycle's kernel assumptions, checked on this runner before any row is trusted.
fn selftest() -> i32 {
    fs::create_dir_all("results").expect("results");
    let mut out = json!({});
    let mut ok = true;

    // A: a zombie child. Attach must fail with ESRCH, or attach and still deliver NOTE_EXIT.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::_exit(7);
        }
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let w = libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT);
        let kq = new_kq();
        let r = kq_add(&kq, pid, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS);
        let mut a = json!({ "waitid": w, "receipt": r });
        if r == 0 {
            let e = kq_next(&kq);
            a["event_fflags"] = json!(format!("{:#x}", e.fflags));
            a["status"] = json!(sys::decode_status(e.data as i32));
            ok &= e.fflags & libc::NOTE_EXIT != 0 && e.data as i32 >> 8 == 7;
        } else {
            ok &= r == libc::ESRCH as i64;
        }
        let mut st = 0;
        libc::waitpid(pid, &mut st, 0);
        out["zombie_child"] = a;
    }

    // B: a non-child (reparented grandchild) blocked on a pipe; attach, release, NOTE_EXIT.
    unsafe {
        let (gate_r, gate_w) = pipe();
        let (pid_r, pid_w) = pipe();
        let child = libc::fork();
        if child == 0 {
            let gc = libc::fork();
            if gc == 0 {
                libc::close(gate_w);
                let mut b = [0u8; 1];
                while libc::read(gate_r, b.as_mut_ptr().cast(), 1) > 0 {}
                libc::_exit(3);
            }
            libc::write(pid_w, (&gc as *const i32).cast(), 4);
            libc::_exit(0);
        }
        libc::close(gate_r);
        libc::close(pid_w);
        let mut gc = 0i32;
        libc::read(pid_r, (&mut gc as *mut i32).cast(), 4);
        let mut st = 0;
        libc::waitpid(child, &mut st, 0);
        let kq = new_kq();
        let r = kq_add(&kq, gc, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS);
        libc::close(gate_w);
        let mut b = json!({ "receipt": r });
        if r == 0 {
            let e = kq_next(&kq);
            b["status"] = json!(sys::decode_status(e.data as i32));
            ok &= e.fflags & libc::NOTE_EXIT != 0 && e.data as i32 >> 8 == 3;
        } else {
            ok = false;
        }
        out["non_child"] = b;
    }

    // C (M2): the knote survives exec into a blocking helper: NOTE_EXEC, then NOTE_EXIT.
    unsafe {
        let (gate_r, gate_w) = pipe();
        let (in_r, in_w) = pipe();
        let cat = sys::cstr("/bin/cat");
        let argv = [cat.as_ptr(), std::ptr::null()];
        let devnull = sys::cstr("/dev/null");
        let pid = libc::fork();
        if pid == 0 {
            libc::close(gate_w);
            libc::close(in_w);
            let mut b = [0u8; 1];
            libc::read(gate_r, b.as_mut_ptr().cast(), 1);
            libc::dup2(in_r, 0);
            let n = libc::open(devnull.as_ptr(), libc::O_WRONLY);
            libc::dup2(n, 1);
            libc::execv(cat.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
        libc::close(gate_r);
        libc::close(in_r);
        let kq = new_kq();
        let r = kq_add(&kq, pid, libc::NOTE_EXIT | libc::NOTE_EXEC | libc::NOTE_EXITSTATUS);
        libc::write(gate_w, b"x".as_ptr().cast(), 1);
        let mut seen = Vec::new();
        let mut status = None;
        if r == 0 {
            loop {
                let e = kq_next(&kq);
                if e.fflags & libc::NOTE_EXEC != 0 {
                    seen.push("exec");
                    libc::close(in_w); // cat sees EOF and exits 0
                }
                if e.fflags & libc::NOTE_EXIT != 0 {
                    seen.push("exit");
                    status = Some(sys::decode_status(e.data as i32));
                    break;
                }
            }
        }
        let mut st = 0;
        libc::waitpid(pid, &mut st, 0);
        ok &= r == 0 && seen == ["exec", "exit"] && status.as_deref() == Some("exited:0");
        out["exec"] = json!({ "receipt": r, "events": seen, "status": status });
    }

    out["ok"] = json!(ok);
    let text = out.to_string();
    fs::write("results/selftest.json", &text).expect("write");
    println!("RESULT {text}");
    if ok { 0 } else { 1 }
}

// exfat (D9) ---------------------------------------------------------------------------------------

fn rename_excl_to_absent(dir: &Path) -> i32 {
    let x = dir.join("x");
    let z = dir.join("z");
    let _ = fs::create_dir(&x);
    let (cx, cz) = (sys::cpath(&x), sys::cpath(&z));
    let r = unsafe { libc::renameatx_np(libc::AT_FDCWD, cx.as_ptr(), libc::AT_FDCWD, cz.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 { 0 } else { sys::errno() }
}

fn vol_caps(path: &Path) -> Value {
    #[repr(C)]
    struct Buf {
        len: u32,
        caps: libc::vol_capabilities_attr_t,
    }
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.volattr = libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES;
    let mut buf: Buf = unsafe { std::mem::zeroed() };
    let c = sys::cpath(path);
    let r = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&mut al as *mut libc::attrlist).cast(),
            (&mut buf as *mut Buf).cast(),
            std::mem::size_of::<Buf>(),
            0,
        )
    };
    if r != 0 {
        return json!({ "errno": sys::errno() });
    }
    let i = libc::VOL_CAPABILITIES_INTERFACES;
    let bit = libc::VOL_CAP_INT_RENAME_EXCL;
    json!({
        "interfaces_valid": format!("{:#x}", buf.caps.valid[i]),
        "interfaces_caps": format!("{:#x}", buf.caps.capabilities[i]),
        "rename_excl_valid": buf.caps.valid[i] & bit != 0,
        "rename_excl_supported": buf.caps.capabilities[i] & bit != 0,
    })
}

fn exfat(run_id: &str) -> i32 {
    fs::create_dir_all("results").expect("results");
    let tmp = std::env::var("RUNNER_TEMP").expect("RUNNER_TEMP passed through sudo");
    let dmg = format!("{tmp}/goetia-probe-{run_id}.dmg");
    let mnt = format!("/Volumes/goetia-probe-{run_id}");
    let mut out = json!({});
    let mut ok = true;

    let base = lifecycle::base(run_id).join("exfat-baseline");
    fs::create_dir_all(&base).expect("baseline dir");
    out["apfs_tmp"] = json!({ "rename_excl_to_absent": rename_excl_to_absent(&base), "caps": vol_caps(&base) });
    let _ = fs::remove_dir_all(&base);

    let create = fixtures::run("hdiutil", &["create", "-size", "64m", "-fs", "ExFAT", "-volname", "GPROBE", &dmg]);
    out["create"] = json!(create);
    let attach = fixtures::run("hdiutil", &["attach", "-nobrowse", "-mountpoint", &mnt, &dmg]);
    out["attach"] = json!(attach);
    if attach.0 {
        let m = Path::new(&mnt);
        out["exfat"] = json!({ "rename_excl_to_absent": rename_excl_to_absent(m), "caps": vol_caps(m) });
        let detach = fixtures::run("hdiutil", &["detach", &mnt]);
        out["detach"] = json!(detach);
        if !detach.0 {
            ok = false; // stop: never delete anything while a mount may remain
        }
    } else {
        ok = false;
    }
    if ok {
        let _ = fs::remove_file(&dmg);
    }
    out["ok"] = json!(ok);
    let text = out.to_string();
    fs::write("results/exfat.json", &text).expect("write");
    println!("RESULT {text}");
    if ok { 0 } else { 1 }
}
